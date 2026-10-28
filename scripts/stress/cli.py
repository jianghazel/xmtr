"""Bounded CLI stress tests. Network probes use loopback and never upload routes."""

import argparse
import json
import os
from pathlib import Path
import random
import resource
import subprocess
import time


def run(binary, arguments, *, limits=None, timeout=8, prefix=()):
    def constrain():
        for key, value in (limits or {}).items():
            resource.setrlimit(key, (value, value))

    started = time.monotonic()
    result = subprocess.run(
        [*prefix, str(binary), *arguments],
        capture_output=True,
        timeout=timeout,
        check=False,
        preexec_fn=constrain if limits else None,
        env={**os.environ, "NO_COLOR": "1", "XMTR_API": "http://127.0.0.1:9"},
    )
    return result, time.monotonic() - started


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path)
    parser.add_argument("--cases", type=int, default=2000)
    parser.add_argument("--trace", action="store_true")
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    binary = args.binary.resolve()
    rng = random.Random(391029)
    records = []
    failures = []

    def record(name, arguments, expected, **options):
        result, elapsed = run(binary, arguments, **options)
        passed = result.returncode in expected and b"panicked at" not in result.stderr
        records.append(
            {
                "name": name,
                "seconds": elapsed,
                "exit": result.returncode,
                "passed": passed,
            }
        )
        if not passed:
            failures.append(
                {
                    "name": name,
                    "arguments": arguments,
                    "stderr": result.stderr[-2000:].decode(errors="replace"),
                }
            )
        return result

    for option, values in {
        "--max-rounds": ["0", "65536", "-1", "1.5", "9" * 4096],
        "--min-rounds": ["0", "65536", "-1"],
        "--max-ttl": ["0", "255", "256", "-1"],
        "--parallel": ["0", "65", "65536", "-1"],
        "--port": ["0", "65536", "-1"],
        "--interval": ["0s", "99ms", "-1s", "nan", "inf", "9" * 4096 + "s"],
        "--time-limit": ["0s", "999ms", "-1s", "nan", "inf"],
        "--pin": ["", "1234", "9999", "１２３５", "1" * 4096],
        "--dns": ["ftp://127.0.0.1", "https://[::1]/dns-query", "https://"],
    }.items():
        for value in values:
            record(
                f"boundary {option} {value[:24]}",
                ["--no-upload", option, value, "127.0.0.1"],
                {2},
            )

    options = [
        "--parallel",
        "--max-ttl",
        "--port",
        "--max-rounds",
        "--interval",
        "--time-limit",
        "--dns",
        "--pin",
    ]
    alphabet = "abcdefghijklmnopqrstuvwxyz0123456789-+.:/[]µ中文"
    for index in range(args.cases):
        value = "!" + "".join(rng.choices(alphabet, k=rng.randrange(0, 256)))
        option = rng.choice(options)
        if option == "--dns":
            value = "ftp://" + value
        record(
            f"random-invalid-{index}", ["--no-upload", option, value, "127.0.0.1"], {2}
        )

    for descriptors in [8, 16, 32]:
        record(
            f"help nofile={descriptors}",
            ["--help"],
            {0},
            limits={resource.RLIMIT_NOFILE: descriptors},
        )
    for memory in [32, 64, 128]:
        record(
            f"help address-space={memory}MiB",
            ["--help"],
            {0},
            limits={resource.RLIMIT_AS: memory * 1024 * 1024},
        )
    record("large argument list", ["--help", *(["127.0.0.1"] * 10000)], {0})

    if args.trace:
        common = [
            "--no-upload",
            "--no-rdns",
            "--json",
            "--quiet",
            "-p",
            "icmp",
            "-c",
            "1",
        ]
        for parallel in [1, 8, 64]:
            result = record(
                f"loopback parallel={parallel}",
                [*common, "-j", str(parallel), *(["127.0.0.1"] * 64)],
                {0},
                timeout=150,
            )
            if result.returncode == 0:
                body = json.loads(result.stdout)
                assert len(body["routes"]) == 64, body
                assert all(route["reached"] for route in body["routes"]), body
        for duration in ["4294967295s", "18446744073709551615s"]:
            record(
                f"huge duration {duration}", [*common, "-t", duration, "127.0.0.1"], {0}
            )
        for descriptors in [16, 32]:
            record(
                f"trace nofile={descriptors}",
                [*common, "127.0.0.1"],
                {0, 1},
                limits={resource.RLIMIT_NOFILE: descriptors},
            )

    output = {
        "binary": str(binary),
        "random_seed": 391029,
        "cases": len(records),
        "failures": failures,
        "records": records,
    }
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(output, indent=2) + "\n")
    print(
        json.dumps(
            {
                "cases": len(records),
                "failures": len(failures),
                "seconds": sum(row["seconds"] for row in records),
            }
        )
    )
    if failures:
        print(json.dumps(failures[:10], indent=2))
        raise SystemExit(1)


if __name__ == "__main__":
    main()
