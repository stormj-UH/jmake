#!/bin/sh
# install.sh — strictly POSIX installer for jmake (clean-room GNU Make 4.4.1
# replacement). No bashisms: no [[ ]], no <(), no (( )), no `local`, no arrays,
# no `set -o pipefail`. Validate with the commands:
#     dash -n install.sh
#     mksh -n install.sh
#     "shellcheck" -s sh install.sh
#
# One-liner:
#   curl -fsSL https://raw.githubusercontent.com/stormj-UH/jmake/main/install.sh | sh
#
# Default install:
#   $PREFIX/bin/jmake
#   $PREFIX/share/licenses/jmake/LICENSE   (if shipped in the .jpkg)
#   $PREFIX/share/man/manN/...             (if shipped in the .jpkg)
#
# Opt-in only ($PREFIX/bin/make symlink → jmake):
#   --make-default    (CLI), or answer "y" to the interactive prompt.
#
# This script never modifies /usr/bin/make or any other system binary.

# shellcheck disable=SC2016  # we intentionally print literal '$PATH' / '`jmake --version`' in user-facing text

set -eu

DEFAULT_VERSION="1.2.1"
DEFAULT_PREFIX="/usr/local"
URL_BASE="https://github.com/stormj-UH/jonerix/releases/download/packages"
ISSUES_URL="https://github.com/stormj-UH/jmake/issues"

VERSION="$DEFAULT_VERSION"
PREFIX="$DEFAULT_PREFIX"
ARCH=""
# MAKE_DEFAULT: 1 = yes, 0 = no, "" = ask interactively (only if a TTY is wired up).
MAKE_DEFAULT=""
# NO_PROMPT: 1 = never ask; "" = ask if stdin is a TTY.
NO_PROMPT=""

usage() {
	cat <<EOF
Usage: install.sh [options]

Default install (no flags): just \$PREFIX/bin/jmake (+ LICENSE / man pages
if the package ships them). The \$PREFIX/bin/make symlink is OPT-IN only.

Options:
  --version <VER>            jmake version to install (default: $DEFAULT_VERSION)
  --version=<VER>
  --prefix  <DIR>            install prefix (default: $DEFAULT_PREFIX)
  --prefix=<DIR>
  --arch    <ARCH>           override architecture (default: detected via uname -m)
  --arch=<ARCH>
  --make-default             also install \$PREFIX/bin/make as a symlink to jmake
                             (never touches /usr/bin/make)
  --no-make-default          do NOT install the make symlink (this is the default)
  --no-prompt                disable the interactive y/N prompt; assume "no" for
                             every opt-in not explicitly enabled by a flag
  --yes, -y                  disable prompts and assume "yes" for every opt-in
                             (currently: enables --make-default unless paired
                             with --no-make-default)
  --help, -h                 show this message and exit

Supported architectures: x86_64, aarch64
Required tools: curl or wget, zstd, tar, od, dd
EOF
}

err()  { printf '%s\n' "install.sh: $*" >&2; }
warn() { printf '%s\n' "install.sh: warning: $*" >&2; }
info() { printf '%s\n' "install.sh: $*" >&2; }
die()  { err "$*"; exit 1; }

# ---- arg parsing (POSIX) -----------------------------------------------------
while [ $# -gt 0 ]; do
	case "$1" in
		--version)            [ $# -ge 2 ] || die "--version requires an argument"; VERSION="$2"; shift 2 ;;
		--version=*)          VERSION="${1#--version=}"; shift ;;
		--prefix)             [ $# -ge 2 ] || die "--prefix requires an argument"; PREFIX="$2"; shift 2 ;;
		--prefix=*)           PREFIX="${1#--prefix=}"; shift ;;
		--arch)               [ $# -ge 2 ] || die "--arch requires an argument"; ARCH="$2"; shift 2 ;;
		--arch=*)             ARCH="${1#--arch=}"; shift ;;
		--make-default)       MAKE_DEFAULT=1; shift ;;
		--no-make-default)    MAKE_DEFAULT=0; shift ;;
		--no-prompt)          NO_PROMPT=1; shift ;;
		--yes|-y)             NO_PROMPT=1; [ -z "$MAKE_DEFAULT" ] && MAKE_DEFAULT=1; shift ;;
		--help|-h)            usage; exit 0 ;;
		--)                   shift; break ;;
		*)                    err "unknown option: $1"; usage >&2; exit 2 ;;
	esac
