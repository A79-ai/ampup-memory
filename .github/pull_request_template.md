## What and why

<!-- What this changes, and why it is needed. Link the issue it closes, if any (Closes #123). -->

## How I verified it

<!--
What you ran locally and what you saw: the flow you drove against a real `ampup-memory serve` and store
(`serve --local` is fine), and for a bug, the reproduction before the fix and the same steps after it.
For a permission or auth change, also show that the caller who must be refused is refused.
See CONTRIBUTING.md: most changes are verified this way, not with a new test.
-->

## Checklist

- [ ] `cargo fmt --all` and `cargo clippy --all-targets --locked -- -D warnings` are clean
- [ ] `cargo test --features dylib` (or `cargo nextest run --features dylib`) passes
- [ ] Tests are added or extended only for isolation or deletion flows (CONTRIBUTING.md, "Tests")
- [ ] A new dependency is in `[workspace.dependencies]`, the crate that uses it, and `crates/memstore-deps`
