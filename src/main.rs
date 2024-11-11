use std::{
    fmt::{self},
    fs, io,
    path::{Path, PathBuf},
    sync::mpsc,
    thread,
};

use clap::Parser;
use color_eyre::eyre::{self, Context};
use ignore::WalkBuilder;
use rayon::iter::{IntoParallelIterator, ParallelIterator};
use tracing_subscriber::EnvFilter;
use tree_sitter::Node;

#[derive(Debug, Parser)]
struct Args {
    root: Vec<PathBuf>,
}

fn find_test_files(root: impl AsRef<Path>, chan: mpsc::Sender<PathBuf>) -> eyre::Result<()> {
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
    // tracing_subscriber::fmt::init();
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_writer(io::stderr)
        .init();
    color_eyre::install()?;

    let args = Args::parse();

    let (files_tx, files_rx) = mpsc::channel();

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

    let (test_tx, test_rx) = mpsc::channel();
    files
        .into_par_iter()
        .for_each_with(test_tx, |sender, path| {
            if let Err(e) = parse_file(sender, &path) {
                tracing::warn!(error = %e, path = %path.display(), "error parsing file");
            }
        });

    for test in test_rx {
        println!("{test}");
    }

    Ok(())
}

struct Visitor<'s> {
    filename: &'s Path,
    sender: &'s mut mpsc::Sender<TestCase>,
    bytes: Vec<u8>,
}

impl<'s> Visitor<'s> {
    pub fn new(filename: &'s Path, sender: &'s mut mpsc::Sender<TestCase>) -> eyre::Result<Self> {
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

        self.sender
            .send(test_case)
            .wrap_err("sending test case to closed receiver")?;

        Ok(())
    }
}

fn parse_file(sender: &mut mpsc::Sender<TestCase>, path: &Path) -> eyre::Result<()> {
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
