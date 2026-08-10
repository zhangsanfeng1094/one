# shellcheck shell=bash
# Grok Build CLI eval (default TUI supervise; --headless for CI / streaming-json)
#
# TUI (default):
#   grok --cwd <ws> --always-approve --max-turns N "<prompt>"
#
# Headless (true eval / CI):
#   grok --cwd <ws> --prompt-file <prompt> --always-approve \
#        --output-format streaming-json --max-turns N
#
# Env:
#   GROK_BIN          binary (default: grok)
#   GROK_ARGS         extra flags (word-split)
#   GROK_OUTPUT_FORMAT  streaming-json | json | plain  (default: streaming-json)
#   ONE_BENCH_UI      tui|headless (default tui, same as One)

cmd_grok() {
  local prc=0
  set +e
  parse_agent_args "$@"
  prc=$?
  set -e
  if [[ $prc -eq 2 ]]; then cmd_help; return 0; fi
  if [[ $prc -eq 3 ]]; then return 0; fi
  if [[ $prc -ne 0 ]]; then return "$prc"; fi

  local task="$ARG_TASK"
  echo "selected task: $task"
  echo ""
  local tdir
  tdir="$(task_dir "$task")"
  load_task_meta "$tdir"

  local grok_bin="${GROK_BIN:-grok}"
  if ! command -v "$grok_bin" >/dev/null 2>&1; then
    cat <<EOF >&2
error: grok not found ($grok_bin)

  # install Grok Build CLI, then auth:
  grok login
  # or CI:
  export XAI_API_KEY=xai-...
EOF
    exit 2
  fi

  local label="grok"
  if [[ -n "$ARG_MODEL" ]]; then
    label="grok-${ARG_MODEL//\//_}"
  fi
  if [[ "$ARG_UI" == "tui" ]]; then
    label="${label}-tui"
  else
    label="${label}-headless"
  fi

  local out
  out="$(make_out "${task}-${label}")"
  local ws fixture_src
  fixture_src="$(resolve_fixture_src "$tdir")"
  ws="$(prepare_workspace "$out" "$tdir")"
  copy_task_meta "$tdir" "$out"
  # Own .git so Grok project-root discovery does not walk up to the One monorepo.
  ensure_git_repo "$ws"

  local criterion
  criterion="$(task_success_criterion "$tdir")"
  local max_turns
  max_turns="$(task_max_turns "$tdir")"

  local prompt
  prompt="$(compose_agent_prompt "$tdir" "Success criterion: in this directory, make the following command(s) succeed:
  $criterion
Do not delete or weaken tests. Prefer minimal correct fixes.")"
  printf '%s' "$prompt" >"$out/prompt.md"
  printf '%s' "$prompt" >"$ws/BENCH_PROMPT.md" 2>/dev/null || true
  cp "$tdir/prompt.md" "$out/prompt.task.md" 2>/dev/null || true

  local events_file="$out/grok-events.jsonl"
  local result_json="$out/grok-result.json"
  local progress_file="$out/grok-progress.log"
  local last_msg="$out/grok-last-message.txt"
  local fmt="${GROK_OUTPUT_FORMAT:-streaming-json}"

  echo "=== grok ==="
  echo "task:     $task"
  echo "ui:       $ARG_UI"
  if [[ -n "$TASK_META_PROJECT" ]]; then
    echo "project:  $TASK_META_PROJECT"
  elif [[ -n "$fixture_src" ]]; then
    echo "fixture:  $fixture_src"
  else
    echo "fixture:  (empty workspace)"
  fi
  echo "bin:      $grok_bin  ($("$grok_bin" --version 2>/dev/null | head -1 || echo '?'))"
  if [[ -n "$ARG_MODEL" ]]; then
    echo "model:    $ARG_MODEL"
  else
    echo "model:    (grok default / config)"
  fi
  echo "cwd:      $ws"
  echo "score:    $criterion"
  echo "max_turns:$max_turns"
  echo "events:   $events_file"
  echo "out:      $out"
  echo ""

  if [[ -f "$ws/Cargo.toml" ]]; then
    require_rust
    echo "=== stock cargo test (expect FAIL for broken fixtures) ==="
    expect_stock_fail "$ws" || true
    echo ""
  fi

  # TTY check for TUI path
  if [[ "$ARG_UI" == "tui" ]]; then
    if [[ ! -t 0 || ! -t 1 ]]; then
      echo "warning: stdin/stdout is not a TTY — falling back to headless" >&2
      ARG_UI="headless"
    fi
  fi

  local -a grok_cmd=("$grok_bin" --cwd "$ws")
  if [[ -n "$ARG_MODEL" ]]; then
    grok_cmd+=(--model "$ARG_MODEL")
  fi
  if [[ "$ARG_YES" -eq 1 ]]; then
    grok_cmd+=(--always-approve)
  fi

  if [[ -n "${GROK_ARGS:-}" ]]; then
    # shellcheck disable=SC2206
    grok_cmd+=($GROK_ARGS)
  fi

  local agent_ec=0
  local start_ms end_ms wall_ms
  start_ms="$(now_ms)"

  if [[ "$ARG_UI" == "tui" ]]; then
    # Interactive supervise: first message = task prompt; /quit then score.
    grok_cmd+=(--max-turns "$max_turns" "$prompt")
    echo "=== grok run (TUI) ==="
    echo "+ ${grok_cmd[*]}" >&2
    echo "  · first turn = task prompt"
    echo "  · after quit, rubric scoring runs automatically"
    echo ""
    set +e
    "${grok_cmd[@]}"
    agent_ec=$?
    set -e
    : >"$events_file"
    : >"$progress_file"
    echo "(tui — no streaming-json capture)" >"$last_msg"
  else
    # Headless eval: prompt file + structured stream on stdout.
    grok_cmd+=(
      --prompt-file "$out/prompt.md"
      --output-format "$fmt"
      --max-turns "$max_turns"
      --no-memory
    )
    echo "=== grok run (headless) ==="
    echo "+ ${grok_cmd[*]}" >&2
    echo "  · output-format=$fmt"
    echo "  · max_turns=$max_turns"
    echo "  · always-approve=$([[ $ARG_YES -eq 1 ]] && echo yes || echo no)"
    echo ""
    set +e
    "${grok_cmd[@]}" >"$events_file" 2>"$progress_file"
    agent_ec=$?
    set -e

    # Extract last message / compact result for summary.
    if [[ "$fmt" == "json" && -s "$events_file" ]]; then
      cp -f "$events_file" "$result_json" 2>/dev/null || true
      python3 - "$events_file" "$last_msg" <<'PY' 2>/dev/null || true
import json, sys
p, out = sys.argv[1], sys.argv[2]
try:
    d = json.load(open(p, encoding="utf-8"))
except Exception:
    open(out, "w").write("")
    raise SystemExit(0)
text = (d.get("text") or d.get("message") or "").strip()
open(out, "w", encoding="utf-8").write(text + ("\n" if text else ""))
PY
    elif [[ -s "$events_file" ]]; then
      python3 - "$events_file" "$last_msg" "$result_json" <<'PY' 2>/dev/null || true
import json, sys
path, last_path, end_path = sys.argv[1], sys.argv[2], sys.argv[3]
texts = []
end = None
with open(path, encoding="utf-8", errors="replace") as f:
    for line in f:
        line = line.strip()
        if not line:
            continue
        try:
            ev = json.loads(line)
        except json.JSONDecodeError:
            continue
        t = ev.get("type")
        if t == "text" and ev.get("data"):
            texts.append(str(ev["data"]))
        elif t == "end":
            end = ev
open(last_path, "w", encoding="utf-8").write("".join(texts))
if end is not None:
    json.dump(end, open(end_path, "w", encoding="utf-8"), indent=2)
    open(end_path, "a", encoding="utf-8").write("\n")
else:
    open(end_path, "w").write("")
PY
    fi
  fi

  end_ms="$(now_ms)"
  wall_ms=$((end_ms - start_ms))
  echo "$wall_ms" >"$out/wall_ms.txt"

  {
    echo "=== stderr progress ==="
    cat "$progress_file" 2>/dev/null || true
    echo ""
    echo "=== last message (truncated) ==="
    if [[ -s "$last_msg" ]]; then
      head -c 4000 "$last_msg" 2>/dev/null || true
      echo ""
    else
      echo "(none)"
    fi
  } >"$out/grok.log"

  local n_events=0
  if [[ -s "$events_file" ]]; then
    n_events="$(wc -l <"$events_file" 2>/dev/null | tr -d ' ' || echo 0)"
  fi
  echo "agent exit: $agent_ec  wall_ms: $wall_ms  events: $n_events"
  echo ""

  if [[ -s "$events_file" ]]; then
    python3 "$_BENCH_LIB_DIR/summarize_grok_events.py" \
      "$events_file" "$out/grok-stats.txt" || true
  else
    echo "(no events — tui or empty run)" >"$out/grok-stats.txt"
  fi

  echo "=== score (rubric / test_filter) ==="
  local result
  result="$(score_task "$ws" "$out" "$tdir")"
  echo "SCORE: $result"

  cat >"$out/summary.md" <<EOF
# $task · grok · ui=$ARG_UI

- result: $result
- agent exit: $agent_ec
- wall_ms: $wall_ms
- model: ${ARG_MODEL:-(default)}
- project: ${TASK_META_PROJECT:-(local fixture)}
- score cmd: \`$criterion\`
- max_turns: $max_turns
- workspace: \`$ws\`
- events: \`$events_file\`
- last message: \`$last_msg\`
- stats: \`$out/grok-stats.txt\`
- score log: \`$out/score.log\`

## grok-stats
\`\`\`
$( [[ -f "$out/grok-stats.txt" ]] && cat "$out/grok-stats.txt" || echo "(none)" )
\`\`\`
EOF

  echo ""
  echo "=== done ==="
  echo "summary: $out/summary.md"
  echo "events:  $events_file"
  echo "result:  $result"
  [[ "$result" == "pass" ]]
}