done

# ---- arch detection ----------------------------------------------------------
if [ -z "$ARCH" ]; then
	uname_m=$(uname -m)
	case "$uname_m" in
		x86_64|amd64)  ARCH="x86_64" ;;
		aarch64|arm64) ARCH="aarch64" ;;
		*) die "unsupported architecture: $uname_m (try --arch x86_64 or --arch aarch64)" ;;
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
info "downloading $URL"
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

# ---- destination layout (computed before install + before the prompt) -------
DEST_BIN="$PREFIX/bin"
DEST_LIC="$PREFIX/share/licenses/jmake"
DEST_MAN_BASE="$PREFIX/share/man"

# ---- find first 'make' on PATH (used by warnings + interactive prompt) ------
first_make_on_path() {
	IFS_SAVE=$IFS
	IFS=:
	for d in $PATH; do
		[ -z "$d" ] && continue
		if [ -x "$d/make" ]; then
			IFS=$IFS_SAVE
			printf '%s\n' "$d/make"
			return 0
		fi
	done
	IFS=$IFS_SAVE
	return 1
}
FIRST_MAKE=$(first_make_on_path 2>/dev/null || true)

# ---- interactive prompt (only if MAKE_DEFAULT is still unset) ---------------
# POSIX read into a single var is fine; -r is supported by every POSIX read.
if [ -z "$MAKE_DEFAULT" ]; then
	if [ "${NO_PROMPT:-0}" = 1 ]; then
		MAKE_DEFAULT=0
	elif [ -t 0 ] && [ -t 2 ]; then
		# Honest prompt: tell the user what they're agreeing to.
		printf '\n' >&2
		printf 'Symlink %s/make -> jmake?\n' "$DEST_BIN" >&2
		printf '  (Will be shadowed by /usr/bin/make if %s is later on $PATH.)\n' "$DEST_BIN" >&2
		printf '[y/N] ' >&2
		ans=""
		# Read from controlling terminal so this still works under
		#   curl ... | sh -s -- ...
		# where stdin is the pipe; if /dev/tty is unavailable, default to "no".
		if [ -r /dev/tty ]; then
			read -r ans </dev/tty || ans=""
		else
			MAKE_DEFAULT=0
		fi
		case "$ans" in
			y|Y|yes|YES|Yes) MAKE_DEFAULT=1 ;;
			*)               MAKE_DEFAULT=0 ;;
		esac
	else
		# No TTY, no flag, no --no-prompt: behave like --no-prompt (safe default).
		MAKE_DEFAULT=0
	fi
fi

# ---- install -----------------------------------------------------------------
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

# Primary binary.
run install -m 0755 "$EXTRACT/bin/jmake" "$DEST_BIN/jmake"

# License: try a few common locations from the payload.
license_src=""
for cand in \
	"$EXTRACT/share/licenses/jmake/LICENSE" \
	"$EXTRACT/LICENSE" \
	"$EXTRACT/usr/share/licenses/jmake/LICENSE"; do
	if [ -f "$cand" ]; then license_src="$cand"; break; fi
done
INSTALLED_LICENSE=0
if [ -n "$license_src" ]; then
	run install -m 0644 "$license_src" "$DEST_LIC/LICENSE"
	INSTALLED_LICENSE=1
fi

