#!/usr/bin/env bash
# Pre-commit scope check.
#
# Lists every staged file so the author can sanity-check that the
# index matches the intended commit topic. Particularly useful in
# multi-task workflows where `git add` is easy to overshoot — e.g.
# concurrent edits leave unstaged changes from other workstreams,
# and a wide `git add` (or a tool that auto-stages) silently carries
# them into the next commit. The fix is to abort and `git reset HEAD
# <file>` the strays before re-committing.
#
# Advisory only — never blocks. The intent is to make scope obvious
# at commit time, not to enforce a particular file count.

set -e

staged=$(git diff --cached --name-only)
if [ -z "$staged" ]; then
    exit 0
fi

count=$(printf '%s\n' "$staged" | wc -l | tr -d ' ')

# Only surface the listing for non-trivial commits. A one-or-two-file
# commit is usually too small to overshoot.
if [ "$count" -le 2 ]; then
    exit 0
fi

# Group by top-level path so a cross-crate / cross-area commit
# surfaces visually.
echo "── pre-commit scope check ────────────────────────────────"
printf '%s\n' "$staged" | sed 's|^|  |'
echo ""
echo "  $count files staged across the index above."
echo "  If anything here doesn't belong to this commit's topic,"
echo "  abort now with Ctrl+C and run:"
echo "      git reset HEAD <file>"
echo "──────────────────────────────────────────────────────────"

exit 0
