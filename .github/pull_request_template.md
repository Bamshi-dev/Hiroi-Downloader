<!--
Thank you for the pull request. For a bug fix, the bug fix template asks
for a little more: add `?template=bug_fix.md` to the address of the page
that opens a pull request.
-->

Closes #

## Description

What this changes, and why. Anything a reviewer would otherwise have to guess.

## Screenshots

For changes to the browser extension or anything else that shows on screen. Delete this section otherwise.

## Documentation

Where the change is documented (the guide in `docs/`, the README, `--help` texts, or doc comments), or why it needs none.

## Ready?

- [ ] It does one thing, and the commits say what and why.
- [ ] New behaviour has tests; a bug fix has a test that fails without it.
- [ ] `cargo fmt --all --check` passes.
- [ ] `cargo clippy --workspace --all-targets --all-features -- -D warnings` passes.
- [ ] `cargo test --workspace --all-features` passes.
- [ ] The documentation is up to date.
- [ ] Commits are signed off (`git commit -s`).
