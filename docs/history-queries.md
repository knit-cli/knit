# Inspect local history

`knit log` inspects the current bundle. Add `--all` to inspect the current
project's locally recorded history, including archived bundles and preserved
events from deleted bundles. Neither command fetches from Git, contacts a sync
remote, or regenerates the history ledger.

```sh
knit log
knit log --all --oneline -n 20
knit log --all --project demo --repo api
knit log --all --view backend --since="2 weeks ago"
knit log --all --grep="authentication" --oneline
```

Project inspection works from the workspace root even when several bundles are
open. Bundle inspection uses the usual explicit bundle, environment, worktree,
and workspace context rules. An ad-hoc bundle without a project remains
inspectable with `knit log`.

## Select repositories and views

Repeat `--repo` to select several repository IDs. The default matches entries
involving any selected repository. Use `--repo-match all` to require every
selected repository in the same entry.

```sh
knit log --all --repo api --repo web
knit log --all --repo api --repo web --repo-match all
```

Repository matching uses recorded history. Including a repository in a project's
default bundle shape does not mean it participated in every commit. Historical
repository IDs remain searchable after removal from the project.

`--view` resolves a named local view against the project's current repository
set. Personal views override shared templates of the same name. The view's
`base`, `include`, and `exclude` rules are the same as for bundle creation.
Selecting a view means "history involving this repository set", not "bundles
originally created from this view". The default bundle-creation view does not
implicitly filter history.

When `--view` and `--repo` are combined, only repositories in both selections
match. An empty selection produces no results.

## Choose the unit of history

`--group` controls the entry being selected and counted:

| Group | Entry |
| --- | --- |
| `commit` (default) | One recorded node or commit group within a bundle |
| `event` | One history event, such as one repository's commit |
| `bundle` | One bundle's recorded history |

For example, the first command below finds a single recorded group involving
both repositories; the second finds a bundle involving both, possibly in
different groups.

```sh
knit log --all --repo api --repo web --repo-match all
knit log --all --repo api --repo web --repo-match all --group bundle
```

Repository selection normally limits the displayed event details to those
repositories. Add `--full-context` to include companion events from other
repositories in each matching entry. This does not broaden which entries match.

```sh
knit log --all --repo api --full-context
```

## Combine filters and control output

Date bounds (`--since`/`--after`, `--until`/`--before`), message searches
(`--grep`), and repeated event kinds (`--kind`) combine with repository and view
selection. Repeated `--grep` patterns match any pattern unless `--all-match` is
set. Patterns use basic regular expressions by default, like Git; `-E` selects
extended regular expressions. `-i` ignores message case; `-F` treats patterns
as fixed strings. Matching examines each message line, so `^` and `$` anchor
individual lines.

```sh
knit log --all --kind commit.recorded --kind commit.reverted
knit log --all --since=2026-01-01 --until=2026-06-30 --grep=cache -i
knit log --all --grep=api --grep=timeout --all-match
knit log --all -E --grep='api|web'
```

Dates accept ISO 8601 timestamps, `YYYY-MM-DD` (midnight UTC), `now`, `today`,
`yesterday`, `tomorrow`, and relative forms such as `2 weeks ago`. Both bounds
are inclusive. This is a defined local date syntax, not Git's complete natural
language date parser.

The regular expression dialect does not support backreferences,
collation/equivalence classes, GNU buffer anchors, or PCRE extensions; unsupported
constructs report errors. Word/space escapes are available in basic mode;
extended mode uses POSIX bracket classes instead. Unicode matching is independent
of the process locale, and POSIX named classes use ASCII definitions.

Entries are newest first. Current-bundle commit groups follow the bundle's
recorded node sequence, consistent with `HEAD` selectors; their date bounds use
node timestamps. Project-wide and individual-event queries use recorded event
timestamps. Original event timestamps remain available in JSON payloads.
`-n`/`--max-count` limits entries after selection;
`--skip` skips matching entries. `--reverse` reverses the selected page, so
`-n 10 --reverse` shows the newest ten entries in chronological order.
Existing `--limit`, `-N`, and bare `-n` (ten entries) remain accepted.

`--oneline` prints a compact subject per entry. `--json` emits a JSON array with
entry IDs, bundle context, timestamps, messages, and recorded event payloads.
It does not mix terminal headings or progress messages into standard output.

Inspect a selected entry with `knit show <id>` in its bundle, or
`knit show --all <id>` across the project's preserved history. Project inspection
also accepts `--project` and `--json`. Recorded metadata remains available when
a checkout or Git object is missing; patches require local Git objects. `HEAD`
and relative selectors retain their current-bundle meaning.

## Local storage and refresh

Git owns commits, file contents, and patches. Knit retains portable history in
`.knit/history/<project>.history.jsonl` and bundle metadata in `.knit/bundles/`.
The SQLite files beneath `.knit/cache/history/` are derived query indexes.
They need no database server or configuration and are not synchronized.

The next query incorporates appended history and detects rewritten ledgers.
Removing the cache causes automatic reconstruction from the local ledger;
events from deleted bundles survive because reconstruction uses preserved
history, not just the remaining bundle artifacts.

Use explicit commands to change what history is available:

```sh
knit sync                            # record external Git work in this bundle
knit history refresh                 # record missing events from local bundles
knit history refresh --rebuild       # refresh event detail, preserving orphan events
knit sync pull --history             # download recorded history from a sync remote
```

`knit history list` remains available as a flat event listing for compatibility.
It uses the same indexed query engine and does not implicitly refresh history.

## Hosted history

The project's History page offers the same repository-set selection: choose
multiple repositories, match any or all, or select a saved view. Personal views
override shared templates with the same name. Combining a view with explicit
repositories takes their intersection; an empty intersection has no matches.

In the Bundles reading, "all repositories" means they participated somewhere
in the same bundle. In the Events reading, they must participate in the same
recorded group. Companion context keeps other visible repositories in matching
entries; turning it off narrows the displayed details. Search, date bounds,
and existing entry filters remain available.

The hosted date controls include the entire selected UTC day. CLI date-only
bounds represent midnight UTC; use an explicit timestamp for a precise cutoff.

Hosted filtering runs against the server's existing history database before
pagination. Repository visibility still applies to every matching entry and
companion event. The local SQLite cache is not uploaded: local inspection uses
the locally preserved ledger, while the hosted page uses synchronized history.
Run `knit sync push --history` or `knit sync pull --history` explicitly to move
records between them.
