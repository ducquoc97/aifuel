#!/bin/sh
# Install (or remove) the native Rust aifuel executable.
#
#   ./install.sh              # install `aifuel` into a bin dir on your PATH
#   ./install.sh --uninstall  # remove it
#   BIN_DIR=/usr/local/bin ./install.sh   # override the target dir
#
# The installer builds the Rust binary and copies it to the selected bin dir.
set -eu

CMD=aifuel

# Absolute path to the repo root (one level up from scripts/).
SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd -P)
REPO_ROOT=$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd -P)
TARGET_BIN="$REPO_ROOT/target/release/aifuel"

# Pick an install dir: explicit BIN_DIR, else ~/.local/bin (created if needed).
BIN_DIR=${BIN_DIR:-"$HOME/.local/bin"}
LAUNCHER="$BIN_DIR/$CMD"

if [ "${1:-}" = "--uninstall" ]; then
    if [ -e "$LAUNCHER" ]; then
        rm -f "$LAUNCHER"
        echo "Removed $LAUNCHER"
    else
        echo "Nothing to remove at $LAUNCHER"
    fi
    exit 0
fi

if ! command -v cargo >/dev/null 2>&1; then
    echo "error: cargo is required to build the Rust aifuel binary" >&2
    exit 1
fi

cargo build --release --locked -p aifuel
if [ ! -x "$TARGET_BIN" ]; then
    echo "error: Rust build did not produce an executable at $TARGET_BIN" >&2
    exit 1
fi

mkdir -p "$BIN_DIR"
TEMP_LAUNCHER="$LAUNCHER.tmp.$$"
cp "$TARGET_BIN" "$TEMP_LAUNCHER"
chmod +x "$TEMP_LAUNCHER"
mv "$TEMP_LAUNCHER" "$LAUNCHER"

echo "Installed $CMD -> $TARGET_BIN"
echo "  at $LAUNCHER"

# Warn if the install dir isn't on PATH, with a copy-paste fix.
case ":$PATH:" in
    *":$BIN_DIR:"*) ;;
    *)
        echo
        echo "warning: $BIN_DIR is not on your PATH. Add it, e.g.:"
        echo "  echo 'export PATH=\"$BIN_DIR:\$PATH\"' >> ~/.profile && . ~/.profile"
        ;;
esac

echo
echo "Try it:  $CMD --text"
