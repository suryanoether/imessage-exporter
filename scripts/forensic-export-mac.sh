#!/usr/bin/env bash
#
# Build imessage-exporter on macOS and run an export with --forensic enabled.
#
# Usage:
#   scripts/forensic-export-mac.sh                              # HTML export to ~/imessage_export_forensic
#   scripts/forensic-export-mac.sh -f txt -o ~/some/other/path  # extra flags forwarded to the binary
#   scripts/forensic-export-mac.sh -t "+15558675309"            # filter to one conversation
#
# Re-runs cargo build first, then invokes the freshly-built binary with --forensic.
# Any arguments passed to this script are forwarded to imessage-exporter, so
# every flag in `imessage-exporter --help` is reachable.
#
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

# --- Sanity check: macOS only ----------------------------------------------
if [[ "$(uname -s)" != "Darwin" ]]; then
    echo "error: this script is for macOS; detected $(uname -s)." >&2
    exit 1
fi

# --- Rust toolchain --------------------------------------------------------
if ! command -v cargo >/dev/null 2>&1; then
    cat >&2 <<'EOF'
error: cargo is not installed.

Install Rust with:
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

Then re-open your terminal (or `source "$HOME/.cargo/env"`) and try again.
EOF
    exit 1
fi

# --- Build -----------------------------------------------------------------
echo "==> Building imessage-exporter (release) ..."
cargo build --release --package imessage-exporter

BIN="$REPO_ROOT/target/release/imessage-exporter"
if [[ ! -x "$BIN" ]]; then
    echo "error: build succeeded but binary not found at $BIN" >&2
    exit 1
fi

# --- Full Disk Access reminder --------------------------------------------
DB_PATH="$HOME/Library/Messages/chat.db"
if [[ ! -r "$DB_PATH" ]]; then
    cat >&2 <<EOF
warning: cannot read $DB_PATH

macOS requires Full Disk Access for the terminal app running this script.
Grant it under: System Settings -> Privacy & Security -> Full Disk Access,
then re-run this script.

Continuing anyway in case you're pointing at a custom --db-path ...
EOF
fi

# --- Defaults if caller passed no flags ------------------------------------
# The exporter requires --format. If the caller didn't pass one, default to
# HTML into ~/imessage_export_forensic. Pre-existing exports in that
# directory will cause the exporter to abort, so we rotate the path.
DEFAULT_OUT="$HOME/imessage_export_forensic"

if [[ $# -eq 0 ]]; then
    if [[ -d "$DEFAULT_OUT" && -n "$(ls -A "$DEFAULT_OUT" 2>/dev/null)" ]]; then
        TS="$(date +%Y%m%d-%H%M%S)"
        DEFAULT_OUT="${DEFAULT_OUT}-${TS}"
        echo "==> Default export path already populated; using $DEFAULT_OUT instead."
    fi
    set -- --format html --export-path "$DEFAULT_OUT"
fi

# --- Run -------------------------------------------------------------------
echo "==> Running: $BIN --forensic $*"
echo
exec "$BIN" --forensic "$@"
