use std::{
    borrow::Cow,
    collections::HashMap,
    fmt, fs, io,
    io::Write,
    path::{Path, PathBuf},
    process::ExitCode,
    str::FromStr,
    thread,
};

use clap::{CommandFactory, Parser, Subcommand};
use color_eyre::eyre::{self, Context};
use crossterm::{
    event::{self, Event, KeyCode, KeyEvent, KeyModifiers},
    terminal::{disable_raw_mode, enable_raw_mode},
};
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

#[derive(Debug, clap::Args, Clone, Default)]
struct SearchArgs {
    /// Paths to search for tests
    #[arg(short, long)]
    root: Vec<PathBuf>,

    /// Print results rather than using fuzzy find
    #[arg(short, long)]
    no_fuzzy_selection: bool,
}

#[derive(Subcommand, Debug, Clone)]
enum Command {
    // Search for a test or rerun the last test
    Search(SearchArgs),
    /// Rerun a previous test
    Rerun {
        /// Path to re-run tests from
        root: Option<PathBuf>,

        /// Automatically pick the most recent test
        #[arg(short, long)]
        last: bool,
    },
    /// Start interactive REPL mode
    Repl,
    /// View or manage state
    State {
        #[command(subcommand)]
        state_command: StateCommand,
    },
    /// Generate shell completions
    Completion {
        /// The shell to generate completions for
        #[arg(value_enum)]
        shell: clap_complete::Shell,
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
    #[command(flatten)]
    search: Option<SearchArgs>,

    #[command(subcommand)]
    command: Option<Command>,
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

fn perform_search(
    args: SearchArgs,
    skim_options: &SkimOptions,
    state: &mut State,
) -> eyre::Result<ExitCode> {
    let SearchArgs {
        root,
        no_fuzzy_selection,
    } = args;
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

    if no_fuzzy_selection {
        for test in test_rx {
            println!("{}", test.text());
        }

        return Ok(ExitCode::SUCCESS);
    }

    // perform fuzzy search
    let search_result = skim::Skim::run_with(&skim_options, Some(test_rx))
        .ok_or_else(|| eyre::eyre!("performing interactive search"))?;

    if search_result.is_abort {
        tracing::info!("no tests selected");
        return Ok(ExitCode::FAILURE);
    }

    let selected_items = search_result.selected_items;

    if selected_items.is_empty() {
        tracing::warn!("no tests selected");
        return Ok(ExitCode::SUCCESS);
    }

    if selected_items.len() > 1 {
        panic!("programming error: multiple tests selected");
    }

    let test = selected_items[0].text();
    state.set_last_test(test.clone())?;
    println!("{test}");

    Ok(ExitCode::SUCCESS)
}

fn get_colour() -> eyre::Result<Option<&'static str>> {
    use dark_light::Mode::*;
    match dark_light::detect().context("detecting colour from system")? {
        Dark => Ok(Some("dark")),
        Light => Ok(Some("light")),
        _ => Ok(None),
    }
}

fn run_repl(mut state: State, skim_options: SkimOptions) -> eyre::Result<ExitCode> {
    println!("🔍 testsearch REPL mode");
    println!("Press 'f' to find test, 'r' to rerun last test, 'esc' or 'ctrl-c' to exit");
    println!();

    enable_raw_mode().context("enabling raw terminal mode")?;

    let result = repl_loop(&mut state, &skim_options);

    // Always ensure we disable raw mode, even on error
    let _ = disable_raw_mode();

    result
}

fn repl_loop(state: &mut State, skim_options: &SkimOptions) -> eyre::Result<ExitCode> {
    loop {
        print!("testsearch> ");
        io::stdout().flush().context("flushing stdout")?;

        match event::read().context("reading terminal event")? {
            Event::Key(KeyEvent {
                code: KeyCode::Char('f'),
                modifiers: KeyModifiers::NONE,
                ..
            }) => {
                println!("f");
                println!("🔍 Finding tests...");
                
                // Perform search using existing logic
                match perform_search(SearchArgs::default(), skim_options, state) {
                    Ok(_) => println!("✅ Search completed"),
                    Err(e) => {
                        println!("❌ Search failed: {}", e);
                        continue;
                    }
                }
            }
            Event::Key(KeyEvent {
                code: KeyCode::Char('r'),
                modifiers: KeyModifiers::NONE,
                ..
            }) => {
                println!("r");
                println!("🔄 Rerunning last test...");
                
                // Rerun last test using existing logic
                match rerun_test(None, true, state, skim_options) {
                    Ok(_) => println!("✅ Rerun completed"),
                    Err(e) => {
                        println!("❌ Rerun failed: {}", e);
                        continue;
                    }
                }
            }
            Event::Key(KeyEvent {
                code: KeyCode::Esc, ..
            })
            | Event::Key(KeyEvent {
                code: KeyCode::Char('c'),
                modifiers: KeyModifiers::CONTROL,
                ..
            }) => {
                println!();
                println!("👋 Goodbye!");
                return Ok(ExitCode::SUCCESS);
            }
            Event::Key(KeyEvent {
                code: KeyCode::Char(c),
                modifiers: KeyModifiers::NONE,
                ..
            }) => {
                println!("{}", c);
                println!("Unknown command '{}'. Press 'f' to find, 'r' to rerun, 'esc' to exit.", c);
            }
            _ => {
                // Ignore other events
            }
        }
        println!();
    }
}

fn rerun_test(
    root: Option<PathBuf>,
    last: bool,
    state: &State,
    skim_options: &SkimOptions,
) -> eyre::Result<ExitCode> {
    // fetch the tests from the state using root as the key
    let search_root = if let Some(root) = root {
        root
    } else {
        current_dir()?
    };

    let history = state.persisted.history(search_root.clone());
    match history {
        Some(history) => {
            if last {
                // pick last test from history
                match history.last() {
                    Some(last_test) => {
                        println!("{}", last_test);
                        Ok(ExitCode::SUCCESS)
                    }
                    None => {
                        eyre::bail!(
                            "No test history found for path {}",
                            search_root.display()
                        )
                    }
                }
            } else {
                // perform fuzzy search through history
                let (test_tx, test_rx) = unbounded();
                for test in history {
                    let item: Arc<dyn SkimItem> = Arc::new(TestHistoryEntry { text: test });
                    test_tx.send(item)?;
                }

                let search_result = skim::Skim::run_with(skim_options, Some(test_rx))
                    .ok_or_else(|| eyre::eyre!("performing interactive search"))?;

                if search_result.is_abort {
                    tracing::info!("no tests selected");
                    return Ok(ExitCode::SUCCESS);
                }

                let selected_items = search_result.selected_items;
                if selected_items.is_empty() {
                    tracing::warn!("no tests selected");
                    return Ok(ExitCode::SUCCESS);
                }

                let test = selected_items[0].text();
                println!("{}", test);
                Ok(ExitCode::SUCCESS)
            }
        }
        None => eyre::bail!("No test history found for path {}", search_root.display()),
    }
}

fn main() -> eyre::Result<ExitCode> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_writer(io::stderr)
        .init();
    color_eyre::install()?;

