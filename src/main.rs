use std::{
    borrow::Cow,
    collections::HashMap,
    fmt, fs, io,
    path::{Path, PathBuf},
    str::FromStr,
    sync::Arc,
    thread,
};

use clap::{Parser, Subcommand};
use color_eyre::eyre::{self, Context};
use ignore::WalkBuilder;
use rayon::iter::{IntoParallelIterator, ParallelIterator};
use serde::{Deserialize, Serialize};
use skim::prelude::*;
use tracing_subscriber::EnvFilter;
use tree_sitter::Node;

#[derive(Debug, Clone, Copy)]
enum CacheClearOption {
    Current,
    All,
}

impl FromStr for CacheClearOption {
    type Err = eyre::Report;

    fn from_str(s: &str) -> eyre::Result<Self> {
        match s {
            "current" => Ok(Self::Current),
            "all" => Ok(Self::All),
            other => eyre::bail!("invalid cache clear option: {other}"),
        }
    }
}

#[derive(Subcommand, Debug, Clone)]
enum Command {
    /// Search for a test or rerun the last test
    Search {
        /// Paths to search for tests
        root: Vec<PathBuf>,

        /// Print results rather than using fuzzy find
        #[arg(short, long)]
        no_fizzy_selection: bool,

        /// Re-run the last test
        #[arg(short, long)]
        rerun_last: bool,
    },
    /// View or manage state
    State {
        #[command(subcommand)]
        state_command: StateCommand,
    },
}

#[derive(Subcommand, Debug, Clone)]
enum StateCommand {
    /// Clear the state
    Clear {
        /// Clear the state for all directories
        #[arg(short, long)]
        all: bool,
    },
    /// Show the state contents
    Show {
        /// Show the last run test for every directory
        #[arg(short, long)]
        all: bool,
    },
}

#[derive(Debug, Parser)]
struct Args {
    #[command(subcommand)]
    command: Command,
}

fn current_dir() -> eyre::Result<PathBuf> {
    std::env::current_dir().wrap_err("locating current directory")
}

#[derive(Serialize, Deserialize, Default)]
struct PersistedState {
    /// Persisted history of all previous test runs
    #[serde(default)]
    test_history: Option<HashMap<PathBuf, Vec<String>>>,

    /// Persisted state of the last run test
    ///
    /// The HashMap is a mapping from directory to test name
    ///
    /// legacy option
    #[serde(default)]
    last_test: Option<HashMap<PathBuf, String>>,
}

impl PersistedState {
    fn history(&self, path: impl AsRef<Path>) -> Option<Vec<String>> {
        let path = path.as_ref();
        match (self.test_history.as_ref(), self.last_test.as_ref()) {
            (Some(h), _) => h.get(path).cloned(),
            (None, Some(_)) => panic!("we should never have last_test but not test_history"),
            _ => None,
        }
    }

    fn clear(&mut self, clear_option: CacheClearOption) -> eyre::Result<()> {
        match clear_option {
            CacheClearOption::Current => {
                let here = current_dir()?;
                if let Some(last_test) = self.last_test.as_mut() {
                    last_test.remove(&here);
                }
                if let Some(history) = self.test_history.as_mut() {
                    history.remove(&here);
                }
            }
            CacheClearOption::All => {
                *self = Self::default();
            }
        }
        Ok(())
    }

    fn migrate_settings(&mut self) -> eyre::Result<()> {
        if let Some(last_test) = self.last_test.take() {
            let mut test_history = HashMap::new();
            for (path, test) in last_test {
                test_history.insert(path, vec![test]);
            }
            self.test_history = Some(test_history);
        }

        Ok(())
    }
}

struct State {
    persisted: PersistedState,
    cache_file: PathBuf,
}

impl State {
    fn new(cache_root: impl AsRef<Path>) -> eyre::Result<Self> {
        let cache_root = cache_root.as_ref();
        std::fs::create_dir_all(cache_root).wrap_err("creating cache dir")?;
        let cache_file = cache_root.join("cache.json");

        let persisted_state = if cache_file.is_file() {
            let mut f = std::fs::File::open(&cache_file).wrap_err("opening existing cache file")?;
            serde_json::from_reader(&mut f).wrap_err("decoding existing cache file")?
        } else {
            PersistedState::default()
        };

        Ok(Self {
            persisted: persisted_state,
            cache_file,
        })
    }

    fn last_test(&self) -> Option<String> {
        let here = current_dir().ok()?;
        if let Some(history) = self.persisted.history(&here) {
            return history.last().cloned();
        }
        None
    }

    fn set_last_test(&mut self, last_test: impl Into<String>) -> eyre::Result<()> {
        let here = current_dir()?;
        // TODO
        self.persisted
            .last_test
            .get_or_insert_with(HashMap::new)
            .insert(here, last_test.into());
        self.flush().wrap_err("flushing cache changes to disk")?;
        Ok(())
    }

    fn clear(&mut self, clear_option: CacheClearOption) -> eyre::Result<()> {
        self.persisted
            .clear(clear_option)
            .wrap_err("clearing cache")?;
        self.flush()?;
        Ok(())
    }

