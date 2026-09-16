#!/usr/bin/env python3
"""Benchmark fleets against ansible-inventory on a generated large inventory.

Usage:
    cargo build --release
    python3 bench/benchmark.py [num_groups] [hosts_per_group]

Requires `ansible-inventory` on PATH for the comparison (the fleets-only timing
still runs without it).
"""
import json
import os
import platform
import statistics
import subprocess
import sys
import tempfile
import time

NUM_GROUPS = int(sys.argv[1]) if len(sys.argv) > 1 else 50
PER_GROUP = int(sys.argv[2]) if len(sys.argv) > 2 else 100

ENV = dict(
    os.environ,
    ANSIBLE_NOCOLOR="1",
    LC_ALL="en_US.UTF-8",
    LANG="en_US.UTF-8",
    ANSIBLE_TRANSFORM_INVALID_GROUP_CHARS="silently",
    ANSIBLE_INVENTORY_ENABLED="host_list,script,auto,yaml,ini,toml",
)
FLEETS = os.path.join(os.path.dirname(__file__), "..", "target", "release", "fleets")


def write_inventory(path):
    lines = []
    for g in range(NUM_GROUPS):
        start = g * PER_GROUP
        end = start + PER_GROUP - 1
        lines += [f"[group{g:02d}]", f"host[{start:05d}:{end:05d}].example.com svc=app idx={g}", ""]
        lines += [f"[group{g:02d}:vars]", f"region = zone{g % 5}", f"weight = {g}", ""]
    lines.append("[everything:children]")
    lines += [f"group{g:02d}" for g in range(NUM_GROUPS)]
    open(path, "w").write("\n".join(lines) + "\n")


def run_checked(cmd, env=None):
    return subprocess.run(cmd, capture_output=True, check=True, env=env, text=True)


def bench(cmd, env=None, n=7):
    run_checked(cmd, env=env)  # warm caches without counting the warmup
    times = []
    for _ in range(n):
        t = time.perf_counter()
        run_checked(cmd, env=env)
        times.append(time.perf_counter() - t)
    return min(times), statistics.median(times)


def have(cmd):
    try:
        return run_checked([cmd, "--version"]).returncode == 0
    except FileNotFoundError:
        return False
    except subprocess.CalledProcessError:
        return False


def main():
    total = NUM_GROUPS * PER_GROUP
    if not os.path.isfile(FLEETS):
        raise SystemExit(f"release binary not found at {FLEETS}; run cargo build --release")

    with tempfile.TemporaryDirectory() as d:
        inv = os.path.join(d, "hosts.ini")
        write_inventory(inv)
        print(f"inventory: {total} hosts across {NUM_GROUPS} groups\n")
        print(f"platform: {platform.platform()}")
        print(f"python: {platform.python_version()}")

        fleets_cmd = [FLEETS, "--strict-compatibility", "--list", "-i", inv]

        f_min, f_med = bench(fleets_cmd)
        print(f"fleets (release):   min={f_min*1000:8.1f} ms  median={f_med*1000:8.1f} ms")

        if have("ansible-inventory"):
            ansible_cmd = ["ansible-inventory", "--list", "-i", inv]
            fleets_output = json.loads(run_checked(fleets_cmd).stdout)
            ansible_output = json.loads(run_checked(ansible_cmd, env=ENV).stdout)
            if fleets_output != ansible_output:
                raise SystemExit("refusing to benchmark: fleets and ansible outputs differ")

            print(run_checked(["ansible-inventory", "--version"]).stdout.splitlines()[0])
            a_min, a_med = bench(ansible_cmd, env=ENV)
            print(f"ansible-inventory:  min={a_min*1000:8.1f} ms  median={a_med*1000:8.1f} ms")
            print(f"\nspeedup (median): {a_med / f_med:.1f}x faster")
        else:
            print("ansible-inventory not found; skipping comparison")


if __name__ == "__main__":
    main()
