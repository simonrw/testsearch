use std::{
    borrow::Cow,
    collections::HashMap,
    fmt, fs, io,
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::{ExitCode, Stdio},
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
use regex::Regex;
use rustyline::DefaultEditor;
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
#[command(version)]
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
    Repl {
        /// Command template to execute tests (use {} as placeholder for test path)
        #[arg(value_name = "COMMAND")]
        command: String,
    },
    /// Search for tests containing specific function calls
    Grep {
        /// Regular expression pattern to search for in test function bodies
        pattern: String,

        /// Command template to execute matching tests (use {} as placeholder for test path)
        #[arg(long)]
        run: Option<String>,

        #[command(flatten)]
        search_args: SearchArgs,
    },
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
) -> eyre::Result<Option<String>> {
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

        return Ok(None); // No specific test selected in print mode
    }

    // perform fuzzy search
    let search_result = skim::Skim::run_with(&skim_options, Some(test_rx))
        .ok_or_else(|| eyre::eyre!("performing interactive search"))?;

    if search_result.is_abort {
        tracing::info!("no tests selected");
        return Ok(None);
    }

    let selected_items = search_result.selected_items;

    if selected_items.is_empty() {
        tracing::warn!("no tests selected");
        return Ok(None);
    }

    if selected_items.len() > 1 {
        panic!("programming error: multiple tests selected");
    }

    let test = selected_items[0].text();
    state.set_last_test(test.clone())?;
    println!("{test}");

    Ok(Some(test.to_string()))
}

fn perform_grep_search(
    pattern: String,
    args: SearchArgs,
    run_command: Option<String>,
) -> eyre::Result<()> {
    // Compile the regex pattern
    let regex =
        Regex::new(&pattern).wrap_err_with(|| format!("compiling regex pattern: {}", pattern))?;

    let (files_tx, files_rx) = unbounded();

    // Use provided roots or default to current working directory
    let search_roots = if args.root.is_empty() {
        let here = current_dir()?;
        vec![here]
    } else {
        args.root
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
            if let Err(e) = parse_file_with_regex(sender, &path, Some(&regex)) {
                tracing::warn!(error = %e, path = %path.display(), "error parsing file for grep");
            }
        });

    let matching_tests: Vec<_> = test_rx.into_iter().collect();

    if matching_tests.is_empty() {
        println!("No tests found matching pattern: {}", pattern);
        return Ok(());
    }

    // Print all matching test node IDs
    for test in &matching_tests {
        println!("{}", test.text());
    }

    // If run command is provided, execute the tests
    if let Some(command_template) = run_command {
        println!("\nExecuting matching tests...\n");

        for test in matching_tests {
            let test_path = test.text();
            if let Err(e) = execute_test_command(&command_template, &test_path) {
                eprintln!("❌ Execution failed for {}: {}", test_path, e);
            }
        }
    }

    Ok(())
}

fn get_colour() -> eyre::Result<Option<&'static str>> {
    use dark_light::Mode::*;
    match dark_light::detect().context("detecting colour from system")? {
        Dark => Ok(Some("dark")),
        Light => Ok(Some("light")),
        _ => Ok(None),
    }
}

fn run_repl(
    mut state: State,
    skim_options: SkimOptions,
    command_template: String,
) -> eyre::Result<ExitCode> {
    println!("🔍 testsearch REPL mode");
    println!("Command template: {}", command_template);
    println!(
        "Press 'f' to find and execute test, 'e' to edit command before execution, 'r' to rerun last test, 'ctrl-c', 'q', or 'esc' to exit"
    );
    println!();

    enable_raw_mode().context("enabling raw terminal mode")?;

    let result = repl_loop(&mut state, &skim_options, &command_template);

    // Always ensure we disable raw mode, even on error
    let _ = disable_raw_mode();

    result
}