# Man pages: install anything under share/man/man{1..9} that the payload ships.
# We only handle the canonical sections so we never write outside man/manN/.
INSTALLED_MAN_COUNT=0
for sec in 1 2 3 4 5 6 7 8 9; do
	for src_dir in \
		"$EXTRACT/share/man/man$sec" \
		"$EXTRACT/usr/share/man/man$sec"; do
		[ -d "$src_dir" ] || continue
		# POSIX-safe globbing: enable nullglob-equivalent via a guard.
		# (No `shopt`; just check existence inside the loop.)
		for f in "$src_dir"/*; do
			[ -f "$f" ] || continue
			dest_dir="$DEST_MAN_BASE/man$sec"
			run mkdir -p "$dest_dir"
			run install -m 0644 "$f" "$dest_dir/"
			INSTALLED_MAN_COUNT=$((INSTALLED_MAN_COUNT + 1))
		done
	done
done

# ---- optional: install $PREFIX/bin/make symlink (opt-in only) ---------------
INSTALLED_MAKE_SYMLINK=0
if [ "$MAKE_DEFAULT" = 1 ]; then
	# Hard guard: never, under any circumstances, write to /usr/bin/make.
	if [ "$DEST_BIN" = "/usr/bin" ]; then
		warn "refusing to create /usr/bin/make: this script never modifies system binaries"
		warn "re-run with --prefix /usr/local (or another prefix) to enable --make-default"
	else
		run ln -sf jmake "$DEST_BIN/make"
		INSTALLED_MAKE_SYMLINK=1
	fi
fi

# ---- verify ------------------------------------------------------------------
got=$("$DEST_BIN/jmake" --version 2>/dev/null | head -n1 || true)
case "$got" in
	*"$VERSION"*) info "installed: $got" ;;
	*) die "verification failed: '$DEST_BIN/jmake --version' did not include $VERSION (got: $got)" ;;
esac

# ---- post-install trailer ----------------------------------------------------
PATH_OK=1
case ":$PATH:" in
	*":$DEST_BIN:"*) ;;
	*) PATH_OK=0 ;;
esac

printf '\n'
printf '=== jmake %s installed ===\n' "$VERSION"
printf '  bin:     %s/jmake\n' "$DEST_BIN"
if [ "$INSTALLED_LICENSE" = 1 ]; then
	printf '  license: %s/LICENSE\n' "$DEST_LIC"
fi
if [ "$INSTALLED_MAN_COUNT" -gt 0 ]; then
	printf '  man:     %s (%d page(s))\n' "$DEST_MAN_BASE" "$INSTALLED_MAN_COUNT"
fi
if [ "$INSTALLED_MAKE_SYMLINK" = 1 ]; then
	printf '  make:    %s/make -> jmake (opt-in)\n' "$DEST_BIN"
fi
printf '\n'

if [ "$PATH_OK" = 0 ]; then
	printf 'NOTE: %s is NOT on your $PATH.\n' "$DEST_BIN"
	printf '      Add it to your shell profile, e.g.:\n'
	printf '          export PATH="%s:$PATH"\n' "$DEST_BIN"
	printf '\n'
fi

if [ "$INSTALLED_MAKE_SYMLINK" = 1 ]; then
	# Loud warning if /usr/bin/make is ahead of $DEST_BIN on $PATH.
	if [ -n "$FIRST_MAKE" ] && [ "$FIRST_MAKE" = "/usr/bin/make" ] && [ "$DEST_BIN" != "/usr/bin" ]; then
		printf '!!! WARNING: /usr/bin/make is ahead of %s on $PATH.\n' "$DEST_BIN"
		printf '!!! Plain "make" will still invoke the system make, NOT jmake.\n'
		printf '!!! Reorder $PATH (put %s first) to make jmake the default.\n' "$DEST_BIN"
		printf '\n'
	fi
else
	printf 'TIP: %s/make was NOT created. To enable later, re-run:\n' "$DEST_BIN"
	printf '         sh install.sh --prefix %s --make-default --no-prompt\n' "$PREFIX"
	printf '     or symlink it yourself:\n'
	printf '         ln -sf jmake %s/make\n' "$DEST_BIN"
	printf '     (jmake never modifies /usr/bin/make.)\n'
	printf '\n'
fi

printf 'Compatibility note:\n'
printf '  jmake aims for GNU Make 4.4.1 compatibility; if you hit a missing feature,\n'
printf '  run `jmake --version` and report the version + the unsupported syntax to\n'
printf '  %s.\n' "$ISSUES_URL"
printf '\n'
printf 'jmake installed.\n'
