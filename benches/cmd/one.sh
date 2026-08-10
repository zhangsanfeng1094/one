# shellcheck shell=bash
# One agent full-eval: full / fix-all
#
# Eval path uses AgentSpec harness:
#   one run --spec <out/agent.spec.json> -p <prompt> --cwd <workspace>
# Agent template: benches/agents/bench.json (or tasks/<id>/agent.json).
# max_turns comes from rubric.json (type: max_turns), injected into the spec.
#
# Default is TUI (supervise with the main interactive agent). Use --headless for
# the AgentSpec harness (`one run --spec`) — preferred for CI / true eval.

cmd_full() {
  local prc=0
  set +e
  parse_agent_args "$@"
  prc=$?
  set -e
  if [[ $prc -eq 2 ]]; then
    cmd_help
    return 0
  fi
  if [[ $prc -eq 3 ]]; then
    # --list already printed table
    return 0
  fi
  if [[ $prc -ne 0 ]]; then
    return "$prc"
  fi

  local task="$ARG_TASK"
  echo "selected task: $task"
  echo ""
  local tdir
  tdir="$(task_dir "$task")"
  load_task_meta "$tdir"

  local label="settings"
  if [[ -n "$ARG_PROVIDER" ]]; then
    label="$ARG_PROVIDER"
  fi
  if [[ "$ARG_UI" == "headless" ]]; then
    label="${label}-headless"
  elif [[ "$ARG_UI" == "tui" ]]; then
    label="${label}-tui"
  fi

  local out
  out="$(make_out "${task}-${label}")"
  local ws fixture_src
  fixture_src="$(resolve_fixture_src "$tdir")"
  ws="$(prepare_workspace "$out" "$tdir")"
  copy_task_meta "$tdir" "$out"

  local criterion
  criterion="$(task_success_criterion "$tdir")"

  local max_turns
  max_turns="$(task_max_turns "$tdir")"

  # Task body + shared harness constraints (anti-oracle / workspace-only).
  local prompt
  prompt="$(compose_agent_prompt "$tdir")"

  # Keep prompt easy to re-open / paste if the TUI first turn is aborted.
  # Write the *composed* prompt (constraints included) so resume/debug matches the run.
  printf '%s' "$prompt" >"$out/prompt.md"
  printf '%s' "$prompt" >"$ws/BENCH_PROMPT.md"
  cp "$tdir/prompt.md" "$out/prompt.task.md" 2>/dev/null || true

  # Materialize AgentSpec (rubric max_turns + optional task agent.json).
  local agent_spec
  agent_spec="$(materialize_bench_agent "$tdir" "$out" "$max_turns" "${ARG_MODEL:-}")"
  echo "agent:    $agent_spec  (max_turns=$max_turns)"

  # TTY check: TUI needs a real terminal.
  if [[ "$ARG_UI" == "tui" ]]; then
    if [[ ! -t 0 || ! -t 1 ]]; then
      echo "warning: stdin/stdout is not a TTY — falling back to headless AgentSpec harness" >&2
      ARG_UI="headless"
    fi
  fi

  echo "=== one · full ==="
  echo "task:     $task"
  echo "ui:       $ARG_UI"
  if [[ -n "$TASK_META_PROJECT" ]]; then
    echo "project:  $TASK_META_PROJECT"
  elif [[ -n "$fixture_src" ]]; then
    echo "fixture:  $fixture_src"
  else
    echo "fixture:  (empty workspace)"
  fi
  if [[ -n "$ARG_PROVIDER" ]]; then
    echo "provider: $ARG_PROVIDER  (override)"
  else
    echo "provider: (settings)"
  fi
  if [[ -n "$ARG_MODEL" ]]; then
    echo "model:    $ARG_MODEL  (override)"
  else
    echo "model:    (settings / agent inherit)"
  fi
  show_tui_settings || true
  echo "cwd:      $ws"
  echo "score:    $criterion"
  echo "max_turns:$max_turns  (from rubric / ONE_BENCH_MAX_TURNS)"
  if cf="$(bench_constraints_file 2>/dev/null)"; then
    echo "prompt:   task + constraints ($cf)"
  else
    echo "prompt:   task only (constraints disabled)"
  fi
  if [[ -n "${LANGFUSE_PUBLIC_KEY:-}" && -n "${LANGFUSE_SECRET_KEY:-}" ]]; then
    echo "trace:    langfuse (${LANGFUSE_BASE_URL:-${LANGFUSE_HOST:-https://cloud.langfuse.com}})"
  else
    echo "trace:    (no LANGFUSE_* keys — skip export; pass --trace only with keys)"
  fi
  echo "out:      $out"
  echo ""

  if [[ -f "$ws/Cargo.toml" ]]; then
    require_rust
  fi
  local sf
  sf="$(settings_file)"
  if [[ -z "$ARG_PROVIDER" ]]; then
    if [[ ! -f "$sf" ]] || ! grep -q '"provider"' "$sf" 2>/dev/null; then
      echo "warning: no --provider and no provider in $sf → mock likely" >&2
    fi
  fi

  if [[ -f "$ws/Cargo.toml" ]]; then
    echo "=== stock cargo test (expect FAIL for broken fixtures) ==="
    expect_stock_fail "$ws" || true
    echo ""
  fi

  # Global CLI flags must precede the `run` subcommand.
  local -a global_args=()
  local -a run_args=()
  local agent_mode="harness"
  local run_result_file="$out/agent-result.json"

  global_args+=(--cwd "$ws")
  if [[ "$ARG_YES" -eq 1 ]]; then
    global_args+=(-y)
  fi
  if [[ -n "${LANGFUSE_PUBLIC_KEY:-}" && -n "${LANGFUSE_SECRET_KEY:-}" ]]; then
    global_args+=(--trace)
  fi
  if [[ -n "$ARG_PROVIDER" ]]; then
    global_args+=(--provider "$ARG_PROVIDER")
  fi
  if [[ -n "$ARG_MODEL" ]]; then
    global_args+=(--model "$ARG_MODEL")
  fi

  if [[ "$ARG_UI" == "tui" ]]; then
    # Legacy supervise path: main interactive agent (not AgentSpec / one run).
    agent_mode="tui-main"
    echo "warning: --tui uses the main interactive agent, not AgentSpec harness." >&2
    echo "warning: for true eval use --headless (one run --spec)." >&2
    echo ""
    run_args=(
      --no-mcp
      --no-skills
      --no-memory
      --no-subagent
      --max-turns "$max_turns"
      -n "bench:${task}"
      -p "$prompt"
      --tui
    )
    echo "=== agent run (TUI main · not AgentSpec) ==="
    echo "  · first turn = task prompt (auto-started)"
    echo "  · max_turns=$max_turns (from rubric)"
    echo "  · workspace: $ws"
    echo "  · after quit, rubric scoring runs automatically"
    echo ""
  else
    # True eval: AgentSpec harness → RunResult JSON on stdout.
    run_args=(
      run
      --spec "$agent_spec"
      --output-format json
      --isolation none
      -p "$prompt"
    )
    echo "=== agent run (AgentSpec harness · one run --spec) ==="
    echo "  · spec: $agent_spec"
    echo "  · max_turns=$max_turns"
    echo "  · RunResult → $run_result_file"
    echo "  · workspace: $ws"
    echo ""
  fi

  local agent_ec=0
  local agent_status=""
  set +e
  if [[ "$agent_mode" == "harness" ]]; then
    # Capture RunResult JSON on stdout; progress/trace on stderr.
    run_one "${global_args[@]}" "${run_args[@]}" \
      >"$out/agent.stdout.log" 2>"$out/agent.stderr.log"
    agent_ec=$?
    # Prefer last non-empty JSON line.
    if [[ -s "$out/agent.stdout.log" ]]; then
      tail -n 1 "$out/agent.stdout.log" >"$run_result_file"
    else
      : >"$run_result_file"
    fi
    # Surface stderr (trace lines, errors) to the console.
    if [[ -s "$out/agent.stderr.log" ]]; then
      cat "$out/agent.stderr.log" >&2 || true
    fi
    if [[ -s "$run_result_file" ]]; then
      agent_status="$(python3 -c 'import json,sys; d=json.load(open(sys.argv[1])); print(d.get("status") or d.get("stop_reason") or ("ok" if d.get("ok") else "fail"))' "$run_result_file" 2>/dev/null || echo "?")"
      echo "RunResult status: $agent_status  (exit $agent_ec)" >&2
      python3 - "$run_result_file" <<'PY' 2>/dev/null || true
import json, sys
d = json.load(open(sys.argv[1]))
text = (d.get("result") or "").strip()
if text:
    print("--- agent result (truncated) ---", file=sys.stderr)
    print(text[:2000] + ("…" if len(text) > 2000 else ""), file=sys.stderr)
usage = d.get("usage") or {}
print(
    f"turns={d.get('turns')}  tokens_in={usage.get('input_tokens')}  "
    f"tokens_out={usage.get('output_tokens')}  duration_ms={d.get('duration_ms')}",
    file=sys.stderr,
)
PY
    else
      echo "warning: empty RunResult; see $out/agent.stdout.log / agent.stderr.log" >&2
    fi
  else
    run_one "${global_args[@]}" "${run_args[@]}"
    agent_ec=$?
  fi
  set -e
  echo ""
  echo "agent exit: $agent_ec"
  echo ""

  # run.json — launch + agent envelope for offline analysis
  ONE_BENCH_OUT="$out" \
  ONE_BENCH_TASK="$task" \
  ONE_BENCH_AGENT_MODE="$agent_mode" \
  ONE_BENCH_AGENT_SPEC="$agent_spec" \
  ONE_BENCH_MAX_TURNS="$max_turns" \
  ONE_BENCH_UI="$ARG_UI" \
  ONE_BENCH_YES="$ARG_YES" \
  ONE_BENCH_PROVIDER="${ARG_PROVIDER:-}" \
  ONE_BENCH_MODEL="${ARG_MODEL:-}" \
  ONE_BENCH_WS="$ws" \
  ONE_BENCH_AGENT_EC="$agent_ec" \
  ONE_BENCH_CRITERION="$criterion" \
  python3 <<'PY'
import json, os, time
from pathlib import Path
out = Path(os.environ["ONE_BENCH_OUT"])
rr = {}
rr_path = out / "agent-result.json"
if rr_path.is_file() and rr_path.stat().st_size > 0:
    try:
        rr = json.loads(rr_path.read_text())
    except Exception as e:
        rr = {"parse_error": str(e)}
doc = {
    "task": os.environ["ONE_BENCH_TASK"],
    "agent_mode": os.environ["ONE_BENCH_AGENT_MODE"],
    "agent_spec": os.environ["ONE_BENCH_AGENT_SPEC"],
    "max_turns": int(os.environ["ONE_BENCH_MAX_TURNS"]),
    "ui": os.environ["ONE_BENCH_UI"],
    "auto_approve": os.environ.get("ONE_BENCH_YES") == "1",
    "provider": os.environ.get("ONE_BENCH_PROVIDER") or None,
    "model": os.environ.get("ONE_BENCH_MODEL") or None,
    "workspace": os.environ["ONE_BENCH_WS"],
    "agent_exit": int(os.environ["ONE_BENCH_AGENT_EC"]),
    "score_cmd": os.environ.get("ONE_BENCH_CRITERION") or None,
    "agent_result": rr,
    "written_at": time.strftime("%Y-%m-%dT%H:%M:%S%z"),
}
(out / "run.json").write_text(json.dumps(doc, indent=2) + "\n")
print(f"wrote {out / 'run.json'}", flush=True)
PY

  echo "=== score (rubric / test_filter) ==="
  local result
  result="$(score_task "$ws" "$out" "$tdir")"
  if [[ "$result" == "pass" ]]; then
    echo "SCORE: PASS"
  else
    echo "SCORE: FAIL — see $out/score.log"
  fi

  # Patch run.json with score
  python3 - "$out/run.json" "$result" <<'PY'
import json, sys
from pathlib import Path
p = Path(sys.argv[1])
d = json.loads(p.read_text()) if p.is_file() else {}
d["score_result"] = sys.argv[2]
p.write_text(json.dumps(d, indent=2) + "\n")
PY

  local trace_line="(none)"
  if [[ -n "${LANGFUSE_PUBLIC_KEY:-}" && -n "${LANGFUSE_SECRET_KEY:-}" ]]; then
    trace_line="langfuse (${LANGFUSE_BASE_URL:-${LANGFUSE_HOST:-https://cloud.langfuse.com}})"
  fi

  cat >"$out/summary.md" <<EOF
# $task · one · ${ARG_PROVIDER:-settings} · ui=$ARG_UI

- result: $result
- agent exit: $agent_ec
- agent mode: $agent_mode
- agent spec: \`$agent_spec\`
- max_turns: $max_turns
- ui: $ARG_UI
- auto_approve: $ARG_YES
- provider: ${ARG_PROVIDER:-(settings)}
- model: ${ARG_MODEL:-(settings)}
- project: ${TASK_META_PROJECT:-(local fixture)}
- score cmd: \`$criterion\`
- workspace: \`$ws\`
- prompt: \`$out/prompt.md\`
- run: \`$out/run.json\`
- agent result: \`$out/agent-result.json\`
- trace: $trace_line
- score log: \`$out/score.log\`
EOF

  echo ""
  echo "=== done ==="
  echo "summary:   $out/summary.md"
  echo "run.json:  $out/run.json"
  echo "workspace: $ws"
  echo "result:    $result"
  [[ "$result" == "pass" ]]
}

cmd_fix_all() {
  # Explicit target — no picker
  cmd_full kit-fix-all "$@"
}
