#!/usr/bin/env bash
# Install Ironjaw pre-commit hooks into .git/hooks/
set -e

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
HOOKS_DIR="$REPO_ROOT/.git/hooks"

# Handle worktree case where .git is a file pointing to the main repo
if [ -f "$REPO_ROOT/.git" ]; then
    GIT_DIR=$(cat "$REPO_ROOT/.git" | sed 's/gitdir: //')
    HOOKS_DIR="$GIT_DIR/hooks"
fi

mkdir -p "$HOOKS_DIR"
cp "$SCRIPT_DIR/pre-commit" "$HOOKS_DIR/pre-commit"
chmod +x "$HOOKS_DIR/pre-commit"

echo "Pre-commit hook installed to $HOOKS_DIR/pre-commit"
