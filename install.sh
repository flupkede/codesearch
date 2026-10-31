#!/usr/bin/env bash
# codesearch installer/updater for Linux and macOS.
#
# First install: asks for the install path (default ~/.local/bin).
# Existing install (marker ~/.codesearch/install-path or codesearch on PATH):
# stops the running serve from that install, updates in place, never asks.
#
# Download-then-run: verifies the release SHA256 before installing.
#
# Usage: install.sh [--destination DIR] [--no-csharp] [--tag TAG] [--dry-run]
set -euo pipefail

REPO='flupkede/codesearch'
MARKER="$HOME/.codesearch/install-path"
DESTINATION=''
NO_CSHARP=0
TAG=''
DRY_RUN=0

while [[ $# -gt 0 ]]; do
    case "$1" in
        --destination) DESTINATION="$2"; shift 2 ;;
        --no-csharp) NO_CSHARP=1; shift ;;
        --tag) TAG="$2"; shift 2 ;;
        --dry-run) DRY_RUN=1; shift ;;
        -h|--help) grep '^#' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) echo "Unknown option: $1" >&2; exit 2 ;;
    esac
done

sha256_of() { # portable sha256
    if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1" | cut -d' ' -f1
    else shasum -a 256 "$1" | cut -d' ' -f1; fi
}

resolve_platform() { # sets ARTIFACT; exits when no build exists
    local os arch
    os="$(uname -s)"; arch="$(uname -m)"
    if [[ "$os" == Linux && "$arch" == x86_64 ]]; then
        ARTIFACT='codesearch-linux-x86_64'
    elif [[ "$os" == Darwin && ( "$arch" == arm64 || "$arch" == aarch64 ) ]]; then
        ARTIFACT='codesearch-macos-arm64'
    else
        echo "No codesearch release build for $os/$arch." >&2
        exit 1
    fi
    [[ $NO_CSHARP -eq 1 ]] || ARTIFACT="$ARTIFACT-with-csharp"
    ARTIFACT="$ARTIFACT.tar.gz"
}

existing_install_dir() {
    if [[ -f $MARKER ]]; then
        # Trim ends only: tr -d '[:space:]' would delete INTERIOR spaces and
        # break the round-trip for destinations like "/Users/j/my tools/bin".
        local p; p="$(sed 's/^[[:space:]]*//;s/[[:space:]]*$//' <"$MARKER")"
        if [[ -n $p && -x "$p/codesearch" ]]; then printf '%s' "$p"; return 0; fi
    fi
    local c; c="$(command -v codesearch 2>/dev/null || true)"
    if [[ -n $c ]]; then dirname "$c"; return 0; fi
    return 1
}

stop_install_serve() { # stop only serve processes running from OUR install dir
    local dir="$1" pids p exe
    pids="$(pgrep -x codesearch 2>/dev/null || true)"
    [[ -n $pids ]] || return 0
    for p in $pids; do
        if [[ -r "/proc/$p/exe" ]]; then
            exe="$(readlink -f "/proc/$p/exe" 2>/dev/null || true)"
        else
            # Ends-only trim (padding safety) — tr -d ' ' would delete the
            # INTERIOR spaces of a full comm path like "/Users/j/my tools/bin"
            # and silently no-op the scoped stop.
            exe="$(ps -o comm= -p "$p" 2>/dev/null | sed 's/^[[:space:]]*//;s/[[:space:]]*$//' || true)"
            [[ $exe == */codesearch ]] || exe=""
        fi
        if [[ $exe == "$dir/codesearch" ]]; then
            echo "Stopping running codesearch (pid $p) from $dir ..."
            kill "$p" 2>/dev/null || true
        fi
    done
    sleep 2
    for p in $pids; do
        kill -0 "$p" 2>/dev/null || continue
        exe="$(readlink -f "/proc/$p/exe" 2>/dev/null || true)"
        if [[ $exe == "$dir/codesearch" ]]; then kill -9 "$p" 2>/dev/null || true; fi
    done
}

echo '== codesearch installer (Linux/macOS) =='
if [[ -z $TAG ]]; then
    TAG="$(curl -fsSL "https://api.github.com/repos/$REPO/releases/latest" | sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p')"
fi
resolve_platform
BASE="https://github.com/$REPO/releases/download/$TAG"

if DIR="$(existing_install_dir)"; then
    echo "Existing install found: $DIR — updating in place."
    if [[ $DRY_RUN -eq 1 ]]; then
        echo "(dry-run) would stop serve from $DIR if running"
    else
        stop_install_serve "$DIR"
    fi
else
    if [[ -n $DESTINATION ]]; then
        DIR="$DESTINATION"
    else
    default_dir="$HOME/.local/bin"
    # Read from the terminal, not stdin: under `curl ... | bash` the script
    # parser shares stdin with read, so an unprefixed read would eat the
    # installer's own source lines as the "answer".
    if read -r -p "Install path [$default_dir]: " answer < /dev/tty 2>/dev/null; then
        DIR="${answer:-$default_dir}"
    else
        DIR="$default_dir"
    fi
    fi
    echo "Fresh install into: $DIR"
fi

if [[ $DRY_RUN -eq 1 ]]; then
    echo "(dry-run) would install $ARTIFACT from $TAG into $DIR"
    exit 0
fi

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
echo "Downloading $ARTIFACT ($TAG) ..."
curl -fL "$BASE/$ARTIFACT" -o "$tmp/$ARTIFACT"
if curl -fsSL "$BASE/$ARTIFACT.sha256" -o "$tmp/$ARTIFACT.sha256"; then
    expected="$(cut -d' ' -f1 <"$tmp/$ARTIFACT.sha256")"
    actual="$(sha256_of "$tmp/$ARTIFACT")"
    if [[ $actual != "$expected" ]]; then
        echo "SHA256 mismatch for $ARTIFACT (expected $expected, got $actual)" >&2
        exit 1
    fi
    echo 'SHA256 verified.'
else
    echo 'WARNING: no checksum published for this release — skipping verification.' >&2
fi

echo 'Extracting ...'
tar xzf "$tmp/$ARTIFACT" -C "$tmp"
mkdir -p "$DIR"
mv -f "$tmp/codesearch" "$DIR/codesearch"
if [[ -f $tmp/helpers/csharp/scip-csharp ]]; then
    mv -f "$tmp/helpers/csharp/scip-csharp" "$DIR/scip-csharp"
fi
mkdir -p "$(dirname "$MARKER")"
printf '%s' "$DIR" >"$MARKER"
chmod +x "$DIR/codesearch"
[[ ! -f $DIR/scip-csharp ]] || chmod +x "$DIR/scip-csharp"

echo "Installed into $DIR"
"$DIR/codesearch" --version || true
case ":$PATH:" in
    *":$DIR:"*) ;;
    *) echo "NOTE: $DIR is not on your PATH. Add it with:" >&2
       echo "  echo 'export PATH=\"$DIR:\$PATH\"' >> ~/.bashrc" >&2 ;;
esac
