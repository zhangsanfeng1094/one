# Code Quality Repair Design

## Goal

Make the repository enforceable by its existing Rust quality tooling without changing the coding agent's user-facing behavior.

## Scope

- Replace the vacuous compaction assertion with an assertion that verifies no messages are retained when `keep_recent_messages` is zero.
- Resolve the strict Clippy diagnostics in `one-core` with behavior-preserving refactors.
- Apply the repository's Rust formatter.
- Require formatting and strict Clippy checks in CI before the existing build, test, and release steps.

## Out of scope

- Broad removal of feature-gated or dead-code warnings outside `one-core`.
- Changes to localhost-based provider test fixtures; they work in normal CI and are only blocked by restricted execution sandboxes.
- Product roadmap items and API changes.

## Verification

Run the targeted compaction test, `cargo fmt --check`, strict workspace Clippy, the complete workspace test suite, and an all-feature compilation check.