fn execute_test_command(command_template: &str, test_path: &str) -> eyre::Result<()> {
    // Validate that the command template contains the placeholder
    if !command_template.contains("{}") {
        eyre::bail!("Command template must contain '{{}}' placeholder for test path");
    }

    // Replace the placeholder with the actual test path
    let command = command_template.replace("{}", test_path);

    print!("Executing: {}\r\n", command);
    io::stdout().flush()?;

    // Parse the command into program and arguments
    let parts: Vec<&str> = command.split_whitespace().collect();
    if parts.is_empty() {
        eyre::bail!("Empty command");
    }

    let program = parts[0];
    let args = &parts[1..];

    // Start the process with piped I/O for real-time output
    let mut child = std::process::Command::new(program)
        .args(args)
        .env("FORCE_COLOR", "1")
        .env("PY_COLORS", "1")
        .env("PYTEST_DISABLE_PLUGIN_AUTOLOAD", "0")
        .env("TERM", "xterm-256color")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("spawning test command")?;

    // Get handles to stdout and stderr
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| eyre::eyre!("failed to get stdout handle"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| eyre::eyre!("failed to get stderr handle"))?;

    // Create readers for both streams
    let stdout_reader = BufReader::new(stdout);
    let stderr_reader = BufReader::new(stderr);

    // Stream stdout in real-time
    let stdout_handle = thread::spawn(move || {
        for line in stdout_reader.lines() {
            match line {
                Ok(line) => {
                    print!("{}\r\n", line);
                    let _ = io::stdout().flush();
                }
                Err(_) => break,
            }
        }
    });

    // Stream stderr in real-time
    let stderr_handle = thread::spawn(move || {
        for line in stderr_reader.lines() {
            match line {
                Ok(line) => {
                    print!("{}\r\n", line);
                    let _ = io::stdout().flush();
                }
                Err(_) => break,
            }
        }
    });

    // Wait for the process to complete
    let status = child
        .wait()
        .context("waiting for test command to complete")?;

    // Wait for output threads to finish
    let _ = stdout_handle.join();
    let _ = stderr_handle.join();

    if status.success() {
        print!("✅ Test execution completed successfully\r\n");
    } else {
        print!(
            "❌ Test execution failed (exit code: {})\r\n",
            status.code().unwrap_or(-1)
        );
    }

    io::stdout().flush()?;
    Ok(())
}

fn edit_command_for_test(command_template: &str, test_path: &str) -> eyre::Result<String> {
    // Create the default command by filling in the template
    let default_command = command_template.replace("{}", test_path);

    // Create a rustyline editor
    let mut rl = DefaultEditor::new().context("creating rustyline editor")?;

    // Read the edited command from user
    let prompt = "Edit command: ";
    let readline = rl.readline_with_initial(prompt, (&default_command, ""));

    match readline {
        Ok(edited_command) => {
            if !edited_command.trim().is_empty() {
                rl.add_history_entry(&edited_command)
                    .context("adding to history")?;
                Ok(edited_command)
            } else {
                // If user entered empty command, use the default
                Ok(default_command)
            }
        }
        Err(rustyline::error::ReadlineError::Interrupted) => {
            // User pressed Ctrl-C, cancel the operation
            eyre::bail!("Command editing cancelled")
        }
        Err(rustyline::error::ReadlineError::Eof) => {
            // User pressed Ctrl-D, use the default command
            Ok(default_command)
        }
        Err(err) => {
            eyre::bail!("Error reading command: {}", err)
        }
    }
}

fn execute_raw_command(command: &str) -> eyre::Result<()> {
    print!("Executing: {}\r\n", command);
    io::stdout().flush()?;

    // Parse the command into program and arguments
    let parts: Vec<&str> = command.split_whitespace().collect();
    if parts.is_empty() {
        eyre::bail!("Empty command");
    }

    let program = parts[0];
    let args = &parts[1..];

    // Start the process with piped I/O for real-time output
    let mut child = std::process::Command::new(program)
        .args(args)
        .env("FORCE_COLOR", "1")
        .env("PY_COLORS", "1")
        .env("PYTEST_DISABLE_PLUGIN_AUTOLOAD", "0")
        .env("TERM", "xterm-256color")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("spawning test command")?;

    // Get handles to stdout and stderr
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| eyre::eyre!("failed to get stdout handle"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| eyre::eyre!("failed to get stderr handle"))?;

    // Create readers for both streams
    let stdout_reader = BufReader::new(stdout);
    let stderr_reader = BufReader::new(stderr);

    // Stream stdout in real-time
    let stdout_handle = thread::spawn(move || {
        for line in stdout_reader.lines() {
            match line {
                Ok(line) => {
                    print!("{}\r\n", line);
                    let _ = io::stdout().flush();
                }
                Err(_) => break,
            }
        }
    });

    // Stream stderr in real-time
    let stderr_handle = thread::spawn(move || {
        for line in stderr_reader.lines() {
            match line {
                Ok(line) => {
                    print!("{}\r\n", line);
                    let _ = io::stdout().flush();
                }
                Err(_) => break,
            }
        }
    });

    // Wait for the process to complete
    let status = child
        .wait()
        .context("waiting for test command to complete")?;

    // Wait for output threads to finish
    let _ = stdout_handle.join();
    let _ = stderr_handle.join();

    if status.success() {
        print!("✅ Test execution completed successfully\r\n");
    } else {
        print!(
            "❌ Test execution failed (exit code: {})\r\n",
            status.code().unwrap_or(-1)
        );
    }

    io::stdout().flush()?;
    Ok(())
}

