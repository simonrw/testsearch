#!/usr/bin/env python

import argparse
import os
import subprocess as sp


COMMANDS = [
    "cargo run --release -- {paths}",
    "./main.py --method serial {paths}",
    "./main.py --method map --pool threads {paths}",
    "./main.py --method apply --pool threads {paths}",
    "./main.py --method map --pool processes {paths}",
    "./main.py --method apply --pool processes {paths}",
]


def run_benchmark(paths):
    cmd = [
        "hyperfine",
        "--warmup",
        "3",
    ]
    for command in COMMANDS:
        cmd.append(command.format(paths=" ".join(paths)))

    os.execvp(cmd[0], cmd)


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("path", nargs="+")
    args = parser.parse_args()

    run_benchmark(args.path)
