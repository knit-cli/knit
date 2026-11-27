# Building a Knit host

This document is for authors of agent harnesses, editors and dashboards that
want to drive Knit — not for people using the `knit` command. It describes the
surface a host may rely on, in any language.

The contract is one sentence: **read the artifacts, mutate through the CLI.**

Knit's own state is a set of JSON files under `.knit/`. They are the read
surface, they have published schemas, and reading them needs no process. Every
change goes through the `knit` command instead, so one implementation owns
locking, git, and the ledger.

The smallest useful integration is three steps: run `knit bundle "<title>"` in
a workspace, start the agent with its working directory set to
`.knit/worktrees/<slug>/` and `KNIT_SESSION` set to the conversation id, and
poll `knit status --json` from that directory to show what changed. Knit needs
nothing else from the host: `git` on `PATH`, plus the code host's CLI only if
the host offers publishing.

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

The schemas ship in the repository under `schemas/`, and `knit schema print
<name>` prints the copy built into the installed binary. They are additive: new
fields appear over time, so parse permissively and ignore what you do not know.

A bundle artifact is the source of truth for a feature. Its `nodes` array is
the ledger — an append-only history of what happened to that bundle.

Watch `.knit/bundles/` and `.knit/config.json` for changes if the host wants a
live view. Knit writes an artifact to a temporary file and renames it over the
old one, so a reader sees either version and never half of one. (On Windows,
if that rename fails because another process has the file open, Knit deletes
and renames, so there is a brief window where the file is absent — retry
rather than treating absence as deletion.)

## 2. Commands that answer in JSON

Some questions need Knit's own resolution logic (which bundle is active, what
git says right now). Those have machine-readable output:

```sh
knit status --json              # resolved bundle, state, per-repo git state
knit bundle list --json         # every bundle, with its artifact path
knit clone <project> --json     # what a clone produced (--view/--repo scope it)
knit bundle pull <slug> --json  # what a pull produced
knit remote projects --json     # projects visible to a remote token
knit remote views <project> --json  # the user's saved views for one, before cloning
knit remote auth-status <name> --json
```

These documents are contracts and change only deliberately. Everything else the
CLI prints is for humans: do not parse it. The two a host polls have published
schemas, additive like the artifact ones: `schemas/status.schema.json` and
`schemas/bundle-list.schema.json` (`knit schema print status`, `knit schema
print bundle-list`).

When a `--json` command fails, it exits non-zero, prints a human `Error:` line
on stderr, and prints one JSON document on stdout
(`knit schema print error`):

```json
{ "error": { "kind": "noRemote", "message": "No sync remote configured. Run `knit remote add <name> <url>` first." } }
```

`kind` is one of `noWorkspace` (not in a Knit workspace), `noBundle` (no
bundle resolves from here), `noRemote` (no sync remote configured, or no remote
by that name), `noToken` (the remote has no token), `http` (the remote could
not be reached or answered with an error), `notFound` (the remote has no such
project or bundle), `usage` (the command line was rejected) or `other`. New
kinds may be added; treat one you do not know like `other`. The exit status is
2 for `usage` and for a failed handoff readiness check, and 1 otherwise.

A few commands answer with a report that itself says what failed — `knit land
validate`, and with `--json` `knit land preflight`, `knit land apply`, `knit
land resume`, `knit land recover --apply`, `knit handoff probe`, `knit fork`
and `knit auth status --check`. When they fail after printing that
report, the report is the one document on stdout and no `error` envelope
follows. So: on a non-zero exit, read stdout as JSON; if it has an `error`
object, that is the failure, otherwise it is the command's own report.

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

`behind` appears instead when the checkout has not caught up with the ledger:
the recorded head was made on another machine and `knit pull` brings it in.
Both are a short note for display, such as `unrecorded commits: 2` or
``behind recorded head by 2; run `knit pull` ``. A checkout that is behind and
has commits of its own gets
``behind recorded head by 2, with 1 commit(s) not on it; run `knit pull --feature --rebase` ``:
the note always names the command that brings the checkout onto the recorded
head.

`knit status` never calls the host. Once a review is recorded for a repo, its
entry carries `review`, the review state Knit last recorded (by `knit publish
create` or `knit publish sync`), and `publications` gains `readiness` and
`freshForSeconds`:

