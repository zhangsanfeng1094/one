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
  if [[ -z "${LANGFUSE_PUBLIC_KEY:-}" || -z "${LANGFUSE_SECRET_KEY:-}" ]]; then
    die "cmd trace requires LANGFUSE_PUBLIC_KEY + LANGFUSE_SECRET_KEY (export then re-run)"
  fi
  run_one --trace -p "$prompt" --provider mock -y --no-session --no-mcp --no-skills
  echo ""
  echo "trace: langfuse (${LANGFUSE_BASE_URL:-${LANGFUSE_HOST:-https://cloud.langfuse.com}})"
  echo "out:   $out"
}

cmd_stats() {
  die "stats removed — traces live in Langfuse UI (set LANGFUSE_* and use --trace)"
}
