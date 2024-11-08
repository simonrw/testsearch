from pathlib import Path
import sys
import json
import subprocess as sp
import tempfile
from typing import Any

import pytest

PROJECT_ROOT = Path(__file__).parent.parent
DEFAULTS_TEST_PATH = PROJECT_ROOT / "tests" / "localstack" / "tests"


def run_search(root: Path = DEFAULTS_TEST_PATH) -> list[Path]:
    script_path = PROJECT_ROOT / "testsearch.py"
    assert script_path.is_file()

    cmd = [sys.executable, script_path, "-n", str(root)]
    output = sp.check_output(cmd)

    results = []
    for path in output.decode().splitlines():
        results.append(Path(path.strip()))

    return results


Node = list | dict

def extract_tests(root: Node, tests: list[Any] | None):
    if tests is None:
        tests = []

    if isinstance(root, list):
        for every in root:
            extract_tests(every, tests)
    elif isinstance(root, dict):
        if "children" not in root:
            tests.append(0)
        else:
            for child in root["children"]:
                extract_tests(child, tests)
    else:
        raise TypeError(f"{root} is not a list or dict")


def pytest_collect_items(root: Path = DEFAULTS_TEST_PATH) -> list[Path]:
    with tempfile.NamedTemporaryFile() as tfile:
        cmd = [sys.executable, "-m", "pytest", "--collect-only", "--collect-format", "json", "--collect-output-file", str(tfile.name), str(root)]
        sp.run(cmd, check=True, stdout=sp.PIPE, stderr=sp.PIPE)

        tfile.seek(0)
        with open(tfile.name) as infile:
            data = json.load(infile)

    tests = []
    extract_tests(data, tests)
    return tests


@pytest.mark.parametrize("subdir", [
    "aws",
    "bootstrap",
    "cli",
    "integration",
    "unit",
])
def test_something(subdir: str):
    root = DEFAULTS_TEST_PATH / subdir
    pytest_tests = pytest_collect_items(root)
    testsearch_tests = run_search(root)

    assert len(pytest_tests) == len(testsearch_tests)
