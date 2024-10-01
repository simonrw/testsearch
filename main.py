#!/usr/bin/env python


from dataclasses import dataclass
import tree_sitter_python as tspython
from tree_sitter import Language, Node, Parser
import argparse
import subprocess as sp
from concurrent.futures import ThreadPoolExecutor, as_completed

PY_LANGUAGE = Language(tspython.language())
arg_parser = argparse.ArgumentParser()
arg_parser.add_argument("method", choices=["serial", "map", "apply"])
args = arg_parser.parse_args()

parser = Parser(PY_LANGUAGE)


def find_test_files(root: str) -> list[str]:
    cmd = [
        "fd",
        "-0",
        r"test[a-z0-9_]+\.py$",
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
                case _:
                    continue

    def handle_class_definition(self, node: Node):
        class_name_node = node.child(1)
        assert class_name_node is not None
        assert class_name_node.type == "identifier"
        assert class_name_node.text is not None
        class_name = class_name_node.text.decode()

        for child in node.children[2:]:
            match child.type:
                case "block":
                    self.handle_class_block(child, class_name=class_name)

    def handle_class_block(self, node: Node, class_name: str):
        for child in node.children:
            match child.type:
                case "decorated_definition":
                    self.handle_decorated_definition(child, class_name=class_name)

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


files = [
    filename.strip()
    for filename in find_test_files("/Users/simon/work/localstack/localstack/tests")
    if filename.strip()
] + [
    filename.strip()
    for filename in find_test_files(
        "/Users/simon/work/localstack/localstack-ext/localstack-pro-core/tests"
    )
    if filename.strip()
]

match args.method:
    case "serial":
        for file in files:
            for test in extract_tests(file):
                print(test.for_pytest())
    case "map":
        with ThreadPoolExecutor() as pool:
            batches = pool.map(extract_tests, files)
            for batch in batches:
                for test in batch:
                    print(test.for_pytest())
    case "apply":
        with ThreadPoolExecutor() as pool:
            futures = []
            for file in files:
                fut = pool.submit(extract_tests, file)
                futures.append(fut)

            for fut in as_completed(futures):
                for test in fut.result():
                    print(test.for_pytest())
