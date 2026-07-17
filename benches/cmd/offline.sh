# shellcheck shell=bash
# Offline / mock commands: smoke, baseline, task, suite, verify, trace, stats

cmd_smoke() {
  local out
  out="$(make_out smoke)"
  run_one bench --suite smoke --out "$out"
  echo ""; echo "out: $out"
}

cmd_baseline() {
  local out
  out="$(make_out baseline)"
  run_one bench --task kit-baseline --out "$out"
  echo ""; echo "out: $out"
}

cmd_task() {
  local id="${1:-}"
  [[ -n "$id" ]] || die "usage: run.sh task <task-id>"
  local out
  out="$(make_out "$id")"
  run_one bench --task "$id" --out "$out"
  echo ""; echo "out: $out"
}

cmd_suite() {
  local suite="${1:-smoke}"
  local out
  out="$(make_out "suite-$suite")"
  run_one bench --suite "$suite" --out "$out"
  echo ""; echo "out: $out"
}

cmd_verify() {
  local script="$FIXTURE_DIR/verify.sh"
  [[ -f "$script" ]] || die "missing $script"
  chmod +x "$script" 2>/dev/null || true
  "$script"
}

cmd_trace() {
  local prompt="${*:-list files in current directory}"
  local out
  out="$(make_out trace)"
  local trace="$out/run.jsonl"
  run_one --trace "$trace" -p "$prompt" --provider mock -y --no-session --no-mcp --no-skills
  echo ""
  run_one trace-stats "$trace" | tee "$out/trace-stats.txt"
  echo ""; echo "trace: $trace"
}

cmd_stats() {
  local file="${1:-}"
  [[ -n "$file" ]] || die "usage: run.sh stats <trace.jsonl>"
  [[ -f "$file" ]] || die "not found: $file"
  run_one trace-stats "$file"
}
