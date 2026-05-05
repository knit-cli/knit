# Architecture

Knit is split by responsibility so command behavior, persistence, git subprocesses, and pure helpers can be analyzed independently.

## Source Layout

```txt
src/
  main.rs       binary entry point only
  lib.rs        module wiring and command dispatch
  cli.rs        clap command definitions
  commands.rs   user-facing command behavior
  model.rs      bundle / ChangeGroup data structures
  store.rs      .knit config and bundle file persistence
  git.rs        git subprocess helpers
  ids.rs        slugs, commit group ids, SHA formatting
  paths.rs      path comparison helpers
  status.rs     git status classification
  time.rs       timestamp formatting
tests/
  ids.rs
  status.rs
```

Rust does not use classes in the TypeScript sense. The equivalent separation here is modules plus explicit data types. `model.rs` owns the long-lived schema types; `commands.rs` coordinates those types with filesystem and git operations.

## Boundaries

- `main.rs` should stay tiny. It parses CLI arguments and calls `knit::run`.
- `cli.rs` should contain only argument shape and help text.
- `commands.rs` may orchestrate multiple modules, but should avoid low-level git or JSON details.
- `git.rs` is the only place that should construct raw `git` subprocess calls.
- `store.rs` is the only place that should load the active bundle from `.knit/config.json`.
- Pure helper behavior should live in small modules and have integration tests under `tests/`.

## Testing

Cheap pure behavior belongs in integration tests:

- Slug and id behavior: `tests/ids.rs`
- Status classification: `tests/status.rs`

End-to-end git behavior is documented as a manual smoke test in [manual-test.md](manual-test.md). As Knit grows, that smoke test can become an automated integration test using temporary toy repos.
