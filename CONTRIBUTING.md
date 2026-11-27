# Contributing to Knit

Knit is alpha software with one maintainer. Bug reports, fixes and doc
corrections are welcome; reviews may take a while.

## Build

You need Rust 1.86 or newer (the `rust-version` in `Cargo.toml`) and `git` on
`PATH`. The test suite also runs `sh`, Python 3 and `ssh-keygen`.

```sh
cargo build
```

## Check a change the way CI does

CI ([.github/workflows/ci.yml](.github/workflows/ci.yml)) runs these on every
pull request. Run them before you open one:

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo build --all-targets
cargo test --no-fail-fast
python3 -m unittest discover -s scripts -p '*_test.py' -v   # release packaging scripts
```

Build and tests run on Linux, macOS and Windows. A separate MSRV job runs
`cargo check --all-features` on the Rust version named in `Cargo.toml`, so do
not use language or library features newer than that. Clippy findings fail
CI; a lint that is really not worth fixing is allowed by name at its site,
with a comment saying why.

The tests build throwaway git repos and a workspace with its own `KNIT_HOME`,
then drive the real `knit` binary against them. They never read your
`~/.config/knit` or contact a sync remote. Which file a new test belongs in is
described under "Testing" in [docs/architecture.md](docs/architecture.md#testing),
along with the module boundaries to respect.

## Keep the docs true

A behavior change usually needs a doc change in the same pull request:
[docs/reference.md](docs/reference.md) for commands and flags, the schemas in
`schemas/` and [docs/change-group-schema.md](docs/change-group-schema.md) for
artifact fields. `tests/docs.rs` checks that `docs/reference.md` names every
top-level command and that the reference and the generated `AGENTS.md` text
do not teach spellings that were removed. It does not check the rest, so
please read the sections you touch.

## Commits and pull requests

Commit subjects are short imperative sentences that say what changed, such
as "Include the whole day in plain-date --until bounds". Documentation-only
commits start with `docs:`. Keep unrelated changes in separate commits.

Fork the repository, push a branch, and open a pull request against `main`.
The maintainer's own branches are named `knit/<slug>` because Knit is
developed with Knit; you do not need Knit or that naming to contribute.
Pull requests are merged with a merge commit.

## Reporting bugs

Open an issue at <https://github.com/knit-cli/knit/issues> with the output of
`knit --version`, your platform, the commands you ran and what happened.
Remove tokens and private paths from logs before you paste them.
