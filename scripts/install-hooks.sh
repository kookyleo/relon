#!/usr/bin/env bash
# Install the repository's git hooks into .git/hooks/.
#
# Hooks themselves live under scripts/ so they're version-controlled
# and reviewable; this script symlinks them into the .git/hooks/
# directory where git looks for them.
#
# Re-run after a fresh clone:
#     ./scripts/install-hooks.sh

set -e

repo_root=$(git rev-parse --show-toplevel)
hooks_dir=$(git rev-parse --git-path hooks)

ln -sf "${repo_root}/scripts/pre-commit.sh" "${hooks_dir}/pre-commit"
chmod +x "${repo_root}/scripts/pre-commit.sh"

echo "Installed:"
echo "  ${hooks_dir}/pre-commit -> ${repo_root}/scripts/pre-commit.sh"
