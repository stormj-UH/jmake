#!/bin/sh
# upstream_441_runner.sh — Run GNU Make 4.4.1 upstream tests against jmake.
#
# Usage:
#   ./tests/upstream_441_runner.sh [path-to-jmake]
#
# Requires: perl, diff, mktemp, tar
#
# Clean-room note: This script extracts only the *test cases* (black-box
# specifications) from the upstream suite.  It never reads or references
# GNU Make source code (src/).  GPL test content is used solely as a spec.

set -e

SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd)
REPO_ROOT=$(cd "$SCRIPT_DIR/.." && pwd)

JMAKE=${1:-"$REPO_ROOT/target/release/jmake"}
if [ ! -x "$JMAKE" ]; then
    echo "jmake binary not found at $JMAKE — building..." >&2
    (cd "$REPO_ROOT" && cargo build --release 2>&1)
    JMAKE="$REPO_ROOT/target/release/jmake"
fi

TARBALL="$SCRIPT_DIR/vendor/make-4.4.1.tar.gz"
EXTRACT_DIR=$(mktemp -d /tmp/jmake-upstream-runner-XXXXXX)
trap 'rm -rf "$EXTRACT_DIR"' EXIT

echo "Extracting upstream test suite..."
tar -xzf "$TARBALL" -C "$EXTRACT_DIR"

UPSTREAM_TESTS="$EXTRACT_DIR/make-4.4.1/tests/scripts"
EXTRACTOR="$SCRIPT_DIR/upstream_441_extractor.pl"

PASS=0
FAIL=0
SKIP=0
FAILURES=""

run_one_category() {
    category="$1"
    dir="$UPSTREAM_TESTS/$category"
    [ -d "$dir" ] || return 0

    for script in "$dir"/*; do
        name=$(basename "$script")
        # Skip VMS-only and platform-specific tests
        case "$name" in
            guile|loadapi|load|jobserver|parallelism|output-sync|symlinks) continue ;;
        esac

        # Extract and run test cases via the extractor
        result=$(perl "$EXTRACTOR" "$script" "$JMAKE" 2>&1) || true

        while IFS='|' read -r status testid; do
            [ -z "$status" ] && continue
            case "$status" in
                PASS) PASS=$((PASS + 1)) ;;
                FAIL) FAIL=$((FAIL + 1))
                      FAILURES="$FAILURES
$testid" ;;
                SKIP) SKIP=$((SKIP + 1)) ;;
            esac
            printf "%s %s/%s\n" "$status" "$category" "$testid"
        done <<EOF
$result
EOF
    done
}

for category in features functions variables options targets misc; do
    run_one_category "$category"
done

echo ""
echo "==============================="
echo "Results: $PASS passed, $FAIL failed, $SKIP skipped"
echo "==============================="
if [ -n "$FAILURES" ]; then
    echo "FAILURES:$FAILURES"
fi

[ "$FAIL" -eq 0 ]
