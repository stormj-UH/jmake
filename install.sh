#!/bin/sh
# install.sh — POSIX installer for jmake (clean-room GNU Make 4.4.1 replacement).
#
# One-liner:
#   curl -fsSL https://raw.githubusercontent.com/stormj-UH/jmake/main/install.sh | sh
#
# Installs the jmake .jpkg from the jonerix release page, verifies the JPKG
# magic, extracts the zstd-compressed tar payload, and places the binary +
# license under $PREFIX (default /usr/local).

set -eu

DEFAULT_VERSION="1.2.1"
DEFAULT_PREFIX="/usr/local"
URL_BASE="https://github.com/stormj-UH/jonerix/releases/download/packages"
ISSUES_URL="https://github.com/stormj-UH/jmake/issues"

VERSION="$DEFAULT_VERSION"
PREFIX="$DEFAULT_PREFIX"
ARCH=""
MAKE_DEFAULT=0

usage() {
	cat <<EOF
Usage: install.sh [options]

Options:
  --version <VER>      jmake version to install (default: $DEFAULT_VERSION)
  --version=<VER>
  --prefix  <DIR>      install prefix (default: $DEFAULT_PREFIX)
  --prefix=<DIR>
  --arch    <ARCH>     override architecture (default: detected via uname -m)
  --arch=<ARCH>
  --make-default       also install \$PREFIX/bin/make as a symlink to jmake
                       (default: install only as 'jmake'; never touches
                       /usr/bin/make)
  --help, -h           show this message and exit

Supported architectures: x86_64, aarch64
Required tools: curl or wget, zstd, tar, od, dd
EOF
}

err() { printf '%s\n' "install.sh: $*" >&2; }
die() { err "$*"; exit 1; }

# ---- arg parsing (POSIX) -----------------------------------------------------
while [ $# -gt 0 ]; do
	case "$1" in
		--version)        [ $# -ge 2 ] || die "--version requires an argument"; VERSION="$2"; shift 2 ;;
		--version=*)      VERSION="${1#--version=}"; shift ;;
		--prefix)         [ $# -ge 2 ] || die "--prefix requires an argument"; PREFIX="$2"; shift 2 ;;
		--prefix=*)       PREFIX="${1#--prefix=}"; shift ;;
		--arch)           [ $# -ge 2 ] || die "--arch requires an argument"; ARCH="$2"; shift 2 ;;
		--arch=*)         ARCH="${1#--arch=}"; shift ;;
		--make-default)   MAKE_DEFAULT=1; shift ;;
		--help|-h)        usage; exit 0 ;;
		--)               shift; break ;;
		*)                err "unknown option: $1"; usage >&2; exit 2 ;;
	esac
done

# ---- arch detection ----------------------------------------------------------
if [ -z "$ARCH" ]; then
	uname_m=$(uname -m)
	case "$uname_m" in
		x86_64|amd64)        ARCH="x86_64" ;;
		aarch64|arm64)       ARCH="aarch64" ;;
		*)                   die "unsupported architecture: $uname_m (try --arch x86_64 or --arch aarch64)" ;;
	esac
fi
case "$ARCH" in
	x86_64|aarch64) ;;
	*) die "unsupported --arch: $ARCH (supported: x86_64, aarch64)" ;;
esac

# ---- tool check --------------------------------------------------------------
have() { command -v "$1" >/dev/null 2>&1; }

if have curl; then
	DL="curl"
elif have wget; then
	DL="wget"
else
	die "need curl or wget on PATH"
fi
for t in zstd tar od dd; do
	have "$t" || die "missing required tool: $t"
done

# ---- workspace ---------------------------------------------------------------
TMP=$(mktemp -d 2>/dev/null || mktemp -d -t jmake-install)
[ -n "$TMP" ] && [ -d "$TMP" ] || die "could not create temp dir"
cleanup() { rm -rf "$TMP"; }
trap cleanup EXIT INT HUP TERM

JPKG="$TMP/jmake-$VERSION-$ARCH.jpkg"
URL="$URL_BASE/jmake-$VERSION-$ARCH.jpkg"
EXTRACT="$TMP/extract"
mkdir -p "$EXTRACT"

# ---- download ----------------------------------------------------------------
printf 'install.sh: downloading %s\n' "$URL" >&2
if [ "$DL" = "curl" ]; then
	curl -fsSL --retry 3 --retry-delay 1 -o "$JPKG" "$URL" || die "download failed: $URL"
