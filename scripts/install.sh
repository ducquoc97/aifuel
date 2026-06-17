#!/bin/sh
# Install (or remove) `aisub` — a global launcher for usage_monitor.py.
#
#   ./install.sh              # install `aisub` into a bin dir on your PATH
#   ./install.sh --uninstall  # remove it
#   BIN_DIR=/usr/local/bin ./install.sh   # override the target dir
#
# The launcher just forwards to this repo's usage_monitor.py, so:
#   aisub            -> python3 usage_monitor.py        (web dashboard)
#   aisub --json     -> python3 usage_monitor.py --json
#   aisub --text     -> ... and every other flag passes through.
set -eu

CMD=aisub

# Absolute path to the repo root (one level up from scripts/).
SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd -P)
TARGET_PY="$SCRIPT_DIR/../src/usage_monitor.py"

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
    echo "error: usage_monitor.py not found next to install.sh ($TARGET_PY)" >&2
    exit 1
fi

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
exec $PYTHON "$TARGET_PY" "\$@"
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
