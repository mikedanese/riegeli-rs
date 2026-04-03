# Project Notes

## Version Control

- Use `jj commit` (not `git commit`) to commit changes.
- Commit at the close of every successful sprint in the build loop.

## Build Loop

- Pipeline: planner → (generator → evaluator)* with per-phase progress tracking.
- Phase 1: `artifacts/phase1/` (sprints 1–7, complete).
- Phase 2: `artifacts/phase2/` (sprints 8–13, transpose chunk support, complete).
- Phase 3: `artifacts/phase3/` (sprints 14–19, conformance/tuning/projection, in progress).
- Slow proptests in `riegeli/tests/sprint_7_proptest.rs` are `#[ignore]`d by default. Run with `cargo test -p riegeli --test sprint_7_proptest -- --ignored`.

## Reference Implementation

- C++ reference is at `~/code/riegeli/`.
- Key transpose files: `riegeli/chunk_encoding/transpose_{encoder,decoder}.{h,cc}`, `transpose_internal.h`.
