#!/bin/sh
# Keeps Claude Code's state (~/.claude and ~/.claude.json) in the project's
# git-ignored .claude-home folder so it survives a devcontainer rebuild.
# Runs on every container start; safe to run by hand at any time.
set -eu

store="$(cd "$(dirname "$0")/.." && pwd)/.claude-home"
link="$HOME/.claude"
config="$HOME/.claude.json"

mkdir -p "$store"
chmod 700 "$store"

if [ "$(readlink "$link" 2>/dev/null || true)" != "$store" ]; then
    if [ -e "$link" ] || [ -L "$link" ]; then
        # A fresh container may already have a ~/.claude. Keep anything the
        # store lacks, then set the original aside instead of deleting it.
        [ -d "$link" ] && cp -an "$link/." "$store/"
        mv "$link" "$link.before-link.$(date +%s)"
    fi
    ln -s "$store" "$link"
    echo "Linked $link -> $store"
fi

# ~/.claude.json is rewritten in place by Claude Code, so it cannot be a
# symlink. Restore it after a rebuild, otherwise refresh the stored copy.
if [ ! -e "$config" ] && [ -e "$store/claude.json" ]; then
    cp -p "$store/claude.json" "$config"
    echo "Restored $config"
elif [ -e "$config" ]; then
    cp -p "$config" "$store/claude.json"
fi