```json
"review": {
  "number": 12,
  "url": "https://github.com/acme/backend/pull/12",
  "state": "open",
  "mergeable": "conflict",
  "verdict": "action needed: conflict",
  "blocked": true,
  "openGates": [
    { "kind": "conflict", "state": "blocked", "actor": "you",
      "summary": "conflicts with its base: run `knit land update`" }
  ],
  "recordedAt": "2026-03-07T12:00:00.000Z",
  "ageSeconds": 259200,
  "stale": true
}
```

- `mergeable` is `conflict`, `behind`, `clean` (gates recorded, none reported a
  conflict) or `unknown` (no gates recorded); null once the review is merged or
  closed. `verdict` is `ready` or the open gates' headline, null when unknown.
- `recordedAt` is when the recorded gates last changed; a refresh that finds
  the same gates keeps it, so `ageSeconds` can only overstate the age.
- `stale` is true when the record is older than `freshForSeconds` or commits
  were recorded in that repo after it.
- `readiness` is `blocked` (a recorded gate needs your action), `unverified`
  (an open review is unknown or stale) or `ready`. Only `ready` means the
  record supports landing; otherwise ask the host with
  `knit publish status --live`.

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
| `KNIT_ACTOR_SESSION`, `KNIT_ACTOR_LABEL`, `KNIT_ACTOR_EMAIL` | The acting human on a shared environment. Recorded as `actor` when `KNIT_ACTOR_SESSION` is set; the other two are ignored without it. Optional; single-user setups leave them unset. |

Both are read from the environment of the `knit` process, so a host exports
them when it spawns commands and agents. Absent variables simply mean the node
carries no session or actor — nothing fails.

The older names `T3_ACTOR_SESSION`, `T3_ACTOR_LABEL` and `T3_ACTOR_EMAIL` are
still accepted. When `KNIT_ACTOR_SESSION` is set, all three values come from
the `KNIT_ACTOR_*` names; otherwise they come from the `T3_ACTOR_*` names.

Git authorship is separate and ordinary: set `GIT_AUTHOR_NAME` /
`GIT_AUTHOR_EMAIL` (and the committer pair) as you would for any git process.
Knit does not overwrite them.

## 5. Mutating

Run the `knit` binary ([install](../README.md#install)).

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
- **UI.** There is no rendering in Knit.
- **Identity for hosted forges.** `knit remote` holds a token per remote; how a
  host obtains that token is outside Knit.

## 7. What Knit records about the machine and the person

Knit sends nothing anywhere until a sync remote is configured, but its
artifacts hold some identifying data from the start. In a bundle artifact
(`.knit/bundles/<slug>.bundle.json`):

| Field | What it holds | How to avoid it |
| ----- | ------------- | --------------- |
| `nodes[].origin.hostname` | The machine's name, on every node: `HOSTNAME`, else `COMPUTERNAME`, else the output of `hostname` | Set `HOSTNAME` to any label in Knit's environment. Keep it stable per machine: without `KNIT_ENVIRONMENT_ID`, handoff tells machines apart by hostname and platform |
| `nodes[].origin.platform` | OS and architecture, such as `darwin/arm64` | None |
| `nodes[].origin.environmentId` | `KNIT_ENVIRONMENT_ID`, only when set | Leave it unset |
| `nodes[].sessionId`, `nodes[].actor` | The variables in [section 4](#4-attribution), only when set | Leave them unset |
| `commitGroups[].author` | Name and email of the git author of the recorded commits | Use the git identity you want published, such as a no-reply address |
| `repos[].path` | Absolute path of each source repo, which usually contains your OS user name | None |
| `publications[].author` | The code host account that opened each review: login, and the name, avatar and profile URL when the host reports them | None |

Nodes keep what they recorded; changing a variable later does not rewrite
older nodes. Project files (`.knit/projects/<id>.project.json`) also hold the
absolute repo paths.

Where it goes:

- **Sync remote.** Once one is configured, `knit push`, publish and land
  upload the bundle artifact as it is, with every field above. The project
  record is uploaded with repo paths blanked. Project history events
  (`.knit/history/`) carry bundle titles, commit messages, branches and SHAs,
  but none of the fields above.
- **Git commits.** `knit commit` adds two trailers, `Knit-Group: <id>` and
  `Knit-Bundle: <slug>`, and leaves authorship to git; `knit revert` also
  adds `Knit-Reverts: <node id>`. Nothing else is added, so pushing to a code
  host sends what plain git would, plus those lines.
- **Pull request bodies.** Knit's block names the bundle id and title and
  links the other reviews in the bundle, and the bundle's page on the sync
  remote when there is one. It holds no hostname, path or session.
