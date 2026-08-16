#!/usr/bin/env bash
# Rebuild personal-site with a lost detached commit (prepare_workspace strips .git).
set -euo pipefail
cd "$(dirname "$0")"
ROOT="$(pwd)"
rm -rf personal-site
mkdir -p personal-site/_includes personal-site/_layouts
cd personal-site

git init -b master >/dev/null
git config user.email "test@example.com"
git config user.name "Test User"

cp "$ROOT/seed/old/about.md" _includes/about.md
cp "$ROOT/seed/old/default.html" _layouts/default.html
git add -A
git commit -m "Initial site" >/dev/null

git checkout --detach HEAD >/dev/null 2>&1
cp "$ROOT/seed/new/about.md" _includes/about.md
cp "$ROOT/seed/new/default.html" _layouts/default.html
git add -A
git commit -m "Move to Stanford" >/dev/null
git checkout master >/dev/null 2>&1

# Sanity: lost commit only in reflog
if grep -q "Postdoctoral" _includes/about.md 2>/dev/null; then
  echo "prepare_fixture: master should not already have recovered content" >&2
  exit 1
fi
if ! git reflog | grep -q "Move to Stanford"; then
  echo "prepare_fixture: missing reflog entry" >&2
  exit 1
fi
echo "prepare_fixture: personal-site ready (lost commit in reflog)" >&2