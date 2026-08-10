# shellcheck shell=bash
# Side-by-side agent compare: One vs Codex / Grok / all

_read_stat() {
  # $1 file $2 regex key → first field after key
  local f="$1" key="$2"
  [[ -f "$f" ]] || { echo "?"; return; }
  grep -E "^${key}:" "$f" 2>/dev/null | head -1 | awk '{print $2}' || echo "?"
}

_read_tok_total() {
  local f="$1"
  [[ -f "$f" ]] || { echo "?"; return; }
  # codex: tokens: in=.. out=.. total=N
  # grok:  tokens: in=.. out=.. ... total=N
  grep -E '^tokens:' "$f" 2>/dev/null | sed -n 's/.*total=\([0-9]*\).*/\1/p' | head -1 || echo "?"
}

# Parse compare flags: --vs codex|grok|all  + shared agent args
# Sets: ARG_COMPARE_VS (codex|grok|all)
_parse_compare_args() {
  ARG_COMPARE_VS="${COMPARE_VS:-codex}"
  local -a rest=()
  while [[ $# -gt 0 ]]; do
    case "$1" in
      --vs)
        [[ $# -ge 2 ]] || die "missing value for --vs (codex|grok|all)"
        ARG_COMPARE_VS="$2"; shift 2 ;;
      --vs=*)
        ARG_COMPARE_VS="${1#--vs=}"; shift ;;
      --with-grok)
        ARG_COMPARE_VS="grok"; shift ;;
      --all-agents)
        ARG_COMPARE_VS="all"; shift ;;
      *)
        rest+=("$1"); shift ;;
    esac
  done
  case "$ARG_COMPARE_VS" in
    codex|grok|all|both) ;;
    *) die "invalid --vs $ARG_COMPARE_VS (use codex|grok|all)" ;;
  esac
  # both → codex for back-compat wording
  if [[ "$ARG_COMPARE_VS" == "both" ]]; then
    ARG_COMPARE_VS="codex"
  fi
  COMPARE_REST=("${rest[@]}")
}

