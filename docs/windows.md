# Windows support

**Status: the full test suite builds and passes on windows-latest in CI** (build + all integration suites, including the sh-script fixtures via .cmd shims and the native GitHub API transport against a local stub). No interactive/manual validation on a real Windows workstation yet. Knit was originally developed and run only on
macOS and Linux. This document is the first pass at a portability audit. It
records the hazards found in `src/`, what is expected to work on Windows, what
the known blockers are, and the open design questions that must be answered
before Knit can claim Windows support.

Nothing here promises a working Windows build. The CI `windows` job
(`.github/workflows/ci.yml`) currently runs `cargo check` plus the pure-unit
test targets only; it does **not** exercise the git/worktree/process flows that
make up the bulk of real usage.

## How to read this

Severity reflects likelihood of breaking real Knit usage on Windows:

- **High** — likely to break a common command outright.
- **Medium** — works in the common case but has correctness or UX gaps.
- **Low** — cosmetic, edge-case, or already handled by the OS/std.

`file:line` references are accurate as of this audit; treat them as starting
points, not pins.

## Audit table

| Area | Location | Severity | Notes |
|------|----------|----------|-------|
| External `git` lookup | `src/git.rs:146,179,199`, `src/commands/git_passthrough.rs:42`, `src/commands/runtime.rs:458`, `src/commands/remote/clone.rs:491` | Medium | `Command::new("git")`. Rust resolves `git.exe` on `PATH`, so this works if Git for Windows is installed. Not a blocker, but Git is now a hard external dependency on Windows too. |
| External forge CLIs | `src/providers/github.rs:14` (`gh`), `src/providers/gitlab.rs:9` (`glab`), `src/providers/forgejo.rs:9` (`tea`) | Fixed | `forge_cli_command` in `src/providers/mod.rs` probes `.exe`, bare name, then `.cmd`/`.bat` shims on Windows before spawning. `.ps1`-only installs remain unsupported. |
| HTTP client | `src/providers/github.rs`, `src/commands/remote/client.rs` | Fixed | Sync-remote and GitHub artifact-mode API calls use a built-in HTTP client (ureq, rustls). No external `curl` dependency; the GitHub transport resolves IPv4-first, and the temporary netrc credentials file is gone — tokens travel in an `Authorization` header. |
| Interactive shell spawn | `src/commands/init.rs:168-195` | Low | `start_shell_in` already branches: `default_shell()` returns `cmd` on Windows, `/bin/sh` otherwise, and honors `$SHELL`. `cmd` does not understand the `KNIT_*` env it is handed in any special way, but the spawn itself is portable. No `sh -c` wrapper is used. |
| Project/deploy command spawn | `src/commands/run.rs:204`, `src/commands/land/execute.rs:358,435` | Medium | Project commands and land deploy steps are spawned as `Command::new(command[0]).args(command[1..])` — a direct exec, **not** `sh -c`. This is portable in principle, but project/land JSON authored on Unix often assumes a POSIX shell (e.g. `["sh","-c","..."]`, `&&`, globbing). Such commands will not run on Windows. This is a data/portability concern, not a code bug. Documented, intentionally not refactored. |
| Path comparison semantics | `src/paths.rs:14` (`same_path`) | Medium | Used for repo dedup in `src/commands/track.rs:117,272`. Now case-folds path components on Windows so `C:\repo` and `c:\repo` dedupe correctly; stays exact/case-sensitive on Unix. Best-effort textual compare, not canonicalization. (Fixed in this branch.) |
| Worktree bundle resolution | `src/store.rs` (`infer_worktree_bundle`) | Fixed | Uses `paths::strip_path_prefix`, which is exact on Unix and component-wise case-insensitive on Windows. (`resolve_context_bundle` no longer exists; folder contexts were removed.) Canonicalized inputs no longer carry `\\?\` prefixes — all `fs::canonicalize` calls go through `paths::canonicalize` (dunce). |
| Path strings in `Path::join` | `src/store.rs` (`.knit/bundles`, `.knit/locks`, etc., many sites) | Low | Segments like `"join(".knit/bundles")"` embed a `/`. Windows accepts `/` as a separator at the OS level, so these resolve. Cosmetic only (mixed separators in error messages). Not worth churning many lines. |
| Config dir resolution | `src/store.rs` (`global_config_path`) | Fixed | Resolves `KNIT_HOME` → `XDG_CONFIG_HOME` → `HOME` → (Windows) `%APPDATA%\\knit` → `%USERPROFILE%\\.config\\knit`. |
| Color / TTY detection | `src/output.rs:101-119` | Low | Honors `NO_COLOR`, `KNIT_COLOR`, `CLICOLOR_FORCE`, `TERM != dumb`, and `is_terminal()`. The ANSI escapes (`\x1b[...]`) are not enabled on legacy `conhost` without VT processing; modern Windows Terminal / Win10+ console handles them. Low risk; worst case is stray escape sequences on old consoles. |
| File locking | `src/store.rs:276-293` (`acquire_named_lock`), `Drop` at `src/store.rs:482-486` | Low | Lock is an advisory lockfile created with `OpenOptions::create_new(true)` and removed on `Drop`. `create_new` maps to exclusive create on Windows and works. No `flock`/`fcntl` is used. |
| `fs::rename` over existing file | (none found) | n/a | No `fs::rename` calls exist in `src/`. JSON writes use `fs::write` directly (`src/store.rs:301-305`), and history uses append (`src/history.rs:92-95`). The classic Windows "rename onto existing file fails" hazard is **not** present. (Trade-off: writes are not atomic, but that is pre-existing and cross-platform.) |
| Symlinks | (none found) | n/a | No `symlink`/`symlink_file`/`symlink_dir` usage in `src/`. Git worktrees are created by the `git` CLI, which handles its own platform specifics. |
| Path length | implicit (`.knit/worktrees/<bundle>/<repo>/...`) | Low/Medium | Worktree paths nest workspace + bundle slug + repo + repo contents and can exceed the legacy `MAX_PATH` (260) limit. Modern Windows with long-path support enabled handles this; older setups may fail deep in git operations. Out of Knit's direct control; flag for testing. |

## Expected to work

- Pure data / model logic: `ChangeGroup` construction, slugify, id generation,
  status-label parsing (`tests/ids.rs`, `tests/model.rs`, `tests/status.rs`).
  These are exercised by the Windows CI job.
- `cargo check` / compilation of the whole crate (CI-verified per push).
- Advisory locking under `.knit/locks/`.
- Color/TTY handling on modern Windows consoles.
- Spawning `git.exe` / `gh.exe` when those tools are installed as
  real `.exe`s on `PATH`.

## Known blockers (must fix before claiming support)

1. **Global config without `KNIT_HOME`.** `global_config_path` has no Windows
   fallback (`%APPDATA%`/`USERPROFILE`). Users must set `KNIT_HOME` today.
2. **Forge/deploy commands authored as POSIX shell.** Project commands and land
   deploy steps written as `sh -c "..."` (or relying on `&&`, globbing, `$VAR`
   expansion) will not run under a direct `Command` spawn on Windows.
3. **Case/drive-letter sensitivity in bundle resolution.** `infer_worktree_bundle`
   and `resolve_context_bundle` compare paths case-sensitively; a casing
   mismatch silently loses bundle context.
4. **`.cmd`/`.bat` forge-CLI shims** are not resolved by `Command::new`.
5. **Unvalidated:** the entire git-worktree / commit / push / merge / land flow.
   None of it has been run on Windows; the smoke suite is excluded from the
   Windows CI job.

## Open design issue: path normalization in JSON artifacts

This is the most important unresolved question and deserves a deliberate
decision before any serious Windows work.

Knit records repo paths inside bundle artifacts and context files. The relevant
producers are:

- `relative_path_for_storage` (`src/store.rs:244-249`) — `to_string_lossy()` of
  a `strip_prefix` result, i.e. **native separators**.
- `set_folder_active_bundle` (`src/store.rs:257-274`) — stores the same native
  string into `.knit/contexts.json`.
- Recorded repo `path` fields in bundle artifacts (consumed by `same_path`).

These artifacts **travel between machines and operating systems via sync remotes**
(push/pull/clone). A bundle authored on macOS stores `backend/src`; the same
bundle authored on Windows would store `backend\src`. When that artifact is
pulled onto the other OS:

- `Path::new("backend\\src")` on **Unix** is a *single* path component named
  `backend\src` — the separator is lost, and `strip_prefix`/`join` break.
- A Unix-authored `backend/src` on **Windows** happens to work because Windows
  accepts `/`, which is why the asymmetry is easy to miss.

So the hazard is specifically **Windows-authored artifacts consumed on Unix**,
not the reverse.

The open question: **what is the canonical on-disk representation of paths in
Knit artifacts?** Options, none yet chosen:

- **(A) Forward-slash canonical form.** Normalize every stored path to `/` on
  write and translate to native separators on read. Most portable; requires a
  single choke-point for all artifact path serialization and a migration for
  existing artifacts.
- **(B) Store relative POSIX-style paths only, never absolute.** Eliminates
  drive letters entirely; combine with (A). Absolute paths (e.g. tracked repo
  locations outside the workspace) would need a separate, explicitly
  machine-local representation that is *not* synced.
- **(C) Normalize on read, tolerate either separator.** Cheaper but leaves
  mixed-separator artifacts in the wild and doesn't fix the Unix "single
  component" parse problem.

**Resolved (A) for relative paths:** `relative_path_for_storage` now writes forward slashes on Windows, and `PathBuf::from` accepts them on every platform when resolving. Absolute local paths (tracked repo paths) remain native — they never travel meaningfully cross-OS and are rewritten by `localize_bundle` on pull/clone. Original recommendation kept for reference: **(A)+(B)** — a forward-slash, relative-where-possible
canonical form with a documented migration — but this is a design decision, not
something to implement as part of this audit branch.