fn repl_loop(
    state: &mut State,
    skim_options: &SkimOptions,
    command_template: &str,
) -> eyre::Result<ExitCode> {
    let mut last_executed_test: Option<String> = None;
    loop {
        print!("testsearch> ");
        io::stdout().flush().context("flushing stdout")?;

        match event::read().context("reading terminal event")? {
            Event::Key(KeyEvent {
                code: KeyCode::Char('f'),
                modifiers: KeyModifiers::NONE,
                ..
            }) => {
                print!("f\r\n");
                print!("🔍 Finding and executing test...\r\n");
                io::stdout().flush()?;

                // Temporarily disable raw mode for skim
                disable_raw_mode().context("disabling raw mode for search")?;
                let search_result = perform_search(SearchArgs::default(), skim_options, state);

                match search_result {
                    Ok(Some(selected_test)) => {
                        print!("Selected test: {}\r\n", selected_test);

                        // Execute the test
                        match execute_test_command(command_template, &selected_test) {
                            Err(e) => {
                                print!("❌ Execution failed: {}\r\n", e);
                            }
                            _ => {
                                // Store the last executed test for rerun
                                last_executed_test = Some(selected_test);
                            }
                        }
                    }
                    Ok(None) => {
                        print!("❌ No test was selected\r\n");
                    }
                    Err(e) => {
                        print!("❌ Search failed: {}\r\n", e);
                    }
                }

                enable_raw_mode().context("re-enabling raw mode after search")?;
            }
            Event::Key(KeyEvent {
                code: KeyCode::Char('e'),
                modifiers: KeyModifiers::NONE,
                ..
            }) => {
                print!("e\r\n");
                print!("✏️ Finding test and editing command...\r\n");
                io::stdout().flush()?;

                // Temporarily disable raw mode for skim
                disable_raw_mode().context("disabling raw mode for search")?;
                let search_result = perform_search(SearchArgs::default(), skim_options, state);

                match search_result {
                    Ok(Some(selected_test)) => {
                        print!("Selected test: {}\r\n", selected_test);

                        // Edit the command for this test
                        match edit_command_for_test(command_template, &selected_test) {
                            Ok(edited_command) => {
                                print!("Edited command: {}\r\n", edited_command);

                                // Execute the edited command
                                match execute_raw_command(&edited_command) {
                                    Err(e) => {
                                        print!("❌ Execution failed: {}\r\n", e);
                                    }
                                    _ => {
                                        // Store the selected test (not the command) as the last executed test for rerun
                                        last_executed_test = Some(selected_test);
                                    }
                                }
                            }
                            Err(e) => {
                                print!("❌ Command editing failed: {}\r\n", e);
                            }
                        }
                    }
                    Ok(None) => {
                        print!("❌ No test was selected\r\n");
                    }
                    Err(e) => {
                        print!("❌ Search failed: {}\r\n", e);
                    }
                }

                enable_raw_mode().context("re-enabling raw mode after search")?;
            }
            Event::Key(KeyEvent {
                code: KeyCode::Char('r'),
                modifiers: KeyModifiers::NONE,
                ..
            })
            | Event::Key(KeyEvent {
                code: KeyCode::Enter,
                ..
            }) => {
                print!("r\r\n");
                print!("🔄 Rerunning last test...\r\n");
                io::stdout().flush()?;

                match &last_executed_test {
                    Some(test_path) => {
                        // Temporarily disable raw mode for test execution
                        disable_raw_mode().context("disabling raw mode for rerun")?;

                        print!("Rerunning: {}\r\n", test_path);
                        if let Err(e) = execute_test_command(command_template, test_path) {
                            print!("❌ Rerun failed: {}\r\n", e);
                        }

                        enable_raw_mode().context("re-enabling raw mode after rerun")?;
                    }
                    None => {
                        print!(
                            "❌ No test has been executed yet. Press 'f' to find and run a test first.\r\n"
                        );
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
            })
            | Event::Key(KeyEvent {
                code: KeyCode::Char('q'),
                ..
            }) => {
                print!("\r\n");
                print!("👋 Goodbye!\r\n");
                return Ok(ExitCode::SUCCESS);
            }
            Event::Key(KeyEvent {
                code: KeyCode::Char(c),
                modifiers: KeyModifiers::NONE,
                ..
            }) => {
                print!("{}\r\n", c);
                print!(
                    "Unknown command '{}'. Press 'f' to find and execute, 'e' to edit command, 'r' to rerun, 'ctrl-c', 'q', or 'esc' to exit.\r\n",
                    c
                );
            }
            _ => {
                // Ignore other events
            }
        }
        print!("\r\n");
        io::stdout().flush()?;
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
                        eyre::bail!("No test history found for path {}", search_root.display())
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
        Some(Command::Search(args)) => {
            match perform_search(args, &skim_options, &mut state)? {
                Some(_) => Ok(ExitCode::SUCCESS),
                None => Ok(ExitCode::SUCCESS), // No test selected is not an error
            }
        }
        Some(Command::Grep {
            pattern,
            run,
            search_args,
        }) => {
            perform_grep_search(pattern, search_args, run)?;
            Ok(ExitCode::SUCCESS)
        }
        Some(Command::Repl { command }) => run_repl(state, skim_options, command),
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
            let search_args = args.search.unwrap_or_default();
            match perform_search(search_args, &skim_options, &mut state)? {
                Some(_) => Ok(ExitCode::SUCCESS),
                None => Ok(ExitCode::SUCCESS), // No test selected is not an error
            }
        }
        Some(Command::Completion { .. }) => unreachable!("handled above"),
    }
}

