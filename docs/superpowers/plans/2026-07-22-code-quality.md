# Code Quality Repair Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make formatting and strict `one-core` Clippy checks pass and enforce them in CI without changing runtime behavior.

**Architecture:** Keep all changes local to existing implementation and test files. Replace the vacuous test assertion with an exact behavioral assertion, apply behavior-preserving Clippy refactors, format the workspace, and add two CI quality gates.

**Tech Stack:** Rust, Cargo, rustfmt, Clippy, GitHub Actions

---

### Task 1: Make the compaction test meaningful

**Files:**
- Modify: `crates/one-core/src/compaction.rs`

- [ ] Replace the always-true assertion with `assert!(kept.is_empty())`.
- [ ] Run `cargo test -p one-core compaction::tests::extractive_not_debug_dump` and confirm it passes.

### Task 2: Clear strict one-core Clippy diagnostics

**Files:**
- Modify: `crates/one-core/src/agent.rs`
- Modify: `crates/one-core/src/image.rs`
- Modify: `crates/one-core/src/trace.rs`

- [ ] Replace `clone` on `Option<u64>` with a copy.
- [ ] Introduce a local type alias for the parallel tool-job tuple.
- [ ] Introduce a small `ToolExecutionResult` value object and pass it to the result-finalization helper, reducing its argument count without suppressing Clippy.
- [ ] Replace manual prefix/suffix slicing with `strip_prefix` and `strip_suffix`.
- [ ] Replace exhaustive `filter_map` expressions with `map`.
- [ ] Run `cargo clippy -p one-core --all-targets -- -D warnings` and confirm it passes.

### Task 3: Format and enforce quality checks in CI

**Files:**
- Modify: repository Rust source files selected by `cargo fmt --all` (repository-wide mechanical formatting only; inspect the resulting diff for semantic changes)
- Modify: `.github/workflows/ci.yml`

- [ ] Run `cargo fmt --all`.
- [ ] Configure `dtolnay/rust-toolchain@stable` with `components: rustfmt, clippy`.
- [ ] Add `cargo fmt --all -- --check` before the existing CI check step.
- [ ] Add `cargo clippy -p one-core --all-targets -- -D warnings` before the existing CI check step.
- [ ] Run `cargo fmt --all -- --check`.

### Task 4: Verify the complete repository

- [ ] Run `cargo test --workspace` in an environment that permits localhost test listeners.
- [ ] Run `cargo check --workspace --all-features --all-targets`.
- [ ] Inspect `git diff --check` and `git status --short`.
