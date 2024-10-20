#!/usr/bin/env python

from __future__ import annotations
import json
import shutil
import argparse
from collections.abc import Generator, Iterable
from concurrent.futures import ThreadPoolExecutor, as_completed, ProcessPoolExecutor
import logging
from pathlib import Path
import subprocess as sp
import multiprocessing as mp

from dataclasses import dataclass
import sys
from typing import TypeVar
from tree_sitter import Language, Node, Parser
import tree_sitter_python as tspython
from iterfzf import iterfzf

logging.basicConfig(level=logging.DEBUG)
LOG = logging.getLogger(__name__)

PY_LANGUAGE = Language(tspython.language())

parser = Parser(PY_LANGUAGE)


def find_test_files(root: str) -> list[str]:
    cmd = [
        "fd",
        "-0",
        "--type",
        "f",
        r"test_[a-z0-9_]+\.py$",
        root,
    ]
    res = sp.check_output(cmd)
    return res.decode().split("\0")


@dataclass
class TestCase:
    name: str
    file: str

    class_name: str | None = None

    def for_pytest(self) -> str:
        if self.class_name:
            return "::".join(
                [
                    self.file,
                    self.class_name,
                    self.name,
                ]
            )
        else:
            return "::".join(
                [
                    self.file,
                    self.name,
                ]
            )


class Visitor:
    def __init__(self, filename: str):
        self.filename = filename
        self.tests: list[TestCase] = []

    def handle_function_definition(self, node: Node, class_name: str | None):
        identifier_node = node.child(1)
        assert identifier_node is not None
        assert identifier_node.text is not None
        identifier = identifier_node.text.decode()
        if not identifier.startswith("test_"):
            return

        self.tests.append(
            TestCase(name=identifier, class_name=class_name, file=self.filename)
        )

    def handle_decorated_definition(self, node: Node, class_name: str | None = None):
        for child in node.children:
            match child.type:
                case "function_definition":
                    self.handle_function_definition(child, class_name=class_name)
                case "class_definition":
                    # explicitly reset the class definition
                    self.handle_class_definition(child)
                case "decorator" | "comment":
                    continue
                case other:
                    LOG.debug(
                        "unhandled case in handle_decorated_definition: '%s'", other
                    )
                    continue

    def handle_class_definition(self, node: Node):
        class_name_node = node.child(1)
        assert class_name_node is not None
        assert class_name_node.type == "identifier"
        assert class_name_node.text is not None
        class_name = class_name_node.text.decode()

        if not class_name.startswith("Test"):
            return

        for child in node.children[2:]:
            match child.type:
                case "block":
                    self.handle_class_block(child, class_name=class_name)
                case ":" | "argument_list" | "comment":
                    continue
                case other:
                    LOG.debug("unhandled case in class definition: '%s'", other)

    def handle_class_block(self, node: Node, class_name: str):
        for child in node.children:
            match child.type:
                case "decorated_definition":
                    self.handle_decorated_definition(child, class_name=class_name)
                case "function_definition":
                    self.handle_function_definition(child, class_name=class_name)
                case "expression_statement" | "comment":
                    continue
                case other:
                    LOG.debug("unhandled type in handle_class_block: '%s'", other)

    def visit(self):
        with open(self.filename, "rb") as infile:
            tree = parser.parse(infile.read())
        root_node = tree.root_node

        for child in root_node.children:
            match child.type:
                case "decorated_definition":
                    self.handle_decorated_definition(child)
                case "class_definition":
                    self.handle_class_definition(child)
                case "function_definition":
                    self.handle_function_definition(child, class_name=None)
                case (
                    "import_statement"
                    | "import_from_statement"
                    | "expression_statement"
                    | "comment"
                    | "if_statement"
                    | "try_statement"
                    | "assert_statement"
                    | "future_import_statement"
                ):
                    continue
                case other:
                    raise NotImplementedError(
                        f"root parser: not handling {other} ({child.text.decode()}"
                    )


def extract_tests(path: str) -> list[TestCase]:
    visitor = Visitor(path)
    visitor.visit()
    return visitor.tests


# def select(

T = TypeVar("T")


def generate(max: int, output: mp.Queue[str]):
    import time

    for i in range(max):
        output.put(str(i))
        time.sleep(1)


