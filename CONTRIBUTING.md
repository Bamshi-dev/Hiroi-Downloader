# Contributing to Hiroi Downloader

Thank you for considering a contribution. This file explains how the
project is organized, what we expect from a change, and how a change
gets merged.

## Before you start

For anything larger than a typo or an obvious bug fix, open an issue
first and describe what you want to change. A download manager touches
the network, the disk, and other people's servers; a short discussion
up front avoids work that cannot be merged.

## Building and testing

You need Rust 1.89 or newer and a C compiler. From the repository root:

```sh
cargo build --workspace
cargo test --workspace
```

The tests start local HTTP servers, so they need no network access.
Before you push, run the same checks as the CI:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Optional protocols live behind cargo features. If you change one of
them, build and test it explicitly, for example
`cargo test -p hiroi-engine --features ftp`.

## What a good change looks like

A change does one thing. A bug fix comes with a test that fails without
the fix. A new feature comes with tests and with documentation in
`docs/`. Code follows `rustfmt` and produces no clippy warnings.

Comments explain why the code does something, not what it does; the
code itself should say what it does. Public items get doc comments.

Hiroi must stay polite to servers. Changes that send more requests,
open more connections, or retry faster than the current code need a
good reason, and that reason belongs in the commit message.

Hiroi must also stay light. If a change adds a dependency, say in the
pull request what it costs (compile time, binary size, memory) and why
a smaller solution does not work. Heavy features go behind a cargo
feature that is off by default.

## Commit messages

The first line names the area and says what changed, in the imperative
and under about 60 characters:

```
engine: replace slow connections
```

The body explains why the change is needed and anything a reviewer
would otherwise have to guess. Wrap it at 72 columns.

## Sign-off

Every commit must carry a `Signed-off-by` line, which you add with
`git commit -s`. By signing off you certify the
[Developer Certificate of Origin](https://developercertificate.org/):
in short, that you wrote the change or otherwise have the right to
submit it under the project's license.

## License

Hiroi Downloader is licensed under the GNU Affero General Public
License, version 3 or later. By contributing, you agree that your
contribution is licensed under the same terms.
