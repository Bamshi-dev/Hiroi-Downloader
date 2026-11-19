#!/usr/bin/env python3
"""Times hiroi against other downloaders on the same local server.

    cargo build --release -p hiroi-cli -p hiroi-testserver
    python3 bench/run.py [--size 256M] [--rate 10M] [--runs 3] [--connections 8]

For each tool and run we record the wall time, the CPU time (user and
system, from the kernel when the process exits), and the peak memory
(the high-water mark of resident memory, sampled from /proc every few
milliseconds, so Linux only). Every copy is checked against
the server's SHA-256, so a fast wrong answer does not count.

Tools that are not installed are skipped. The table goes to standard
output in Markdown.
"""

import argparse
import hashlib
import os
import shutil
import subprocess
import sys
import tempfile
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
RELEASE = ROOT / "target" / "release"


def tools(url, out, connections):
    """Command lines, by name. Each writes the file to `out`."""
    return {
        "hiroi": [str(RELEASE / "hiroi"), "get", "--quiet", "-c", str(connections),
                  "-d", str(out.parent), "-o", out.name, "--on-conflict", "overwrite", url],
        "aria2c": ["aria2c", "-q", "-x", str(connections), "-s", str(connections), "-k", "1M",
                   "--allow-overwrite=true", "-d", str(out.parent), "-o", out.name, url],
        "curl": ["curl", "-s", "-o", str(out), url],
        "wget": ["wget", "-q", "-O", str(out), url],
    }


def run(argv):
    """Runs a command; returns wall seconds, CPU seconds, and peak KiB.

    The kernel's own peak (ru_maxrss) would include this Python process,
    which every child starts as a copy of, so the peak is sampled from
    /proc instead: VmHWM belongs to the program after exec.
    """
    start = time.monotonic()
    process = subprocess.Popen(argv, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
    status_file = f"/proc/{process.pid}/status"
    peak = 0
    while True:
        pid, status, usage = os.wait4(process.pid, os.WNOHANG)
        if pid:
            break
        peak = max(peak, high_water(status_file))
        time.sleep(0.002)
    wall = time.monotonic() - start
    if os.waitstatus_to_exitcode(status) != 0:
        sys.exit(f"{argv[0]} failed: {process.stderr.read().decode(errors='replace')}")
    return wall, usage.ru_utime + usage.ru_stime, peak


def high_water(status_file):
    """VmHWM in KiB, or 0 once the process is gone."""
    try:
        with open(status_file) as status:
            for line in status:
                if line.startswith("VmHWM:"):
                    return int(line.split()[1])
    except OSError:
        pass
    return 0


def digest(path):
    sha = hashlib.sha256()
    with open(path, "rb") as file:
        while chunk := file.read(1 << 20):
            sha.update(chunk)
    return sha.hexdigest()


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--size", default="256M")
    parser.add_argument("--rate", help="limit per connection, e.g. 10M")
    parser.add_argument("--runs", type=int, default=3)
    parser.add_argument("--connections", type=int, default=8)
    args = parser.parse_args()

    server_argv = [str(RELEASE / "hiroi-testserver"), "--size", args.size]
    if args.rate:
        server_argv += ["--rate", args.rate]
    server = subprocess.Popen(server_argv, stdout=subprocess.PIPE, text=True)
    try:
        url = server.stdout.readline().strip()
        expected = server.stdout.readline().strip()
        rows = []
        with tempfile.TemporaryDirectory() as folder:
            out = Path(folder) / "bench.bin"
            for name, argv in tools(url, out, args.connections).items():
                if shutil.which(argv[0]) is None:
                    print(f"skipping {name}: not installed", file=sys.stderr)
                    continue
                results = []
                for _ in range(args.runs):
                    out.unlink(missing_ok=True)
                    results.append(run(argv))
                    if digest(out) != expected:
                        sys.exit(f"{name} wrote a wrong copy")
                # The middle run, so one slow start does not decide.
                wall, cpu, peak = sorted(results)[len(results) // 2]
                rows.append((name, wall, cpu, peak))
    finally:
        server.terminate()

    rate = f", {args.rate}/s per connection" if args.rate else ", no speed limit"
    print(f"{args.size} from 127.0.0.1{rate}, median of {args.runs} runs\n")
    print("| tool | wall time | CPU time | peak memory |")
    print("|---|---:|---:|---:|")
    for name, wall, cpu, peak in rows:
        print(f"| {name} | {wall:.2f} s | {cpu:.2f} s | {peak / 1024:.1f} MiB |")


if __name__ == "__main__":
    main()