    fn flush(&self) -> eyre::Result<()> {
        let mut outfile =
            std::fs::File::create(&self.cache_file).wrap_err("creating cache file")?;
        serde_json::to_writer(&mut outfile, &self.persisted)
            .wrap_err("writing state to cache file")?;
        Ok(())
    }

    fn migrate_settings(&mut self) -> eyre::Result<()> {
        self.persisted.migrate_settings()?;
        self.flush()?;
        Ok(())
    }
}

fn find_test_files(root: impl AsRef<Path>, chan: Sender<PathBuf>) -> eyre::Result<()> {
    WalkBuilder::new(root).build_parallel().run(|| {
        Box::new(|path| {
            if let Ok(entry) = path {
                let path = entry.path();
                if path.is_file()
                    && path
                        .file_name()
                        .and_then(|filename| filename.to_str())
                        .map(|filename| filename.starts_with("test_") && filename.ends_with(".py"))
                        .unwrap_or_default()
                {
                    let _ = chan.send(path.to_path_buf());
                }
            }
            ignore::WalkState::Continue
        })
    });
    Ok(())
}

fn main() -> eyre::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_writer(io::stderr)
        .init();
    color_eyre::install()?;

    let args = Args::parse();

    let cache_root = dirs::cache_dir()
        .map(|p| p.join("testsearch"))
        .ok_or_else(|| eyre::eyre!("locating cache dir on system"))?;
    tracing::debug!(cache_root = %cache_root.display(), "using cache root dir");
    let mut state = State::new(cache_root).wrap_err("constructing persistent state")?;
    state.migrate_settings().wrap_err("migrating settings")?;

    match args.command {
        Command::Search {
            root,
            no_fizzy_selection,
            rerun_last,
        } => {
            if rerun_last {
                match state.last_test() {
                    Some(name) => {
                        println!("{name}");
                        return Ok(());
                    }
                    None => eyre::bail!("No last test recorded"),
                }
            }

            let (files_tx, files_rx) = unbounded();

            // check that some files were passed, otherwise default to the current working directory
            let search_roots = if root.is_empty() {
                let here = current_dir()?;
                vec![here]
            } else {
                root
            };

            let mut file_handles = Vec::new();
            for path in search_roots {
                let span = tracing::debug_span!("", path = %path.display());
                let _guard = span.enter();

                tracing::debug!("listing files");

                let files_tx = files_tx.clone();
                file_handles.push(thread::spawn(move || {
                    if let Err(e) = find_test_files(&path, files_tx) {
                        tracing::warn!(error = %e, path = %path.display(), "finding test files");
                    }
                }));
            }
            drop(files_tx);

            for handle in file_handles {
                let _ = handle.join();
            }

            let files: Vec<_> = files_rx.into_iter().collect();
            if files.is_empty() {
                eyre::bail!("No compatible test files found");
            }

            tracing::debug!(n = files.len(), "finished collecting files");

            let (test_tx, test_rx) = unbounded();
            files
                .into_par_iter()
                .for_each_with(test_tx, |sender, path| {
                    if let Err(e) = parse_file(sender, &path) {
                        tracing::warn!(error = %e, path = %path.display(), "error parsing file");
                    }
                });

            if no_fizzy_selection {
                for test in test_rx {
                    println!("{}", test.text());
                }

                return Ok(());
            }

            // perform fuzzy search
            let skim_options = SkimOptionsBuilder::default()
                .multi(false)
                .build()
                .expect("invalid skim options");
            let search_result = skim::Skim::run_with(&skim_options, Some(test_rx))
                .ok_or_else(|| eyre::eyre!("performing interactive search"))?;

            if search_result.is_abort {
                tracing::info!("no tests selected");
                return Ok(());
            }

            let selected_items = search_result.selected_items;

            if selected_items.is_empty() {
                tracing::warn!("no tests selected");
                return Ok(());
            }

            if selected_items.len() > 1 {
                panic!("programming error: multiple tests selected");
            }

            let test = selected_items[0].text();
            state.set_last_test(test.clone())?;
            println!("{test}");

            Ok(())
        }
        Command::State { state_command } => match state_command {
            StateCommand::Clear { all } => {
                let cache_clear_option = if all {
                    CacheClearOption::All
                } else {
                    CacheClearOption::Current
                };
                state
                    .clear(cache_clear_option)
                    .wrap_err("clearing cache state")?;
                Ok(())
            }
            StateCommand::Show { all } => {
                let contents = if all {
                    serde_json::to_string_pretty(&state.persisted)
                        .wrap_err("serializing state to JSON")?
                } else {
                    let current_dir = current_dir().wrap_err("getting current directory")?;
                    if let Some(last_test) = state.persisted.last_test {
                        serde_json::to_string_pretty(&last_test.get(&current_dir))
                            .wrap_err("serializing state to JSON")?
                    } else {
                        String::new()
                    }
                };
                println!("{contents}");
                Ok(())
            }
        },
    }
}

struct Visitor<'s> {
    filename: &'s Path,
    sender: &'s mut skim::prelude::Sender<Arc<dyn SkimItem>>,
    bytes: Vec<u8>,
}

