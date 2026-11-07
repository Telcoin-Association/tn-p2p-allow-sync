#!/usr/bin/env bash
# The gate of ci.yaml's `coverage` job: the total line coverage of the test suite may not fall
# more than 0.5 points below the baseline.
#
#   cargo llvm-cov --locked --summary-only --json --output-path target/coverage.json
#   bash etc/coverage-gate.sh target/coverage.json etc/coverage-baseline
#
# COVERAGE_JSON is the summary cargo llvm-cov writes, llvm-cov's export format, whose total is
# `.data[0].totals.lines.percent`. BASELINE_FILE holds one number, the total in percent that
# main reached. The tolerance absorbs the small moves a refactor makes without letting a
# pull request drop a tested path; raise the baseline in the pull request that raises the
# total. Lowering it is a change to the gate and is reviewed like one.
#
# Exit status: 0 at or above the floor (baseline minus 0.5), 1 below it, 2 for a usage error
# or an input this script cannot read.
set -euo pipefail

# awk prints and compares the numbers; a locale with a decimal comma must not reach it
export LC_ALL=C

TOLERANCE=0.5

die() {
	printf 'coverage-gate: %s\n' "$*" >&2
	exit 2
}

[ "$#" -eq 2 ] || die "usage: coverage-gate.sh COVERAGE_JSON BASELINE_FILE"
json=$1
baseline_file=$2
[ -r "$json" ] || die "cannot read $json"
[ -r "$baseline_file" ] || die "cannot read $baseline_file"

number='^[0-9]+(\.[0-9]+)?$'

# exactly one line holding one number, so a stray edit cannot turn the gate off
mapfile -t lines < "$baseline_file"
if [ "${#lines[@]}" -ne 1 ] || ! [[ ${lines[0]} =~ $number ]]; then
	die "$baseline_file must hold one number on one line, such as 97.3"
fi
baseline=${lines[0]}

# one export with a numeric line total; anything else is not a coverage run
if ! total=$(jq -er '
	if .type == "llvm.coverage.json.export" and (.data | length) == 1
	then .data[0].totals.lines.percent | numbers
	else error("not a single llvm-cov export") end' "$json"); then
	die "$json is not the JSON summary of cargo llvm-cov"
fi

# jq may print a float in exponent form; awk reads that too, so every number goes through awk.
# The comparisons use the unrounded total; only the printed values are rounded.
read -r shown floor < <(awk -v t="$total" -v b="$baseline" -v tol="$TOLERANCE" \
	'BEGIN { printf "%.2f %.2f\n", t, b - tol }')

printf 'coverage: total line coverage %s%%, baseline %s%% (%s), floor %s%%\n' \
	"$shown" "$baseline" "$baseline_file" "$floor"

if awk -v t="$total" -v b="$baseline" -v tol="$TOLERANCE" 'BEGIN { exit !(t < b - tol) }'; then
	printf 'coverage-gate: %s%% is more than %s points below the baseline %s%%; add tests for the new paths\n' \
		"$shown" "$TOLERANCE" "$baseline" >&2
	exit 1
fi

if awk -v t="$total" -v b="$baseline" -v tol="$TOLERANCE" 'BEGIN { exit !(t > b + tol) }'; then
	printf 'coverage: the total is more than %s points above the baseline; raise %s to keep the gain\n' \
		"$TOLERANCE" "$baseline_file"
fi
