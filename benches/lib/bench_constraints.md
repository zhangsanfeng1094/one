## Harness evaluation rules (mandatory)

You are scored only on the **workspace cwd** (files you leave there + the task’s verify/check command). Treat this as a closed exam.

### Stay in the workspace
- Work only under the current workspace directory.
- Do **not** read, list, or `cat` paths outside it: parent trees, the repo `benches/tasks/…`, other projects, home agent dirs used as solution dumps.
- Especially forbidden outside workspace: `SOLUTIONS.md`, `SOURCE.md`, `solution/`, `solve.sh`, oracle blobs, reference `re.json` / golden outputs, other agents’ workspaces under `/tmp` or `benches/out/`.
- If a path tool denies “outside workspace”, **do not** bypass with `bash`/`find`/`curl` to the same content.

### No oracle / no solution download
- Do **not** fetch official or third-party **solutions** (e.g. Terminal-Bench / harbor `solution/solve.sh`, base64/gzip solution payloads, gist dumps of the answer file).
- Do **not** search the web or GitHub for “the answer” / “solve.sh” / full solution code for this task.
- Public docs for **languages, libraries, APIs, algorithms** are OK. Copying the task’s packaged answer is not.

### Produce the solution yourself
- Implement or generate the required artifacts in the workspace (edit/write/codegen).
- Self-test with tools in the workspace (`check.py`, unit tests, small scripts). Prefer that over hunting external keys.
- Do **not** weaken, delete, or no-op `verify.py` / tests / checkers to force a pass.

### Dependencies & long runs
- Install runtime deps only if needed for self-test (e.g. `pip install --target ./.pydeps …` or `PIP_CACHE_DIR=/tmp/pipcache` under sandbox limits). Prefer **workspace-local** install paths so scoring can use them when documented by the task.
- For long commands, set an explicit high `timeout_secs` (or equivalent) rather than relying on the 120s default; re-run with a larger timeout if you hit a wall-clock kill.
- Network for package indexes is OK when required for deps. Network for downloading the **answer artifact** is not.

### What “done” means
- Leave the required deliverables in the workspace.
- Run the task’s self-check when practical and fix failures you can.
- Final reply: brief summary of what you changed and how you verified — not a paste of external solution provenance.
