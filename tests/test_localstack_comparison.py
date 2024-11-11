from pathlib import Path
import sys
import os
import json
import subprocess as sp
import tempfile
from typing import Any

import pytest

PROJECT_ROOT = Path(__file__).parent.parent
DEFAULTS_TEST_PATH = PROJECT_ROOT / "tests" / "localstack" / "tests"
BUILT_BINARY_PATH = Path(os.environ["CARGO_TARGET_DIR"]) / "release" / "testsearch"


@pytest.fixture(scope="session", autouse=True)
def build():
    sp.check_call(["cargo", "build", "--release"])


def run_search(root: Path = DEFAULTS_TEST_PATH) -> list[Path]:
    assert BUILT_BINARY_PATH.is_file()
    cmd = [BUILT_BINARY_PATH, "-n", str(root)]
    output = sp.check_output(cmd)

    results = []
    for path in output.decode().splitlines():
        results.append(Path(path.strip()))

    return results


Node = list | dict


def extract_tests(root: Node, tests: list[Any] | None = None, current_path: str = ""):
    if tests is None:
        tests = []

    if isinstance(root, list):
        for every in root:
            extract_tests(every, tests, current_path)
    elif isinstance(root, dict):
        if "children" not in root:
            if current_path:
                node_id = f"{current_path}::{root['title']}"
            else:
                node_id = root["title"]
            tests.append(node_id)
            current_path = ""
        else:
            if current_path:
                if root["type"] == "Class":
                    current_path = current_path + "::" + root["title"]
                else:
                    current_path = current_path + "/" + root["title"]
            else:
                current_path = root["title"]
            for child in root["children"]:
                extract_tests(child, tests, current_path)
    else:
        raise TypeError(f"{root} is not a list or dict")


def pytest_collect_items(root: Path = DEFAULTS_TEST_PATH) -> list[Path]:
    # root = /Users/simon/dev/testsearch/tests/localstack/tests/aws
    with tempfile.NamedTemporaryFile() as tfile:
        cmd = [
            sys.executable,
            "-m",
            "pytest",
            "--collect-only",
            "--collect-format",
            "json",
            "--collect-output-file",
            str(tfile.name),
            str(root),
        ]
        sp.run(cmd, check=True, stdout=sp.PIPE, stderr=sp.PIPE)

        tfile.seek(0)
        with open(tfile.name) as infile:
            data = json.load(infile)

    tests = []
    extract_tests(data, tests)
    # test path = localstack/tests/aws/scenario/loan_broker/test_loan_broker.py
    test_root = Path(__file__).parent
    out = []
    excluded_tests = set()
    for test in tests:
        if test.endswith("]"):
            non_parametrized_test = test.split("[")[0]
            if non_parametrized_test not in excluded_tests:
                out.append(Path(non_parametrized_test))
            excluded_tests.add(non_parametrized_test)
            continue

        full_path = test_root / test
        out.append(full_path)
    return out


@pytest.mark.parametrize(
    "input,expected",
    [
        ({"type": "Function", "title": "test_foo"}, ["test_foo"]),
        (
            {
                "type": "Class",
                "title": "TestClass",
                "children": [
                    {"type": "Function", "title": "test_foo"},
                ],
            },
            ["TestClass::test_foo"],
        ),
        (
            {
                "type": "Module",
                "title": "test_file.py",
                "children": [
                    {
                        "type": "Class",
                        "title": "TestClass",
                        "children": [
                            {"type": "Function", "title": "test_foo"},
                        ],
                    }
                ],
            },
            ["test_file.py::TestClass::test_foo"],
        ),
        (
            {
                "type": "Package",
                "title": "foo",
                "children": [
                    {
                        "type": "Module",
                        "title": "test_file.py",
                        "children": [
                            {
                                "type": "Class",
                                "title": "TestClass",
                                "children": [
                                    {"type": "Function", "title": "test_foo"},
                                ],
                            }
                        ],
                    },
                ],
            },
            ["foo/test_file.py::TestClass::test_foo"],
        ),
        (
            {
                "type": "Package",
                "title": "tests",
                "children": [
                    {
                        "type": "Package",
                        "title": "foo",
                        "children": [
                            {
                                "type": "Module",
                                "title": "test_file.py",
                                "children": [
                                    {
                                        "type": "Class",
                                        "title": "TestClass",
                                        "children": [
                                            {"type": "Function", "title": "test_foo"},
                                        ],
                                    }
                                ],
                            },
                        ],
                    }
                ],
            },
            ["tests/foo/test_file.py::TestClass::test_foo"],
        ),
        (
            {
                "type": "Dir",
                "title": "localstack",
                "children": [
                    {
                        "type": "Package",
                        "title": "tests",
                        "children": [
                            {
                                "type": "Package",
                                "title": "foo",
                                "children": [
                                    {
                                        "type": "Module",
                                        "title": "test_file.py",
                                        "children": [
                                            {
                                                "type": "Class",
                                                "title": "TestClass",
                                                "children": [
                                                    {
                                                        "type": "Function",
                                                        "title": "test_foo",
                                                    },
                                                ],
                                            }
                                        ],
                                    },
                                ],
                            }
                        ],
                    }
                ],
            },
            ["localstack/tests/foo/test_file.py::TestClass::test_foo"],
        ),
    ],
)
def test_extract_tests(input: Node, expected: list[str]):
    tests = []
    extract_tests(input, tests)
    assert tests == expected


@pytest.mark.parametrize(
    "subdir",
    [
        "aws",
        "bootstrap",
        "cli",
        "integration",
        "unit",
    ],
)
def test_something(subdir: str):
    root = DEFAULTS_TEST_PATH / subdir
    pytest_tests = sorted(pytest_collect_items(root))
    testsearch_tests = sorted(run_search(root))

    assert testsearch_tests == pytest_tests
