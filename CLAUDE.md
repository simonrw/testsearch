# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

This is `testsearch`, a Rust CLI tool for fuzzy searching and running Python tests. It parses Python test files using tree-sitter to extract test functions and provides an interactive fuzzy finder interface for test selection.

## Common Commands

### Building and Development
- **Build**: `cargo build`
- **Run**: `cargo run`
- **Test**: `cargo test`
- **Run with debug logs**: `RUST_LOG=debug cargo run`

### Tool Management
- Uses `mise` for tool management (see `mise.toml`)
- Install required tools: `mise install`

## Architecture

### Core Components

- **Main CLI Logic** (`src/main.rs`): Single-file application containing all functionality
- **State Management**: Persistent cache stored in system cache directory using JSON serialization
- **Test Discovery**: Multi-threaded file scanning using the `ignore` crate for .gitignore support
- **Test Parsing**: Tree-sitter based Python AST parsing to extract test functions and classes
- **Interactive Selection**: Skim-based fuzzy finder with system color theme detection

### Key Data Structures

- `PersistedState`: Handles test history and cache management
- `TestCase`: Represents a discovered test with file, class, and function information
- `Visitor`: Tree-sitter AST visitor for parsing Python test files

### Test Discovery Rules

- Scans for files matching `test_*.py` pattern
- Extracts functions starting with `test_`
- Supports test classes (names starting with "Test")
- Handles nested classes with `::` notation
- Supports decorated test functions

### Command Structure

- `search`: Find and select tests interactively (default command)
- `repl`: Start interactive REPL mode with single-key commands
- `rerun`: Re-run previous tests from history
- `state`: Manage persistent state (show/clear)
- `completion`: Generate shell completions

### REPL Mode

The `repl` command starts an interactive mode with single-keypress commands:
- `f`: Launch fuzzy finder to select a test
- `r`: Rerun the last selected test  
- `esc` or `ctrl-c`: Exit REPL gracefully

REPL mode uses crossterm for cross-platform terminal input handling and maintains state between operations.

## Development Notes

- Uses `tree-sitter-python` for parsing Python AST
- Parallel processing with `rayon` for file parsing
- Error handling with `color-eyre` and `tracing` for logging
- System integration with `dark-light` for theme detection
- State persisted to `~/.cache/testsearch/cache.json`

## Dependencies

Key external crates:
- `clap`: CLI argument parsing
- `skim`: Fuzzy finder interface
- `tree-sitter`: AST parsing
- `ignore`: Gitignore-aware file walking
- `rayon`: Parallel processing
- `serde`: JSON serialization