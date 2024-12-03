use std::thread;
use std::{io, path::PathBuf};

use clap::{Parser, Subcommand};
use color_eyre::eyre::{self, Context};
use rayon::iter::{IntoParallelIterator, ParallelIterator};
use skim::prelude::*;
use testsearch_core::{
    current_dir, find_test_files, parse_file, CacheClearOption, State, TestHistoryEntry,
};
use tracing_subscriber::EnvFilter;

#[derive(Subcommand, Debug, Clone)]
enum Command {
    /// Search for a test or rerun the last test
    Search {
        /// Paths to search for tests
        root: Vec<PathBuf>,

        /// Print results rather than using fuzzy find
        #[arg(short, long)]
        no_fizzy_selection: bool,
    },
    /// Rerun a previous test
    Rerun {
        /// Path to re-run tests from
        root: Option<PathBuf>,

        /// Automatically pick the most recent test
        #[arg(short, long)]
        last: bool,
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
        } => {
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
                    if let Some(tests) = state.persisted.history(&current_dir) {
                        serde_json::to_string_pretty(&tests)
                            .wrap_err("serializing state to JSON")?
                    } else {
                        String::new()
                    }
                };
                println!("{contents}");
                Ok(())
            }
        },
        Command::Rerun { root, last } => {
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
                                Ok(())
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

                        let test = selected_items[0].text();
                        println!("{}", test);
                        Ok(())
                    }
                }
                None => eyre::bail!("No test history found for path {}", search_root.display()),
            }
        }
    }
}
