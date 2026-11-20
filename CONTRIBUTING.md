# Contributing to Hiroi Downloader

Thank you for considering a contribution. This guide explains how the
project works, so that your time goes into changes that can be merged.

Hiroi is a download manager written in Rust: an engine, a command-line
front end, a background service, and a browser extension. It is also
a project its maintainer learns from, so changes are read line by line
before they are merged, and that can take a while.

## Ground rules

- One change per pull request, and one topic per issue.
- Hiroi stays polite to servers. A change that sends more requests,
  opens more connections, or retries faster needs a reason, and the
  reason goes into the commit message.
- Hiroi stays light. A new dependency needs a word in the pull request
  on what it costs (compile time, binary size, memory) and why
  something smaller does not do. Heavy features go behind a cargo
  feature that is off by default.
- Secrets (cookies, passwords, tokens) never reach a log, an error
  message, or a file on disk.
- Be kind in issues and reviews.

## Your first contribution

Good places to start are the documentation in `docs/`, a test for a
case nobody tested yet, and issues labelled `good first issue`, if
there are any. For anything larger than a typo or an obvious bug fix,
open an issue first and describe what you want to change.

## Getting started

You need Rust 1.89 or newer and a C compiler (`gcc` or `clang`). From
the repository's root:

```sh
cargo build --workspace
cargo test --workspace
```

The tests start their own servers, so they need no network access.
Before you push, run what the CI runs:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
```

The optional protocols (FTP, SFTP, and BitTorrent) are cargo features;
`--all-features` builds and tests them. The torrent tests need IPv6 on
the loopback interface and are skipped without it.

The browser extension is plain JavaScript; `extension/build.sh` builds
it for Firefox and Chromium into `extension/dist/`.

`docs/` explains every feature, and the README shows the layout of
the crates.

## How to report a bug

A security problem is reported privately, as SECURITY.md describes,
and not in a public issue.

For anything else, open an issue with the bug report form. It asks for
what the project needs to reproduce the problem: the component, the
platform and environment, the version or git revision, the steps,
what happened, what you expected, and how often it happens (as
failures out of attempts, such as 3/3). Logs from `-vv` help most;
remove cookies, passwords, tokens, and private links first.

## How to suggest a feature

Open an issue with the feature request form. Say what you are trying
to do and what gets in the way; a good description of the problem is
worth more than a finished design. Hiroi is a download manager first,
so ideas that serve downloading fit best.

## Code review process

The maintainer reviews pull requests when there is time. A review may
ask for changes; push them to the same branch. A pull request is
merged when the CI passes, the change is understood, and it fits the
ground rules above.

## Conventions

### Code

- `rustfmt` formats, and clippy gives no warnings with `-D warnings`.
- The engine knows nothing about the user interface: front ends listen
  through listener traits, whose methods have empty defaults.
- Constants live together (`constants.rs` in the engine), not as
  numbers scattered through the code.
- Comments say why, not what; the code says what. Public items have
  doc comments.
- A bug fix comes with a test that fails without it. A feature comes
  with tests and with documentation in `docs/`.

### Commit messages

The first line names the area and says what changed, in the imperative
and in about 60 characters at most:

```
engine: replace slow connections
```

The body says why the change is needed and anything a reviewer would
otherwise have to guess, wrapped at 72 columns.

### Sign-off

Every commit carries a `Signed-off-by` line, added with
`git commit -s`. With it you certify the
[Developer Certificate of Origin](https://developercertificate.org/):
in short, that you wrote the change, or otherwise have the right to
submit it under the project's license.

### Labels

Issues carry `bug` or `enhancement` from their forms. Other labels may
be added while an issue is looked at.

## License

Hiroi Downloader is licensed under the GNU Affero General Public
License, version 3 or later. By contributing, you agree that your
contribution is licensed under the same terms.
