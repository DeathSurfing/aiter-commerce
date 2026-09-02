#!/usr/bin/env bash
# Install (or uninstall) the aiter-commerce skill for opencode.
#
# Copies .opencode/skills/aiter-commerce (SKILL.md) into opencode's global
# skills directory (~/.config/opencode/skills/), making the skill available
# in every project. The project-local copy under .opencode/ is already loaded
# when working inside this repo — this script is for global availability.
#
# Usage:
#   bash scripts/skills.sh              # install (idempotent)
#   bash scripts/skills.sh --uninstall  # remove
#   bash scripts/skills.sh --dest DIR   # install to a custom directory
#
# opencode loads skills at startup: quit and restart opencode after running.
set -euo pipefail
cd "$(dirname "$0")/.."

SKILL_SRC=".opencode/skills/aiter-commerce"
DEFAULT_DEST="${XDG_CONFIG_HOME:-$HOME/.config}/opencode/skills"

usage() {
    sed -n '2,15p' "$0" | sed 's/^# \{0,1\}//'
    exit "${1:-0}"
}

install() {
    local dest="$1"
    if [[ ! -d "$SKILL_SRC" ]]; then
        echo "error: skill not found at $SKILL_SRC" >&2
        exit 1
    fi
    mkdir -p "$dest"
    rm -rf "$dest/aiter-commerce"
    cp -R "$SKILL_SRC" "$dest/"
    echo "installed aiter-commerce skill -> $dest/aiter-commerce"
    echo "restart opencode for the skill to load."
}

uninstall() {
    local dest="$1"
    rm -rf "$dest/aiter-commerce"
    echo "removed $dest/aiter-commerce (if it existed)."
}

dest="$DEFAULT_DEST"
action="install"
while [[ $# -gt 0 ]]; do
    case "$1" in
        --uninstall | -u) action="uninstall" ;;
        --dest) dest="${2:?--dest requires a directory}"; shift ;;
        --help | -h) usage 0 ;;
        *) echo "unknown flag: $1" >&2; usage 2 ;;
    esac
    shift
done

case "$action" in
    install) install "$dest" ;;
    uninstall) uninstall "$dest" ;;
esac