else
	wget -q -O "$JPKG" "$URL" || die "download failed: $URL"
fi
[ -s "$JPKG" ] || die "downloaded file is empty: $JPKG"

# ---- magic check -------------------------------------------------------------
magic=$(dd if="$JPKG" bs=1 count=4 status=none 2>/dev/null)
if [ "$magic" != "JPKG" ]; then
	die "not a JPKG file (bad magic): $JPKG"
fi

# ---- extract -----------------------------------------------------------------
md_len=$(od -An -tu4 -N4 -j8 "$JPKG" | tr -d ' \t\n')
case "$md_len" in
	''|*[!0-9]*) die "could not read metadata length from JPKG header" ;;
esac
payload_off=$((12 + md_len))

dd if="$JPKG" bs=1 skip="$payload_off" status=none | zstd -d -q | tar -x -C "$EXTRACT" \
	|| die "failed to extract JPKG payload"

[ -f "$EXTRACT/bin/jmake" ] || die "extracted payload missing bin/jmake"

# ---- install -----------------------------------------------------------------
DEST_BIN="$PREFIX/bin"
DEST_LIC="$PREFIX/share/licenses/jmake"

# Try without sudo first; escalate if PREFIX isn't writable.
need_sudo=0
if ! mkdir -p "$DEST_BIN" "$DEST_LIC" 2>/dev/null; then
	if have sudo; then
		need_sudo=1
		sudo mkdir -p "$DEST_BIN" "$DEST_LIC" || die "could not create $DEST_BIN / $DEST_LIC"
	else
		die "cannot write to $PREFIX and no sudo available; rerun as root or pass --prefix \$HOME/.local"
	fi
fi

run() {
	if [ "$need_sudo" = 1 ]; then sudo "$@"; else "$@"; fi
}

run install -m 0755 "$EXTRACT/bin/jmake" "$DEST_BIN/jmake"

# License: try a few common locations from the payload.
license_src=""
for cand in "$EXTRACT/share/licenses/jmake/LICENSE" "$EXTRACT/LICENSE" "$EXTRACT/usr/share/licenses/jmake/LICENSE"; do
	if [ -f "$cand" ]; then license_src="$cand"; break; fi
done
if [ -n "$license_src" ]; then
	run install -m 0644 "$license_src" "$DEST_LIC/LICENSE"
else
	err "warning: LICENSE not found in payload; skipping license install"
fi

# ---- optional: install as 'make' --------------------------------------------
if [ "$MAKE_DEFAULT" = 1 ]; then
	# Warn if /usr/bin/make exists ahead on PATH and would still win.
	first_make=""
	IFS_SAVE=$IFS
	IFS=:
	for d in $PATH; do
		[ -z "$d" ] && continue
		if [ -x "$d/make" ]; then first_make="$d/make"; break; fi
	done
	IFS=$IFS_SAVE
	if [ -n "$first_make" ] && [ "$first_make" = "/usr/bin/make" ] && [ "$DEST_BIN" != "/usr/bin" ]; then
		err "note: /usr/bin/make appears earlier on PATH than $DEST_BIN; reorder PATH or remove the system make to use jmake as 'make'"
	fi
	run ln -sf jmake "$DEST_BIN/make"
fi

# ---- verify ------------------------------------------------------------------
got=$("$DEST_BIN/jmake" --version 2>/dev/null | head -n1 || true)
case "$got" in
	*"$VERSION"*) printf 'install.sh: installed: %s\n' "$got" ;;
	*) die "verification failed: '$DEST_BIN/jmake --version' did not include $VERSION (got: $got)" ;;
esac

# ---- PATH advisory -----------------------------------------------------------
case ":$PATH:" in
	*":$DEST_BIN:"*) ;;
	*) err "note: $DEST_BIN is not on PATH; add it to your shell profile (e.g. 'export PATH=\"$DEST_BIN:\$PATH\"')" ;;
esac

cat <<EOF

jmake $VERSION installed to $DEST_BIN/jmake.

Compatibility note:
  jmake aims for GNU Make 4.4.1 compatibility; if you hit a missing feature,
  run \`jmake --version\` and report the version + the unsupported syntax to
  $ISSUES_URL.
EOF
