#!/usr/bin/env python3
"""Summarize codex exec --json JSONL into a short stats text file."""

from __future__ import annotations

import json
import sys
from collections import Counter


def main() -> int:
    if len(sys.argv) < 2:
        print("usage: summarize_codex_events.py <events.jsonl> [out.txt]", file=sys.stderr)
        return 2
    path = sys.argv[1]
    outp = sys.argv[2] if len(sys.argv) > 2 else None

    turns = tool_cmds = file_changes = messages = errors = 0
    in_tok = out_tok = cache_tok = 0
    types: Counter[str] = Counter()
    item_types: Counter[str] = Counter()
    thread_id = None

    with open(path, encoding="utf-8", errors="replace") as f:
        for line in f:
            line = line.strip()
            if not line:
                continue
            try:
                ev = json.loads(line)
            except json.JSONDecodeError:
                continue
            t = ev.get("type") or ""
            types[t] += 1
            if t == "thread.started":
                thread_id = ev.get("thread_id") or thread_id
            if t == "turn.started":
                turns += 1
            if t == "turn.completed":
                u = ev.get("usage") or {}
                in_tok += int(u.get("input_tokens") or 0)
                out_tok += int(u.get("output_tokens") or 0)
                cache_tok += int(u.get("cached_input_tokens") or 0)
            if t in ("error", "turn.failed"):
                errors += 1
            if t in ("item.started", "item.completed", "item.updated"):
                item = ev.get("item") or {}
                it = item.get("type") or "?"
                item_types[it] += 1
                if t != "item.completed":
                    continue
                if it == "command_execution":
                    tool_cmds += 1
                elif it in ("file_change", "apply_patch", "patch"):
                    file_changes += 1
                elif it == "agent_message":
                    messages += 1

    lines = [
        f"thread_id:     {thread_id or '?'}",
        f"events:        {sum(types.values())}",
        f"turns:         {turns}",
        f"commands:      {tool_cmds}",
        f"file_changes:  {file_changes}",
        f"messages:      {messages}",
        f"errors/fails:  {errors}",
        f"tokens:        in={in_tok} out={out_tok} cache={cache_tok} total={in_tok + out_tok}",
        f"event_types:   {dict(types)}",
        f"item_types:    {dict(item_types)}",
    ]
    text = "\n".join(lines) + "\n"
    if outp:
        with open(outp, "w", encoding="utf-8") as f:
            f.write(text)
    print(text, end="")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
