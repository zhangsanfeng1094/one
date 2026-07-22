# Code Quality Repair Design

## Goal

Make the repository enforceable by its existing Rust quality tooling without changing the coding agent's user-facing behavior.

## Scope

- Replace the vacuous compaction assertion with an assertion that verifies no messages are retained when `keep_recent_messages` is zero.
- Resolve the strict Clippy diagnostics in `one-core` with behavior-preserving refactors.
- Apply the repository's Rust formatter.
- Require formatting and strict `one-core` Clippy checks in CI before the existing build, test, and release steps.

## Out of scope

- Broad removal of feature-gated or dead-code warnings outside `one-core`.
- Changes to localhost-based provider test fixtures; they work in normal CI and are only blocked by restricted execution sandboxes.
- Product roadmap items and API changes.

## Verification

Run `cargo test -p one-core compaction::tests::extractive_not_debug_dump`, `cargo fmt --check`, `cargo clippy -p one-core --all-targets -- -D warnings`, `cargo test --workspace`, and `cargo check --workspace --all-features --all-targets`. CI runs the format and `one-core` Clippy commands before its existing checks.
