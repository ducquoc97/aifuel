#!/bin/sh
# Install (or remove) `aifuel` — a global launcher for aifuel.py.
#
#   ./install.sh              # install `aifuel` into a bin dir on your PATH
#   ./install.sh --uninstall  # remove it
#   BIN_DIR=/usr/local/bin ./install.sh   # override the target dir
#
# The launcher just forwards to this repo's aifuel.py, so:
#   aifuel            -> python3 aifuel.py        (web dashboard)
#   aifuel --json     -> python3 aifuel.py --json
#   aifuel --text     -> ... and every other flag passes through.
set -eu

CMD=aifuel

# Absolute path to the repo root (one level up from scripts/).
SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd -P)
TARGET_PY="$SCRIPT_DIR/../src/aifuel.py"

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

if [ ! -f "$TARGET_PY" ]; then
    echo "error: aifuel.py not found next to install.sh ($TARGET_PY)" >&2
    exit 1
fi

# Canonicalize, then refuse a path we can't safely single-quote into the launcher
# (a literal single quote is the only char that can break out of single quotes).
TARGET_PY=$(CDPATH= cd -- "$SCRIPT_DIR/../src" && pwd -P)/aifuel.py
case $TARGET_PY in
    *\'*)
        echo "error: repo path contains a single quote; refusing to write launcher ($TARGET_PY)" >&2
        exit 1
        ;;
esac

# Find a Python 3 interpreter to bake into the launcher.
PYTHON=
for cand in python3 python; do
    if command -v "$cand" >/dev/null 2>&1; then
        PYTHON=$cand
        break
    fi
done
if [ -z "$PYTHON" ]; then
    echo "error: no python3 found on PATH" >&2
    exit 1
fi

mkdir -p "$BIN_DIR"
cat > "$LAUNCHER" <<EOF
#!/bin/sh
exec $PYTHON '$TARGET_PY' "\$@"
EOF
chmod +x "$LAUNCHER"

echo "Installed $CMD -> $TARGET_PY"
echo "  at $LAUNCHER (via $PYTHON)"

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