impl<'s> Visitor<'s> {
    pub fn new(
        filename: &'s Path,
        sender: &'s mut skim::prelude::Sender<Arc<dyn SkimItem>>,
    ) -> eyre::Result<Self> {
        let bytes = fs::read(filename).wrap_err("reading file")?;
        Ok(Self {
            filename,
            sender,
            bytes,
        })
    }

    fn visit(&mut self) -> eyre::Result<()> {
        let language = tree_sitter_python::LANGUAGE;
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&language.into())
            .wrap_err("configuring language")?;

        let tree = parser
            .parse(&self.bytes, None)
            .ok_or_else(|| eyre::eyre!("parsing file"))?;

        let root = tree.root_node();

        let mut cursor = root.walk();
        for child in root.children(&mut cursor) {
            match child.kind() {
                "decorated_definition" => self.handle_decorated_definition(child, None)?,
                "class_definition" => self.handle_class_definition(child)?,
                "function_definition" => self.handle_function_definition(child, None)?,
                "import_statement"
                | "import_from_statement"
                | "expression_statement"
                | "comment"
                | "if_statement"
                | "try_statement"
                | "assert_statement" => continue,
                kind => todo!("{kind}"),
            }
        }

        Ok(())
    }

    fn handle_decorated_definition(
        &mut self,
        node: Node,
        class_name: Option<String>,
    ) -> eyre::Result<()> {
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            match child.kind() {
                "function_definition" => {
                    self.handle_function_definition(child, class_name.clone())?
                }
                "class_definition" => self.handle_class_definition(child)?,
                "decorator" | "comment" => continue,
                kind => todo!("{kind}"),
            }
        }
        Ok(())
    }

    fn handle_class_definition(&mut self, node: Node) -> eyre::Result<()> {
        // TODO: nested classes?
        let Some(class_name_node) = node.child(1) else {
            eyre::bail!("no class name found");
        };

        if class_name_node.kind() != "identifier" {
            eyre::bail!(
                "invalid class name node type, expected 'identifier', got '{}'",
                class_name_node.kind()
            );
        }

        // TODO: can we prevent this clone?
        let bytes = self.bytes.clone();
        let class_name = class_name_node
            .utf8_text(&bytes)
            .wrap_err("reading class name")?;

        if !class_name.starts_with("Test") {
            // stop parsing
            return Ok(());
        }

        let mut cursor = node.walk();
        for child in node.children(&mut cursor).skip(2) {
            match child.kind() {
                "block" => self.handle_class_block(child, Some(class_name.to_string()))?,
                ":" | "argument_list" | "comment" => continue,
                kind => todo!("{kind}"),
            }
        }

        Ok(())
    }

    fn handle_class_block(&mut self, node: Node, class_name: Option<String>) -> eyre::Result<()> {
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            match child.kind() {
                "decorated_definition" => {
                    self.handle_decorated_definition(child, class_name.clone())?
                }
                "function_definition" => {
                    self.handle_function_definition(child, class_name.clone())?
                }
                "expression_statement" | "comment" | "pass_statement" => continue,
                kind => todo!("{kind} {}", node.parent().unwrap().utf8_text(&self.bytes)?),
            }
        }
        Ok(())
    }

    fn handle_function_definition(
        &mut self,
        node: Node,
        class_name: Option<String>,
    ) -> eyre::Result<()> {
        let Some(identifier_node) = node.child(1) else {
            eyre::bail!("no identifier node found");
        };

        let bytes = self.bytes.clone();
        let identifier = identifier_node
            .utf8_text(&bytes)
            .wrap_err("reading bytes for function identifier")?;

        if !identifier.starts_with("test_") {
            return Ok(());
        }

        self.emit(identifier, class_name)
            .wrap_err("sending test case")?;

        Ok(())
    }

    fn emit(
        &mut self,
        test_name: impl Into<String>,
        class_name: Option<String>,
    ) -> eyre::Result<()> {
        let test_case = TestCase {
            name: test_name.into(),
            file: self.filename.to_path_buf(),
            class_name,
        };

        let send_item = Arc::new(test_case);

        self.sender
            .send(send_item)
            .wrap_err("sending test case to closed receiver")?;

        Ok(())
    }
}

fn parse_file(
    sender: &mut skim::prelude::Sender<Arc<dyn SkimItem>>,
    path: &Path,
) -> eyre::Result<()> {
    let mut visitor = Visitor::new(path, sender).wrap_err("creating visitor")?;
    visitor.visit().wrap_err("parsing file")?;
    Ok(())
}

#[derive(Debug)]
struct TestCase {
    name: String,
    file: PathBuf,
    class_name: Option<String>,
}

impl skim::SkimItem for TestCase {
    fn text(&self) -> std::borrow::Cow<str> {
        Cow::Owned(format!("{self}"))
    }
}

impl fmt::Display for TestCase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(class_name) = &self.class_name {
            f.write_str(&format!(
                "{}::{}::{}",
                self.file.display(),
                class_name,
                self.name
            ))
        } else {
            f.write_str(&format!("{}::{}", self.file.display(), self.name))
        }
    }
}
