#!/usr/bin/python3
"""Compare benchmark output, and optionally enforce a ceiling.

Two modes, and they answer different questions.

The report mode (-b/-t) diffs a PR against main. It is advisory by design and
always exits 0: it is read by a person, and cross-run noise on a shared runner
would make it useless as a gate.

The budget mode (--baseline) checks a run against absolute committed ceilings
and exits non-zero when one is exceeded. Absolute rather than a diff against
main, because once a change merges, main *is* the change: a diff-based ratchet
lets a regression through once and then treats it as the new floor.
"""

import argparse
import json
import re
import sys
from dataclasses import dataclass
from typing import Dict

parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
parser.add_argument("-b", "--base-results", dest="base_results_file_path", type=str, help="Path to base results output file")
parser.add_argument("-t", "--test-results", dest="test_results_file_path", type=str, help="Path to test results output file", required=True)
parser.add_argument("--baseline", dest="baseline_file_path", type=str, help="Path to a committed JSON budget; enables the failing check")
parser.add_argument("--update-baseline", dest="update_baseline", action="store_true", help="Rewrite the budget from the test results instead of checking against it")

args = parser.parse_args()

if not args.baseline_file_path and not args.base_results_file_path:
    parser.error("one of --base-results or --baseline is required")

@dataclass
class Benchmark:
    test_name: str
    duration_in_ns: int
    plus_or_minus_in_ns: int

def parse_benchmarks_results(file_path: str) -> Dict[str, Benchmark]:
    benchmarks = {}

    with open(file_path, "r") as file:
        for line in file.readlines():
            match = re.match(r"test (.*) \.\.\. bench: +([\d,]+) ns/iter \(\+/- ([\d,]+)\)", line.strip())
            if match:
                benchmark = Benchmark(
                    test_name=match.group(1),
                    duration_in_ns=int(match.group(2).replace(',', '')),
                    plus_or_minus_in_ns=int(match.group(3).replace(',', ''))
                )

                benchmarks[benchmark.test_name] = benchmark

    return benchmarks

test_results = parse_benchmarks_results(args.test_results_file_path)


def check_against_baseline(baseline_file_path: str, results: Dict[str, Benchmark]) -> int:
    """Check a run against committed ceilings. Returns a process exit code."""
    with open(baseline_file_path, "r") as file:
        baseline = json.load(file)

    budgets = baseline["benchmarks"]
    failures = []
    missing = []

    print("# Performance Budget")
    print()
    print(baseline.get("note", ""))
    print()
    print(f"| {'Benchmark name':38} | {'Measured (μs)':>13} | {'Ceiling (μs)':>13} | {'Verdict':>10} |")
    print(f"| {'-' * 38} | {'-' * 13} | {'-' * 13} | {'-' * 10} |")

    for name in sorted(budgets):
        ceiling_us = float(budgets[name])
        if name not in results:
            missing.append(name)
            print(f"| `{name:36}` | {'not run':>13} | `{ceiling_us:8.2f} μs` | {'MISSING':>10} |")
            continue

        measured_us = results[name].duration_in_ns / 1000.0
        over = measured_us > ceiling_us
        if over:
            failures.append((name, measured_us, ceiling_us))
        print(f"| `{name:36}` | `{measured_us:8.2f} μs` | `{ceiling_us:8.2f} μs` | {'OVER' if over else 'ok':>10} |")

    untracked = sorted(set(results) - set(budgets))
    if untracked:
        print()
        print("Benchmarks with no budget (add them to the baseline to track them):")
        for name in untracked:
            print(f"  - {name}")

    if missing:
        print()
        print("Budgeted benchmarks that did not run. A benchmark that is renamed or")
        print("deleted must be removed from the baseline on purpose, not silently:")
        for name in missing:
            print(f"  - {name}")

    if failures:
        print()
        print("Over budget:")
        for name, measured_us, ceiling_us in failures:
            excess = 100.0 * (measured_us - ceiling_us) / ceiling_us
            print(f"  - {name}: {measured_us:.2f} μs exceeds {ceiling_us:.2f} μs by {excess:.1f}%")

    return 1 if (failures or missing) else 0


if args.baseline_file_path:
    if args.update_baseline:
        with open(args.baseline_file_path, "r") as file:
            baseline = json.load(file)
        for name in baseline["benchmarks"]:
            if name in test_results:
                measured_us = test_results[name].duration_in_ns / 1000.0
                baseline["benchmarks"][name] = round(measured_us * baseline["headroom"], 2)
        with open(args.baseline_file_path, "w") as file:
            json.dump(baseline, file, indent=2)
            file.write("\n")
        print(f"Rewrote {args.baseline_file_path} from {args.test_results_file_path}.")
        sys.exit(0)

    sys.exit(check_against_baseline(args.baseline_file_path, test_results))

base_results = parse_benchmarks_results(args.base_results_file_path)

base_test_names = set(base_results.keys())
test_test_names = set(test_results.keys())

removed_from_base = base_test_names - test_test_names
added_by_test = test_test_names - base_test_names
common = base_test_names & test_test_names

print("# Performance Benchmark Report")

if common:
    print(f"| {'Benchmark name':38} | {'Baseline (μs)':>13} | {'Test/PR (μs)':>13} | {'Delta (μs)':>13} | {'Delta %':15} |")
    print(f"| {'-' * 38} | {'-' * 13} | {'-' * 13} | {'-' * 13} | {'-' * 15} |")
    for name in sorted(common):
        # Retrieve base data
        base_duration = base_results[name].duration_in_ns / 1000.0
        base_plus_or_minus = base_results[name].plus_or_minus_in_ns / 1000.0
        base_plus_or_minus_percentage = (100.0 * base_plus_or_minus) / base_duration

        # Retrieve test data
        test_duration = test_results[name].duration_in_ns / 1000.0
        test_plus_or_minus = test_results[name].plus_or_minus_in_ns / 1000.0
        test_plus_or_minus_percentage = (100.0 * test_plus_or_minus) / test_duration

        # Compute delta
        delta_duration = test_duration - base_duration
        delta_percentage = (100.0 * delta_duration) / base_duration
        abs_delta_percentage = abs(delta_percentage)
        max_plus_or_minus_percentage = max(base_plus_or_minus_percentage, test_plus_or_minus_percentage)

        # Format
        delta_str = f"{delta_duration:8.2f}"

        if abs_delta_percentage > max_plus_or_minus_percentage:
            if delta_percentage < 0:
                delta_prefix = "🟢 "
            elif delta_percentage > 0:
                delta_prefix = "🟠 +"
            else:
                delta_prefix = "⚪  "

            delta_percentage_str = f"{delta_prefix}{delta_percentage:.2f}%"
        else:
            delta_percentage_str = "⚪  Unchanged"

        print(f"| `{name:36}` | `{base_duration:8.2f} μs` | `{test_duration:8.2f} μs` | `{delta_str:>8} μs` | `{delta_percentage_str:12}` |")

if removed_from_base:
    print()
    print("Benchmarks removed:")
    for name in removed_from_base:
        print(f"  - {name}")

if added_by_test:
    print()
    print("Benchmarks added:")
    for name in added_by_test:
        print(f"  - {name}")