cmd_compare() {
  _parse_compare_args "$@"
  set -- "${COMPARE_REST[@]}"

  local prc=0
  set +e
  parse_agent_args "$@"
  prc=$?
  set -e
  if [[ $prc -eq 2 ]]; then cmd_help; return 0; fi
  if [[ $prc -eq 3 ]]; then return 0; fi
  if [[ $prc -ne 0 ]]; then return "$prc"; fi

  local task="$ARG_TASK"
  echo "→ 对比任务: $task  (vs=$ARG_COMPARE_VS)"
  echo ""
  task_dir "$task" >/dev/null

  local base
  base="$(make_out "compare-${task}")"
  mkdir -p "$base"

  local vs_label="Codex"
  case "$ARG_COMPARE_VS" in
    grok) vs_label="Grok" ;;
    all)  vs_label="Codex + Grok" ;;
  esac

  echo "############################################"
  echo "# 同一任务对比  One  vs  $vs_label"
  echo "# task = $task"
  echo "# out  = $base"
  echo "############################################"
  echo ""

  # ---- One ----
  echo "---------- [One] ----------"
  local one_out="$base/one"
  local -a one_args=("$task")
  if [[ -n "$ARG_PROVIDER" ]]; then
    one_args+=(--provider "$ARG_PROVIDER")
  fi
  if [[ -n "$ARG_MODEL" ]]; then
    one_args+=(--model "$ARG_MODEL")
  fi
  # Compare is batch-oriented: prefer headless AgentSpec for One.
  one_args+=(--headless)
  set +e
  ONE_BENCH_OUT="$one_out" cmd_full "${one_args[@]}"
  local one_ec=$?
  set -e

  local one_result="fail"
  if [[ -f "$one_out/result.txt" ]]; then
    one_result="$(cat "$one_out/result.txt")"
  fi
  local one_wall="?" one_turns="?" one_tools="?" one_tok="?"
  if [[ -f "$one_out/agent-result.json" ]]; then
    one_wall="$(python3 -c 'import json,sys; d=json.load(open(sys.argv[1])); print(d.get("duration_ms") or "?")' "$one_out/agent-result.json" 2>/dev/null || echo "?")"
    one_turns="$(python3 -c 'import json,sys; d=json.load(open(sys.argv[1])); print(d.get("turns") if d.get("turns") is not None else "?")' "$one_out/agent-result.json" 2>/dev/null || echo "?")"
    one_tok="$(python3 -c 'import json,sys; d=json.load(open(sys.argv[1])); u=d.get("usage") or {}; print((u.get("input_tokens") or 0)+(u.get("output_tokens") or 0) or "?")' "$one_out/agent-result.json" 2>/dev/null || echo "?")"
  fi

  local table_rows=""
  table_rows+="| **One** | $one_result | $one_wall | $one_turns | $one_tools | $one_tok | \`one/\` |"
  table_rows+=$'\n'
  local exit_bits="One exit: $one_ec"
  local path_bits="- One: \`$one_out\`"
  local rerun_bits="./benches/run.sh full $task --headless"

  local all_pass=1
  [[ "$one_result" == "pass" ]] || all_pass=0

  # ---- Codex ----
  if [[ "$ARG_COMPARE_VS" == "codex" || "$ARG_COMPARE_VS" == "all" ]]; then
    echo ""
    echo "---------- [Codex] ----------"
    local codex_out="$base/codex"
    set +e
    ONE_BENCH_OUT="$codex_out" cmd_codex "$task"
    local codex_ec=$?
    set -e

    local codex_result="fail"
    if [[ -f "$codex_out/result.txt" ]]; then
      codex_result="$(cat "$codex_out/result.txt")"
    fi
    local codex_wall codex_turns codex_tools codex_tok
    codex_wall="$(cat "$codex_out/wall_ms.txt" 2>/dev/null || echo "?")"
    codex_turns="$(_read_stat "$codex_out/codex-stats.txt" turns)"
    codex_tools="$(_read_stat "$codex_out/codex-stats.txt" commands)"
    codex_tok="$(_read_tok_total "$codex_out/codex-stats.txt")"

    table_rows+="| **Codex** | $codex_result | $codex_wall | $codex_turns | $codex_tools | $codex_tok | \`codex/codex-events.jsonl\` |"
    table_rows+=$'\n'
    exit_bits+=" · Codex exit: $codex_ec"
    path_bits+=$'\n'"- Codex: \`$codex_out\`"
    rerun_bits+=$'\n'"./benches/run.sh codex $task"
    [[ "$codex_result" == "pass" ]] || all_pass=0
  fi

  # ---- Grok ----
  if [[ "$ARG_COMPARE_VS" == "grok" || "$ARG_COMPARE_VS" == "all" ]]; then
    echo ""
    echo "---------- [Grok] ----------"
    local grok_out="$base/grok"
    local -a grok_args=("$task" --headless)
    if [[ -n "$ARG_MODEL" ]]; then
      # model flag is One-oriented; only pass if user set it and Grok accepts it
      grok_args+=(--model "$ARG_MODEL")
    fi
    set +e
    ONE_BENCH_OUT="$grok_out" cmd_grok "${grok_args[@]}"
    local grok_ec=$?
    set -e

    local grok_result="fail"
    if [[ -f "$grok_out/result.txt" ]]; then
      grok_result="$(cat "$grok_out/result.txt")"
    fi
    local grok_wall grok_turns grok_tools grok_tok
    grok_wall="$(cat "$grok_out/wall_ms.txt" 2>/dev/null || echo "?")"
    grok_turns="$(_read_stat "$grok_out/grok-stats.txt" turns)"
    grok_tools="$(_read_stat "$grok_out/grok-stats.txt" tool_calls)"
    grok_tok="$(_read_tok_total "$grok_out/grok-stats.txt")"

    table_rows+="| **Grok** | $grok_result | $grok_wall | $grok_turns | $grok_tools | $grok_tok | \`grok/grok-events.jsonl\` |"
    table_rows+=$'\n'
    exit_bits+=" · Grok exit: $grok_ec"
    path_bits+=$'\n'"- Grok: \`$grok_out\`"
    rerun_bits+=$'\n'"./benches/run.sh grok $task --headless"
    [[ "$grok_result" == "pass" ]] || all_pass=0
  fi

  local cmp="$base/compare.md"
  cat >"$cmp" <<EOF
# Compare · $task · vs=$ARG_COMPARE_VS · $(now_ts)

Same fixture, same prompt, score = task rubric / \`cargo test\`.

| agent | result | wall_ms | turns | tools/cmds | tokens | artifact |
|-------|--------|---------|-------|------------|--------|----------|
${table_rows}
- $exit_bits
$path_bits

Re-run:
\`\`\`
$rerun_bits
\`\`\`
EOF

  echo ""
  echo "############################################"
  cat "$cmp"
  echo "############################################"
  echo "wrote: $cmp"

  [[ "$all_pass" -eq 1 ]]
}
