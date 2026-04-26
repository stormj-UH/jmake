#!/bin/sh
# Feature test driver: run each .mk file through jmake with JMAKE_TEST_MODE=1,
# diff combined stdout+stderr against the golden file captured from GNU make.
# All tests run with CWD set to the tests/feature directory so paths in
# error messages match the golden (relative path as given to -f).
set -e

SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd)
JMAKE=${1:-/tmp/jmake-tests/target/release/jmake}
PASS=0
FAIL=0
ERRORS=""

cd "$SCRIPT_DIR"

for mkfile in *.mk; do
    name="${mkfile%.mk}"
    golden="$name.golden"
    if [ ! -f "$golden" ]; then
        printf "SKIP %-40s (no golden)\n" "$name"
        continue
    fi

    case "$name" in
        make_k)   flags="-k" ;;
        make_r)   flags="-r" ;;
        make_flags_n) flags="-n" ;;
        *)        flags="" ;;
    esac

    actual=$(JMAKE_TEST_MODE=1 "$JMAKE" -f "$mkfile" $flags 2>&1) || true
    expected=$(cat "$golden")

    if [ "$actual" = "$expected" ]; then
        printf "PASS %-40s\n" "$name"
        PASS=$((PASS + 1))
    else
        printf "FAIL %-40s\n" "$name"
        FAIL=$((FAIL + 1))
        diff_out=$(diff - - << DIFFEOF
$(printf '%s' "$expected")
DIFFEOF
)
        ERRORS="$ERRORS
--- $name ---
$(diff <(printf '%s\n' "$expected") <(printf '%s\n' "$actual") 2>&1 || true)"
    fi
done

printf '\nResults: %d passed, %d failed\n' "$PASS" "$FAIL"
if [ -n "$ERRORS" ]; then
    printf '%s\n' "$ERRORS"
    exit 1
fi
exit 0
