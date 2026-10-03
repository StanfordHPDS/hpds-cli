## What changed

<!-- Describe the changes in this PR and why they were made. -->

## Important decisions

<!-- Document design decisions, trade-offs, or spec ambiguities you resolved
     (and how), so reviewers and future readers don't have to reconstruct them. -->

## Checklist

- [ ] `cargo test --locked` passes
- [ ] `cargo clippy --all-targets --all-features --locked -- -D warnings` passes
- [ ] `cargo fmt --check` passes
- [ ] `RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --locked` passes
- [ ] `typos` passes
- [ ] New behavior is covered by tests (test-first)
