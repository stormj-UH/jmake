#!/bin/sh
# Phase 3 Tier 1 real-world test driver for jmake conformance.
# Compares `jmake -n` output against GNU make -n for real-world packages.
# Tarballs are cached in tests/realworld/cache/ (git-ignored).
# Auto-downloads missing tarballs if curl is available.
#
# Usage: tests/realworld/run.sh [JMAKE_BIN]
# Exit 0 if all packages match, non-zero with summary if any drift.
set -e

SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd)
CACHE="$SCRIPT_DIR/cache"
JMAKE=${1:-/tmp/jmake-tests/target/release/jmake}
GNU_MAKE=${GNU_MAKE:-/usr/bin/make}
WORK=$(mktemp -d /tmp/jmake-rw-XXXXXX)
trap 'rm -rf "$WORK"' EXIT

mkdir -p "$CACHE"

PASS=0
FAIL=0
SKIP=0
REPORT=""

sha256_check() {
    tarball=$1; expected=$2
    actual=$(sha256sum "$tarball" | cut -d' ' -f1)
    [ "$actual" = "$expected" ]
}

fetch_if_missing() {
    tarball=$1; url=$2; sha256=$3
    if [ ! -f "$tarball" ]; then
        if command -v curl > /dev/null 2>&1; then
            printf "Downloading %s...\n" "$(basename "$tarball")"
            curl -L --silent --show-error --fail -o "$tarball" "$url" || {
                printf "Download failed for %s\n" "$(basename "$tarball")" >&2
                return 1
            }
        else
            printf "SKIP: %s not cached and curl not available\n" "$(basename "$tarball")"
            return 1
        fi
    fi
    sha256_check "$tarball" "$sha256" || {
        printf "SHA256 mismatch for %s — delete and re-download\n" "$(basename "$tarball")" >&2
        return 1
    }
}

run_pkg() {
    name=$1; tarball_name=$2; sha256=$3; url=$4; target=$5; configure_cmd=$6
    tarball="$CACHE/$tarball_name"

    fetch_if_missing "$tarball" "$url" "$sha256" || { SKIP=$((SKIP + 1)); return; }

    extract="$WORK/$name"
    mkdir -p "$extract"
    case "$tarball" in
        *.tar.bz2) tar -xjf "$tarball" -C "$extract" ;;
        *.tar.gz|*.tgz) tar -xzf "$tarball" -C "$extract" ;;
        *) printf "SKIP %-20s (unknown archive format)\n" "$name"
           SKIP=$((SKIP + 1)); return ;;
    esac

    srcdir=$(ls -1 "$extract" | head -1)
    srcdir="$extract/$srcdir"

    if [ -n "$configure_cmd" ]; then
        (cd "$srcdir" && eval "$configure_cmd" > /dev/null 2>&1) || {
            printf "SKIP %-20s (configure failed)\n" "$name"
            SKIP=$((SKIP + 1)); return
        }
    fi

    gnu_out="$WORK/${name}-gnu.out"
    jmake_out="$WORK/${name}-jmake.out"
    (cd "$srcdir" && "$GNU_MAKE" -n "$target" >"$gnu_out" 2>&1) || true
    (cd "$srcdir" && JMAKE_TEST_MODE=1 "$JMAKE" -n "$target" >"$jmake_out" 2>&1) || true

    gnu_lines=$(wc -l < "$gnu_out")
    jmake_lines=$(wc -l < "$jmake_out")

    if diff "$gnu_out" "$jmake_out" > /dev/null 2>&1; then
        printf "PASS  %-20s  gnu=%d lines\n" "$name" "$gnu_lines"
        PASS=$((PASS + 1))
    else
        drift=$(diff "$gnu_out" "$jmake_out" | grep -c "^[<>]") || drift=0
        printf "DRIFT %-20s  gnu=%d jmake=%d changed=%d\n" \
            "$name" "$gnu_lines" "$jmake_lines" "$drift"
        REPORT="${REPORT}
--- $name ---
$(diff "$gnu_out" "$jmake_out" | head -8)"
        FAIL=$((FAIL + 1))
    fi
}

# ---- package table: name tarball sha256 url target configure ----

run_pkg "musl-1.2.6" \
    "musl-1.2.6.tar.gz" \
    "d585fd3b613c66151fc3249e8ed44f77020cb5e6c1e635a616d3f9f82460512a" \
    "https://musl.libc.org/releases/musl-1.2.6.tar.gz" \
    "lib/libc.a" \
    "./configure --prefix=/tmp/jmake-rw-musl"

run_pkg "expat-2.6.4" \
    "expat-2.6.4.tar.gz" \
    "fd03b7172b3bd7427a3e7a812063f74754f24542429b634e0db6511b53fb2278" \
    "https://github.com/libexpat/libexpat/releases/download/R_2_6_4/expat-2.6.4.tar.gz" \
    "all" \
    "./configure --prefix=/tmp/jmake-rw-expat"

run_pkg "libffi-3.4.6" \
    "libffi-3.4.6.tar.gz" \
    "b0dea9df23c863a7a50e825440f3ebffabd65df1497108e5d437747843895a4e" \
    "https://github.com/libffi/libffi/releases/download/v3.4.6/libffi-3.4.6.tar.gz" \
    "libffi.la" \
    "./configure --prefix=/tmp/jmake-rw-libffi"

run_pkg "dropbear-2024.86" \
    "dropbear-2024.86.tar.bz2" \
    "e78936dffc395f2e0db099321d6be659190966b99712b55c530dd0a1822e0a5e" \
    "https://matt.ucc.asn.au/dropbear/releases/dropbear-2024.86.tar.bz2" \
    "dropbear" \
    "./configure --prefix=/tmp/jmake-rw-dropbear"

run_pkg "toybox-0.8.11" \
    "toybox-0.8.11.tar.gz" \
    "15aa3f832f4ec1874db761b9950617f99e1e38144c22da39a71311093bfe67dc" \
    "https://landley.net/toybox/downloads/toybox-0.8.11.tar.gz" \
    "toybox" \
    ""

printf '\nResults: %d matched, %d drifted, %d skipped\n' "$PASS" "$FAIL" "$SKIP"

if [ "$FAIL" -gt 0 ]; then
    printf '\nDrift details:\n%s\n' "$REPORT"
    printf '\nKnown drift causes:\n'
    printf '  $(MAKE): GNU make uses argv[0] (/usr/bin/make); test mode uses "make"\n'
    printf '  ./prefix: jmake wildcard expansion prepends ./ to bare-name paths\n'
    printf '  execution order: independent target order may differ\n'
    exit 1
fi
exit 0
