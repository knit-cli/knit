# Building a Knit host

This document is for authors of agent harnesses, editors and dashboards that
want to drive Knit — not for people using the `knit` command. It describes the
surface a host may rely on, in any language.

The contract is one sentence: **read the artifacts, mutate through the CLI.**

Knit's own state is a set of JSON files under `.knit/`. They are the read
surface, they have published schemas, and reading them needs no process. Every
change goes through the `knit` command instead, so one implementation owns
locking, git, and the ledger.

TypeScript hosts can skip the plumbing: [`knit-typescript-sdk`](https://www.npmjs.com/package/knit-typescript-sdk)
is the reader, watcher, CLI runner and action catalog this document describes,
already written.

## 1. Read surface

The workspace layout is in [reference.md](reference.md#storage). What a host
usually reads:

| Path                              | What it is                                             | Schema                     |
| --------------------------------- | ------------------------------------------------------ | -------------------------- |
| `.knit/bundles/<slug>.bundle.json` | One feature: repos, branches, commit groups, ledger nodes | `schemas/bundle.schema.json` |
| `.knit/projects/<id>.project.json` | A reusable repo template: repos, bases, landing config | `schemas/project.schema.json` |
| `.knit/config.json`               | Workspace fallback state and settings                  | `schemas/config.schema.json` |
| `.knit/views/<project>.views.json` | Saved bundle shapes, per user                          | `schemas/views.schema.json` |
| `.knit/land-plans/<slug>.land.json` | The editable landing plan for a bundle                | `schemas/land-plan.schema.json` |
| `.knit/land-runs/*.run.json`       | What a landing actually did                            | `schemas/land-run.schema.json` |
| `.knit/merge-runs/<run-id>.json`   | What a merge actually did                              | `schemas/merge-run.schema.json` |

The schemas ship in the repository under `schemas/`. They are additive: new
fields appear over time, so parse permissively and ignore what you do not know.

A bundle artifact is the source of truth for a feature. Its `nodes` array is
the ledger — an append-only history of what happened to that bundle.

Watch `.knit/bundles/` and `.knit/config.json` for changes if the host wants a
live view. Knit writes an artifact to a temporary file and renames it over the
old one, so a reader sees either version and never half of one. (On Windows,
where rename-over-existing is refused, there is a brief window where the file
is absent — retry rather than treating absence as deletion.)

## 2. Commands that answer in JSON

Some questions need Knit's own resolution logic (which bundle is active, what
git says right now). Those have machine-readable output:

```sh
knit status --json              # resolved bundle, state, per-repo git state
knit bundle list --json         # every bundle, with its artifact path
knit clone <project> --json     # what a clone produced
knit bundle pull <slug> --json  # what a pull produced
knit remote projects --json     # projects visible to a remote token
knit remote auth-status <name> --json
```

These documents are contracts and change only deliberately. Everything else the
CLI prints is for humans: do not parse it.

`knit status --json` is the one a host polls:

```json
{
  "bundle": "add-search",
  "resolvedFrom": "cwd",
  "state": "open",
  "repos": [
    {
      "id": "backend",
      "branch": "knit/add-search",
      "expectedBranch": "knit/add-search",
      "worktree": ".knit/worktrees/add-search/backend",
      "mode": "worktree",
      "checkoutPresent": true,
      "status": "modified",
      "wrongBranch": false
    }
  ],
  "publications": { "reviews": 0, "repos": 1 }
}
```

`unrecorded` appears on a repo when a tracked branch moved outside Knit and the
ledger has not caught up; `knit sync` records those commits.

## 3. Bundle context

Every bundle-scoped command resolves which bundle it is talking about, in this
order:

1. `--bundle <slug>`
2. `KNIT_BUNDLE` in the environment
3. the current working directory, when it is inside `.knit/worktrees/<slug>/`
4. the workspace fallback in `.knit/config.json`

A host with more than one bundle in flight should use (1) or (2) and never rely
on (4): the fallback is shared, and Knit refuses ambiguous mutations when
several bundles are open.

**The one architectural assumption.** Bind a conversation's working directory to
a bundle worktree root, `.knit/worktrees/<slug>/`. That directory holds every
repo checkout for the feature side by side, plus a generated `AGENTS.md`. A
coding agent started there needs no Knit-specific adapter: it sees an ordinary
directory of repos, and its file edits land in the right branches. This is what
makes a harness a Knit host cheaply.

## 4. Attribution

Knit stamps every ledger node with the identity in the environment, so the
history answers "which conversation did this".

| Variable                                          | Effect                                                              |
| ------------------------------------------------- | -------------------------------------------------------------------- |
| `KNIT_SESSION`                                    | Opaque session id. Recorded as `sessionId` on every node the command writes. Set it per conversation. |
| `T3_ACTOR_SESSION`, `T3_ACTOR_LABEL`, `T3_ACTOR_EMAIL` | The acting human on a shared environment. Recorded as `actor`. Optional; single-user setups leave them unset. |

Both are read from the environment of the `knit` process, so a host exports
them when it spawns commands and agents. Absent variables simply mean the node
carries no session or actor — nothing fails.

The `T3_ACTOR_*` prefix is historical and kept for compatibility with harnesses
that already export it.

Git authorship is separate and ordinary: set `GIT_AUTHOR_NAME` /
`GIT_AUTHOR_EMAIL` (and the committer pair) as you would for any git process.
Knit does not overwrite them.

## 5. Mutating

Run the `knit` binary. It is on `PATH` after `brew install knit-cli/tap/knit`.
(The TypeScript SDK also honours a `KNIT_BIN` override when the binary lives
somewhere else; that convention is the SDK's, not the CLI's.)

Rules that keep a host out of trouble:

- **One command at a time per workspace.** Knit takes a lock per bundle
  (`.knit/locks/<bundle>.lock`); serialize your own calls rather than racing it.
- **Do not write `.knit/` yourself.** A host that edits a bundle artifact
  directly loses the ledger entry that explains the change.
- **Do not amend a commit Knit recorded.** `git commit --amend` after
  `knit commit` strands the recorded `commit.group`; make a new commit instead.
- **Exit codes matter, output does not.** Treat non-zero as failure and show
  the process's stderr; do not scrape success out of stdout.

The everyday verbs a host wraps are `knit bundle`, `knit add`, `knit commit`,
`knit push`, `knit publish create`, `knit land`. Their behavior is in
[reference.md](reference.md).

## 6. What Knit deliberately leaves to the host

- **Which agent runs, and how.** Knit provides the directory and the ledger; it
  never spawns an agent.
- **When to create a bundle.** Naming, one-bundle-per-conversation, or one per
  ticket — all host policy.
- **UI.** There is no rendering in Knit. `knit-typescript-sdk/ui` offers the
  headless decisions (status rollups, list truncation, picker semantics) if you
  want them.
- **Identity for hosted forges.** `knit remote` holds a token per remote; how a
  host obtains that token is outside Knit.