    let args = Args::parse();
    // if we need to generate completions, do that early since we don't need to build the state/cache etc.
    // which fails if we build a nix pkackage
    if let Some(Command::Completion { shell }) = args.command {
        return generate_completions(shell);
    };

    let cache_root = dirs::cache_dir()
        .map(|p| p.join("testsearch"))
        .ok_or_else(|| eyre::eyre!("locating cache dir on system"))?;
    tracing::debug!(cache_root = %cache_root.display(), "using cache root dir");
    let mut state = State::new(cache_root).wrap_err("constructing persistent state")?;
    state.migrate_settings().wrap_err("migrating settings")?;

    let colour = get_colour().context("getting colour from system")?;
    let skim_options = SkimOptionsBuilder::default()
        .multi(false)
        .color(colour)
        .build()
        .expect("invalid skim options");

    match args.command {
        Some(Command::Search(args)) => perform_search(args, &skim_options, &mut state),
        Some(Command::Repl) => run_repl(state, skim_options),
        Some(Command::State { state_command }) => match state_command {
            StateCommand::Clear { all } => {
                let cache_clear_option = if all {
                    CacheClearOption::All
                } else {
                    CacheClearOption::Current
                };
                state
                    .clear(cache_clear_option)
                    .wrap_err("clearing cache state")?;
                Ok(ExitCode::SUCCESS)
            }
            StateCommand::Show { all } => {
                let contents = if all {
                    serde_json::to_string_pretty(&state.persisted)
                        .wrap_err("serializing state to JSON")?
                } else {
                    let current_dir = current_dir().wrap_err("getting current directory")?;
                    if let Some(tests) = state.persisted.history(&current_dir) {
                        serde_json::to_string_pretty(&tests)
                            .wrap_err("serializing state to JSON")?
                    } else {
                        String::new()
                    }
                };
                println!("{contents}");
                Ok(ExitCode::SUCCESS)
            }
        },
        Some(Command::Rerun { root, last }) => rerun_test(root, last, &state, &skim_options),
        None => {
            // Assume search command
            match args.search {
                Some(args) => perform_search(args, &skim_options, &mut state),
                None => perform_search(SearchArgs::default(), &skim_options, &mut state),
            }
        }
        Some(Command::Completion { .. }) => unreachable!("handled above"),
    }
}

struct TestHistoryEntry {
    text: String,
}

impl SkimItem for TestHistoryEntry {
    fn text(&self) -> std::borrow::Cow<str> {
        Cow::Borrowed(&self.text)
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
                "class_definition" => self.handle_class_definition(child, None)?,
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
                "class_definition" => self.handle_class_definition(child, None)?,
                "decorator" | "comment" => continue,
                kind => todo!("{kind}"),
            }
        }
        Ok(())
    }

    fn handle_class_definition(
        &mut self,
        node: Node,
        parent_class_name: Option<String>,
    ) -> eyre::Result<()> {
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
        let mut class_name = class_name_node
            .utf8_text(&bytes)
            .wrap_err("reading class name")?
            .to_string();

        if !class_name.starts_with("Test") {
            // stop parsing
            return Ok(());
        }

        if let Some(parent_class_name) = parent_class_name {
            if parent_class_name.starts_with("Test") {
                class_name = format!("{parent_class_name}::{class_name}");
            }
        }

        let mut cursor = node.walk();
        for child in node.children(&mut cursor).skip(2) {
            match child.kind() {
                "block" => self.handle_class_block(child, Some(class_name.clone()))?,
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
                "class_definition" => self.handle_class_definition(child, class_name.clone())?,
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

fn generate_completions(shell: clap_complete::Shell) -> eyre::Result<ExitCode> {
    let mut cmd = Args::command();
    let bin_name = cmd.get_name().to_string();
    clap_complete::generate(shell, &mut cmd, bin_name, &mut io::stdout());
    Ok(ExitCode::SUCCESS)
}
