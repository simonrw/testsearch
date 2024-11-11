use std::{
    borrow::Cow,
    collections::HashMap,
    fmt, fs, io,
    path::{Path, PathBuf},
    sync::Arc,
    thread,
};

use clap::Parser;
use color_eyre::eyre::{self, Context};
use ignore::WalkBuilder;
use rayon::iter::{IntoParallelIterator, ParallelIterator};
use serde::{Deserialize, Serialize};
use skim::prelude::*;
use tracing_subscriber::EnvFilter;
use tree_sitter::Node;

#[derive(Debug, Parser)]
struct Args {
    root: Vec<PathBuf>,
    #[clap(short, long)]
    no_fizzy_selection: bool,
    #[clap(short, long)]
    rerun_last: bool,
}

#[derive(Serialize, Deserialize, Default)]
struct PersistedState {
    /// Persisted state of the last run test
    ///
    /// The HashMap is a mapping from directory to test name
    last_test: HashMap<PathBuf, String>,
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

    fn last_test(&self) -> Option<&String> {
        let here = std::env::current_dir().ok()?;
        self.persisted.last_test.get(&here)
    }

    fn set_last_test(&mut self, last_test: impl Into<String>) -> eyre::Result<()> {
        let here = std::env::current_dir().wrap_err("locating current directory")?;
        self.persisted.last_test.insert(here, last_test.into());
        self.flush().wrap_err("flushing cache changes to disk")?;
        Ok(())
    }

    fn flush(&self) -> eyre::Result<()> {
        let mut outfile =
            std::fs::File::create(&self.cache_file).wrap_err("creating cache file")?;
        serde_json::to_writer(&mut outfile, &self.persisted)
            .wrap_err("writing state to cache file")?;
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

    if args.rerun_last {
        match state.last_test() {
            Some(name) => {
                println!("{name}");
                return Ok(());
            }
            None => eyre::bail!("No last test recorded"),
        }
    }

    let (files_tx, files_rx) = unbounded();

    let mut file_handles = Vec::new();
    for path in args.root {
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
    tracing::debug!(n = files.len(), "finished collecting files");

    let (test_tx, test_rx) = unbounded();
    files
        .into_par_iter()
        .for_each_with(test_tx, |sender, path| {
            if let Err(e) = parse_file(sender, &path) {
                tracing::warn!(error = %e, path = %path.display(), "error parsing file");
            }
        });

    if args.no_fizzy_selection {
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
                "expression_statement" | "comment" => continue,
                kind => todo!("{kind}"),
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
