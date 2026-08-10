#!/usr/bin/env python3
"""Summarize Grok headless --output-format streaming-json NDJSON into stats text."""

from __future__ import annotations

import json
import sys
from collections import Counter


def main() -> int:
    if len(sys.argv) < 2:
        print(
            "usage: summarize_grok_events.py <events.jsonl> [out.txt]",
            file=sys.stderr,
        )
        return 2
    path = sys.argv[1]
    outp = sys.argv[2] if len(sys.argv) > 2 else None

    types: Counter[str] = Counter()
    tools: Counter[str] = Counter()
    tool_calls = tool_done = tool_err = 0
    text_chunks = thought_chunks = errors = 0
    in_tok = out_tok = cache_read = cache_create = reasoning = 0
    num_turns = 0
    session_id = stop_reason = None
    end_seen = False

    with open(path, encoding="utf-8", errors="replace") as f:
        for line in f:
            line = line.strip()
            if not line:
                continue
            try:
                ev = json.loads(line)
            except json.JSONDecodeError:
                continue
            if not isinstance(ev, dict):
                continue
            t = ev.get("type") or ""
            types[t] += 1

            if t == "tool_call":
                tool_calls += 1
                name = ev.get("toolName") or ev.get("title") or "?"
                tools[str(name)] += 1
            elif t == "tool_call_update":
                st = (ev.get("status") or "").lower()
                if st in ("completed", "success", "ok"):
                    tool_done += 1
                elif st in ("failed", "error", "cancelled"):
                    tool_err += 1
            elif t == "text":
                text_chunks += 1
            elif t == "thought":
                thought_chunks += 1
            elif t == "error":
                errors += 1
            elif t == "usage":
                u = ev.get("usage") or {}
                in_tok += int(u.get("input_tokens") or 0)
                out_tok += int(u.get("output_tokens") or 0)
                cache_read += int(u.get("cache_read_input_tokens") or 0)
                cache_create += int(u.get("cache_creation_input_tokens") or 0)
                reasoning += int(u.get("reasoning_tokens") or 0)
            elif t == "end":
                end_seen = True
                session_id = ev.get("sessionId") or session_id
                stop_reason = ev.get("stopReason") or stop_reason
                if ev.get("num_turns") is not None:
                    num_turns = int(ev["num_turns"])
                u = ev.get("usage") or {}
                # Prefer end-aggregate when present (may restate totals).
                if u:
                    in_tok = int(u.get("input_tokens") or in_tok)
                    out_tok = int(u.get("output_tokens") or out_tok)
                    cache_read = int(u.get("cache_read_input_tokens") or cache_read)
                    cache_create = int(
                        u.get("cache_creation_input_tokens") or cache_create
                    )
                    reasoning = int(u.get("reasoning_tokens") or reasoning)

    # Also accept a single final JSON object (output-format json).
    if not types and path:
        try:
            with open(path, encoding="utf-8", errors="replace") as f:
                blob = f.read().strip()
            if blob:
                d = json.loads(blob)
                if isinstance(d, dict) and ("text" in d or "sessionId" in d):
                    session_id = d.get("sessionId")
                    stop_reason = d.get("stopReason")
                    num_turns = int(d.get("num_turns") or 0)
                    u = d.get("usage") or {}
                    in_tok = int(u.get("input_tokens") or 0)
                    out_tok = int(u.get("output_tokens") or 0)
                    cache_read = int(u.get("cache_read_input_tokens") or 0)
                    cache_create = int(u.get("cache_creation_input_tokens") or 0)
                    reasoning = int(u.get("reasoning_tokens") or 0)
                    types["json_result"] = 1
                    end_seen = True
                    if d.get("type") == "error" or d.get("message"):
                        if d.get("type") == "error":
                            errors = 1
        except (json.JSONDecodeError, OSError):
            pass

    total = in_tok + out_tok + cache_read + cache_create
    lines = [
        f"session_id:    {session_id or '?'}",
        f"stop_reason:   {stop_reason or ('?' if not end_seen else 'end_turn')}",
        f"events:        {sum(types.values())}",
        f"turns:         {num_turns}",
        f"tool_calls:    {tool_calls}",
        f"tool_done:     {tool_done}",
        f"tool_errors:   {tool_err}",
        f"text_chunks:   {text_chunks}",
        f"thoughts:      {thought_chunks}",
        f"errors:        {errors}",
        (
            f"tokens:        in={in_tok} out={out_tok} "
            f"cache_read={cache_read} cache_create={cache_create} "
            f"reasoning={reasoning} total={total}"
        ),
        f"tools:         {dict(tools) if tools else '{}'}",
        f"event_types:   {dict(types)}",
    ]
    text = "\n".join(lines) + "\n"
    if outp:
        with open(outp, "w", encoding="utf-8") as f:
            f.write(text)
    print(text, end="")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