struct TestHistoryEntry {
    text: String,
}

impl SkimItem for TestHistoryEntry {
    fn text(&self) -> std::borrow::Cow<'_, str> {
        Cow::Borrowed(&self.text)
    }
}

struct Visitor<'s> {
    filename: &'s Path,
    sender: &'s mut skim::prelude::Sender<Arc<dyn SkimItem>>,
    bytes: Vec<u8>,
    regex: Option<&'s Regex>,
}

impl<'s> Visitor<'s> {
    pub fn new(
        filename: &'s Path,
        sender: &'s mut skim::prelude::Sender<Arc<dyn SkimItem>>,
        regex: Option<&'s Regex>,
    ) -> eyre::Result<Self> {
        let bytes = fs::read(filename).wrap_err("reading file")?;
        Ok(Self {
            filename,
            sender,
            bytes,
            regex,
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
                kind => todo!("{kind} for file {}", self.filename.display()),
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

        // If regex is provided, check if the function body matches the pattern
        if let Some(regex) = self.regex {
            let function_text = node.utf8_text(&bytes)
                .wrap_err("reading function body")?;
            
            if !regex.is_match(function_text) {
                return Ok(());
            }
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
    let mut visitor = Visitor::new(path, sender, None).wrap_err("creating visitor")?;
    visitor.visit().wrap_err("parsing file")?;
    Ok(())
}

fn parse_file_with_regex(
    sender: &mut skim::prelude::Sender<Arc<dyn SkimItem>>,
    path: &Path,
    regex: Option<&Regex>,
) -> eyre::Result<()> {
    let mut visitor = Visitor::new(path, sender, regex).wrap_err("creating visitor")?;
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
    fn text(&self) -> std::borrow::Cow<'_, str> {
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