def iterqueue(queue: mp.Queue[T]) -> Generator[T, None, None]:
    while True:
        value = queue.get()
        yield value


def iter_tests(
    files: Iterable[str], method: str, pool_type: str
) -> Generator[str, None, None]:
    if pool_type == "threads":
        PoolCls = ThreadPoolExecutor
    elif pool_type == "processes":
        PoolCls = ProcessPoolExecutor
    else:
        raise NotImplementedError(pool_type)

    match method:
        case "serial":
            for file in files:
                for test in extract_tests(file):
                    yield test.for_pytest()
        case "map":
            with PoolCls() as pool:
                batches = pool.map(extract_tests, files)
                for batch in batches:
                    for test in batch:
                        yield test.for_pytest()
        case "apply":
            with PoolCls() as pool:
                futures = []
                for file in files:
                    fut = pool.submit(extract_tests, file)
                    futures.append(fut)

                for fut in as_completed(futures):
                    for test in fut.result():
                        yield test.for_pytest()
        case other:
            raise NotImplementedError(f"Method '{other}' not implemented")


class State:
    state: dict[str, dict[str, str]]

    def __init__(self, root: Path):
        root.mkdir(exist_ok=True, parents=True)
        self.cache_file = root / "cache.json"
        self.working_dir = Path.cwd()

        if self.cache_file.is_file():
            with self.cache_file.open() as infile:
                self.state = json.load(infile)

            assert isinstance(self.state, dict)
        else:
            self.state = {"last_test": {}}
            with self.cache_file.open("w") as outfile:
                json.dump(self.state, outfile, indent=2)

    @property
    def last_test(self) -> str | None:
        return self.state["last_test"].get(self._working_dir_str(), None)

    @last_test.setter
    def last_test(self, last_test: str):
        self.state["last_test"][self._working_dir_str()] = last_test
        self._flush()

    def _flush(self):
        with self.cache_file.open("w") as outfile:
            json.dump(self.state, outfile, indent=2)

    def _working_dir_str(self) -> str:
        return str(self.working_dir)


def main():
    arg_parser = argparse.ArgumentParser()
    arg_parser.add_argument("root", nargs="*")
    arg_parser.add_argument(
        "-m", "--method", choices=["serial", "map", "apply"], default="map"
    )
    arg_parser.add_argument(
        "-p", "--pool", choices=["threads", "processes"], default="processes"
    )
    arg_parser.add_argument(
        "-r",
        "--rerun-last",
        action="store_true",
        default=False,
        help="Re-run previously selected test",
    )
    arg_parser.add_argument(
        "-n",
        "--no-fuzzy-selection",
        action="store_true",
        default=False,
        help="Disable selecting a single test with `fzf` and just print all found tests",
    )
    arg_parser.add_argument("-v", "--verbose", action="count", default=0)
    args = arg_parser.parse_args()

    if args.verbose == 1:
        LOG.setLevel(logging.INFO)
    elif args.verbose > 1:
        LOG.setLevel(logging.DEBUG)

    cache_root = Path.home().joinpath(".cache", "testsearch")
    state = State(cache_root)

    if args.rerun_last:
        if state.last_test is None:
            print("No last test recorded for this directory", file=sys.stderr)
            sys.exit(1)

        print(state.last_test)
        sys.exit(0)

    roots = args.root
    if not roots:
        # default to cwd
        roots = [
            str(Path.cwd()),
        ]

    files = []
    for root in roots:
        files.extend(
            filename.strip() for filename in find_test_files(root) if filename.strip()
        )

    if len(files) == 0:
        print("No test files found", file=sys.stderr)
        sys.exit(1)

    tests_iter = iter_tests(files, args.method, args.pool)
    if args.no_fuzzy_selection:
        for test in tests_iter:
            print(test)
        return

    def run_fzf(iter: Iterable[str]) -> str:
        fzf_executable = shutil.which("fzf")
        if fzf_executable is None:
            print("Could not find fzf on $PATH", file=sys.stderr)
            for test in iter:
                print(test)
            sys.exit(0)

        chosen_test = iterfzf(iter, executable=Path(fzf_executable))
        return chosen_test


    chosen_test = run_fzf(tests_iter)
    state.last_test = chosen_test
    print(chosen_test)


if __name__ == "__main__":
    main()
