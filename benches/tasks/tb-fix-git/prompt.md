# Fix Git (Terminal-Bench 2.0)

You just made some changes to a personal site and checked out `master`, but now you can't find those changes.

Help find them and **merge them into `master`**.

Workspace:

- `personal-site/` — git repository (work here; start from its root)
- The lost edits touched `_includes/about.md` and `_layouts/default.html`

Hints that match real git recovery (without spoiling the exact commit hash):

- Look at `git reflog` / `.git/logs/HEAD` for commits no longer on any branch
- Create a recovery branch, then merge into `master`

Scoring: `python3 verify.py` compares those two files to the expected recovered content.
