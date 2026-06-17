#!/usr/bin/env python3
"""Time `burn summary` against a real ledger and check it against a budget.

This is the timing eval for the "summary sub-100ms" goal (see
`plans/009-summary-sub-100ms.md`). It runs the *release* binary end-to-end —
process startup + ingest sweep + query + render — against a live ledger, which
is the number a user actually feels at the prompt.

Usage:
    python3 scripts/bench-summary.py                 # default: live ledger, 20 runs, 100ms budget
    python3 scripts/bench-summary.py --runs 40
    python3 scripts/bench-summary.py --budget-ms 100 --metric p95
    python3 scripts/bench-summary.py --breakdown     # attribute ingest vs query
    python3 scripts/bench-summary.py --json          # machine-readable summary

Exit status is non-zero when the chosen metric exceeds the budget, so this
doubles as a regression gate once the goal is met.

Notes:
  - Defaults to the release binary at target/release/burn and the binary's own
    default ledger ($RELAYBURN_HOME or ~/.agentworkforce/burn). Pass --home to
    point at a different ledger, or --bin to time a different binary.
  - Build first: `cargo build --release -p relayburn-cli` (or pass --build).
  - Measurements are wall-clock around the subprocess, including process spawn.
    stdout/stderr are discarded.
"""

from __future__ import annotations

import argparse
import json
import os
import shlex
import statistics
import subprocess
import sys
import time
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
DEFAULT_BIN = REPO_ROOT / "target" / "release" / "burn"


def percentile(values: list[float], pct: float) -> float:
    """Nearest-rank percentile (pct in 0..100). values need not be sorted."""
    if not values:
        return float("nan")
    ordered = sorted(values)
    if len(ordered) == 1:
        return ordered[0]
    rank = max(1, min(len(ordered), round(pct / 100.0 * len(ordered))))
    return ordered[rank - 1]


def time_command(argv: list[str], env: dict[str, str]) -> float:
    """Return wall-clock seconds for one run of argv, discarding output."""
    start = time.perf_counter()
    subprocess.run(
        argv,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        env=env,
        check=False,
    )
    return time.perf_counter() - start


def run_samples(argv: list[str], env: dict[str, str], warmup: int, runs: int) -> list[float]:
    for _ in range(warmup):
        time_command(argv, env)
    return [time_command(argv, env) * 1000.0 for _ in range(runs)]


def summarize(name: str, samples_ms: list[float]) -> dict:
    return {
        "label": name,
        "runs": len(samples_ms),
        "min_ms": min(samples_ms),
        "median_ms": statistics.median(samples_ms),
        "mean_ms": statistics.fmean(samples_ms),
        "p95_ms": percentile(samples_ms, 95),
        "max_ms": max(samples_ms),
    }


def fmt_row(stats: dict) -> str:
    return (
        f"{stats['label']:<22} "
        f"min {stats['min_ms']:8.1f}  "
        f"median {stats['median_ms']:8.1f}  "
        f"p95 {stats['p95_ms']:8.1f}  "
        f"mean {stats['mean_ms']:8.1f}  "
        f"max {stats['max_ms']:8.1f}   (ms)"
    )


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--bin", default=str(DEFAULT_BIN), help="path to the burn binary")
    ap.add_argument("--home", default=None, help="ledger home (default: binary default / $RELAYBURN_HOME)")
    ap.add_argument("--summary-args", default="summary --since 24h",
                    help='argv after the binary for the summary run (default: "summary --since 24h")')
    ap.add_argument("--runs", type=int, default=20, help="timed runs (default 20)")
    ap.add_argument("--warmup", type=int, default=3, help="discarded warmup runs (default 3)")
    ap.add_argument("--budget-ms", type=float, default=100.0, help="budget the metric is checked against (default 100)")
    ap.add_argument("--metric", choices=["median", "p95", "min", "mean"], default="median",
                    help="which metric the budget gate uses (default median)")
    ap.add_argument("--breakdown", action="store_true",
                    help="also time `ingest` alone to attribute ingest vs query cost")
    ap.add_argument("--build", action="store_true", help="run `cargo build --release -p relayburn-cli` first")
    ap.add_argument("--json", action="store_true", help="emit machine-readable JSON")
    args = ap.parse_args()

    if args.build:
        subprocess.run(["cargo", "build", "--release", "-p", "relayburn-cli"], cwd=REPO_ROOT, check=True)

    bin_path = Path(args.bin)
    if not bin_path.exists():
        print(f"error: binary not found at {bin_path}\n"
              f"build it: cargo build --release -p relayburn-cli  (or pass --build)", file=sys.stderr)
        return 2

    env = dict(os.environ)
    if args.home:
        env["RELAYBURN_HOME"] = args.home
    home_display = args.home or env.get("RELAYBURN_HOME") or "~/.agentworkforce/burn (default)"

    summary_argv = [str(bin_path)] + shlex.split(args.summary_args)

    if not args.json:
        print(f"binary:  {bin_path}")
        print(f"ledger:  {home_display}")
        print(f"command: {' '.join(shlex.quote(a) for a in summary_argv)}")
        print(f"runs:    {args.runs} timed ({args.warmup} warmup)\n")

    summary_stats = summarize("summary", run_samples(summary_argv, env, args.warmup, args.runs))

    results = {"summary": summary_stats}
    if args.breakdown:
        ingest_argv = [str(bin_path), "ingest"]
        ingest_stats = summarize("ingest (alone)", run_samples(ingest_argv, env, args.warmup, args.runs))
        results["ingest"] = ingest_stats
        # Query cost is summary minus ingest at the median; a coarse attribution.
        results["query_estimate_ms"] = max(0.0, summary_stats["median_ms"] - ingest_stats["median_ms"])

    metric_key = f"{args.metric}_ms"
    observed = summary_stats[metric_key]
    passed = observed <= args.budget_ms

    if args.json:
        print(json.dumps({
            "budget_ms": args.budget_ms,
            "metric": args.metric,
            "observed_ms": observed,
            "passed": passed,
            "results": results,
        }, indent=2))
    else:
        print(fmt_row(summary_stats))
        if args.breakdown:
            print(fmt_row(results["ingest"]))
            print(f"\n~query cost (summary median - ingest median): {results['query_estimate_ms']:.1f} ms")
        verdict = "PASS" if passed else "FAIL"
        print(f"\n[{verdict}] summary {args.metric} = {observed:.1f} ms  (budget {args.budget_ms:.0f} ms)")
        if not passed:
            over = observed / args.budget_ms
            print(f"        {over:.1f}x over budget — see plans/009-summary-sub-100ms.md")

    return 0 if passed else 1


if __name__ == "__main__":
    sys.exit(main())
