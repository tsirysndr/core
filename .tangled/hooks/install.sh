#!/usr/bin/env bash
#
# install.sh — enable the repo's git hooks for your local clone.
#
# This points git's core.hooksPath at .tangled/hooks so the version-controlled
# hooks in this directory run. One command, nothing copied, updates travel with
# the repo.
#
#   ./.tangled/hooks/install.sh
#
# To undo:
#   git config --unset core.hooksPath

set -euo pipefail

# Resolve the repo root regardless of where this is invoked from.
root="$(git rev-parse --show-toplevel)"
cd "$root"

hooks_dir=".tangled/hooks"

git config core.hooksPath "$hooks_dir"
chmod +x "$hooks_dir"/commit-msg "$hooks_dir"/commit-msg-lint.sh 2>/dev/null || true

echo "Installed git hooks: core.hooksPath -> $hooks_dir"
echo
echo "Note: jj (jujutsu) does not run git hooks. jj users are covered by the"
echo "commit-lint CI workflow; lint locally on demand with:"
echo "  .tangled/hooks/commit-msg-lint.sh --rev @"
