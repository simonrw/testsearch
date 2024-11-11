#!/usr/bin/env python

import argparse
import os
import sys


COMMANDS = [
    "cargo run --release -- {paths}",
    "{interpreter_path} ./testsearch.py --no-fuzzy-selection --method serial {paths}",
    "{interpreter_path} ./testsearch.py --no-fuzzy-selection --method map --pool threads {paths}",
    "{interpreter_path} ./testsearch.py --no-fuzzy-selection --method apply --pool threads {paths}",
    "{interpreter_path} ./testsearch.py --no-fuzzy-selection --method map --pool processes {paths}",
    "{interpreter_path} ./testsearch.py --no-fuzzy-selection --method apply --pool processes {paths}",
]


def run_benchmark(paths: list[str]):
    cmd = [
        "hyperfine",
        "--warmup",
        "5",
    ]
    for command in COMMANDS:
        cmd.append(command.format(paths=" ".join(paths), interpreter_path=sys.executable))

    os.execvp(cmd[0], cmd)


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("path", nargs="+")
    args = parser.parse_args()

    run_benchmark(args.path)
