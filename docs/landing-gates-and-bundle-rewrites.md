# Library releases, landing gates, fork leases and bundle rewrites

Status: decided 2026-09-30. This note records why the features below look the
way they do. `docs/reference.md` is the user-facing description.

## The workflow this serves

A common multi-repository pattern:

1. A shared **library** repository and one or more **consumer** repositories
   change together in one bundle.
2. The library review merges once its checks are green.
3. A maintainer **releases** the library (tag, publish a crate/package).
4. Each consumer gets a **follow-up commit** that moves its dependency to the
   new release.
5. Each consumer merges once its checks are green again.

Before this change Knit could only express steps 2 and 5. Step 3 had no
conditional home, step 4 broke the reviewed-head pins of every plan and run,
and the consumer was often a repository the bundle tracks without being a
project member.

## Problems 1–3: one landing design

### Chosen approach: dependency edges that compile into ordinary plan steps

A project declares library → consumer edges once:

```json
"landing": {
  "merge": { "waitForChecks": true, "requiredChecksOnly": false },
  "dependencies": [
    {
      "library": "core-lib",
      "consumers": "*",
      "release": { "instructions": "Release core-lib from main." },
      "bump": {
        "instructions": "Move the core-lib dependency to the new release, commit and push.",
        "paths": ["Cargo.toml", "Cargo.lock", "**/Cargo.toml"]
      }
    }
  ]
}
```

Plan generation expands an edge **only when the bundle changed the library and
the plan merges it through its review** (`merge_pr`):

| Generated step | Type | Needs | Meaning |
| --- | --- | --- | --- |
| `merge-core-lib` | `merge_pr` | — | unchanged; waits for checks, merges the reviewed head |
| `release-core-lib` | `manual`, `acknowledge: "resume"` | `merge-core-lib` | the run pauses until a person confirms the release |
| `bump-<consumer>` | `await_update` | `release-core-lib` | the run pauses until the consumer's review has a follow-up commit |
| `merge-<consumer>` | `merge_pr` | `bump-<consumer>` | waits for checks on the bumped head, merges exactly that head |

`consumers` is `"*"` (every other repository this plan merges) or a list of
project repository ids. Without `release` the consumers wait for the library
merge; without `bump` they wait for the release (or the merge). A bundle that
does not change the library gets none of these steps, and needs that point at
them are dropped (see "Conditional needs").

The result is **one plan and one run**. The run pauses at a gate instead of
failing; `knit land resume` continues it. Everything a maintainer needs to see
— order, gates, allowed bump paths — is in the plan file and in `knit land`
output.

### New plan semantics

- **`await_update` step** (new). Reads the live head of the repository's review.
  - Unchanged (still the reviewed head): the run pauses with the step's
    instructions.
  - Changed: the new head must contain the reviewed head (the bump adds
    commits on top; reviewed work is never replaced), and when `paths` is set
    every changed file must match one of its globs. The step then **re-pins**
    the repository: later steps for it (the review merge) use the new head, and
    the merge is refused by the forge if the head moves again.
  - The accepted commits are recorded in the bundle ledger (`git.observed`),
    so a bump made outside this checkout is not lost.
- **`manual` steps with `acknowledge: "resume"`** (new option). The run pauses at
  the step instead of prompting on a terminal; `knit land resume --acknowledge
  <step> [--note <text>]` records the acknowledgement. The default
  (`"terminal"`) keeps the existing interactive prompt. `--acknowledge` also
  satisfies an `await_update` gate whose head already contains the bump (for
  example after the plan was regenerated).
- **Paused runs.** Run `status: "paused"` with `pause: {step, instructions,
  since}`. A paused run is quiescent: receipts are persisted, no process runs,
  so the local lock and hosted landing ownership are released exactly as after
  a clean stop, and `knit land resume` claims them again. Recovery rules are
  unchanged. `knit land apply` and `knit land resume` exit 0 on a pause and
  print the next command; `--json` output carries `status: "paused"`.
- Plans that contain gates require executor `0.6` and capability
  `landing-gates`, so older executors refuse them instead of skipping them.
  Gates need a local runner (like other manual steps), but no terminal.
- **Resume after a bump.** A resumed run normally refuses "bundle changed since
  this run started". Repositories with an `await_update` step are exempt from
  the head comparison, because that step validates the new head itself. Every
  other change (base, branch, review, scope) is still refused.

### Conditional steps and needs (problem 1)

- `landing.steps[]` (and `targets.<branch>.steps`, `lanes.<name>.steps`) accept
  `whenChanged`, with the same meaning as for deployments: the step is part of
  the plan only when one of the listed repositories changed (`"*"` = always).
- **Conditional needs.** Generation knows every step id the configuration
  *could* produce: `merge-<repo>` for project and bundle repositories,
  deployment ids and their `-build`/`-verify` steps, custom step ids, and
  dependency gates. A need that names one of those, but which this bundle did
  not produce, is dropped. A need that names anything else is still an error
  (typo protection), now with a message saying which kind of id was expected.

### Bundle-only repositories (problem 3)

- Bundle repositories that are not project members take part automatically in
  wildcard rules: `consumers: "*"`, `whenChanged: ["*"]`, and
  `merge.repoOrder` (unlisted repositories merge after listed ones).
- Project configuration that **names** a repository must name a project
  repository. Otherwise the same project file would be valid for one bundle
  and invalid for the next. Naming a non-member is refused with the fix:
  `knit project add <id> <path> --observe` (observed repositories are not added
  to new bundles, so this changes nothing else).
- `execution.repoOrder` (strict repository sequence) keeps refusing an
  unlisted repository, now naming the bundle-only case and the same fix.

### Alternatives considered

- **Two runs with a regenerated plan in between** (merge the library, stop,
  regenerate after the bump). No schema change, but the flow the maintainer
  reviews is split across two documents, and the second plan has no memory of
  the first. Kept only as the fallback when a paused run was superseded by
  another landing in the project.
- **A "renew" operation that rewrites the plan mid-run.** Breaks the rule that
  a run executes one immutable plan hash, which recovery and hosted receipts
  rely on. Rejected.
- **Blocking gates** (the process waits until the release/bump appears).
  Holds a terminal, the local lock and project-wide hosted ownership for hours.
  Rejected in favour of pausing.
- **Only generic building blocks** (`whenChanged` + `needs`). Cannot express
  "for each changed consumer, a bump gate before *its* merge, only when the
  library changed": that is an AND over two repositories per consumer.
  `dependencies` is the small first-class form; it compiles to generic steps,
  so the plan stays editable.
- **Accept any repository id in project config.** Silently never matching is
  what a typo looks like. Rejected; see the observed-member fix above.

### Schemas

- `schemas/project.schema.json`: `landing.dependencies[]`;
  `whenChanged` on landing steps.
- `schemas/land-plan.schema.json`: step type `await_update` (`repoId`,
  `instructions`, optional `paths`); `acknowledge` on manual steps;
  executor version `0.6`.

### Hosted sync, history, locks

- Plans and runs sync unchanged. Hosted validation runs the Knit CLI built
  from `main`, so it learns the new step types when Knit lands and the server
  redeploys (the configured landing order does that).
- The run's result bundle records bump commits; bundle history is derived from
  the ledger as before.
- Ownership: released on pause (quiescent), re-claimed on resume. If another
  landing ran in the project meanwhile, the existing "superseded" rule refuses
  the old run; Knit then says to regenerate the plan and pass
  `--acknowledge` for gates that are already done.

## Problem 4: `knit push --force-with-lease` for fork checkouts

Cause: in a split fetch/push remote Knit leased against its own receipt ref
only. A branch last pushed by plain `git push`, an older Knit or another
machine has no receipt, so the lease said "branch must not exist" and Git
answered "stale info". The native tracking ref cannot be used: in a split
remote it may hold the upstream branch of the same name.

Decision: every leased push resolves an explicit lease.

1. Knit's push receipt (or, for a plain remote, the native tracking ref).
2. Otherwise, or when it disagrees, the actual tip from `git ls-remote` of the
   push URL — accepted only if this bundle's ledger recorded that commit or
   the local feature branch's reflog contains it.
3. Anything else is refused as a concurrent update, with the commands to fetch
   and inspect it.

The push itself still carries `--force-with-lease=<ref>:<sha>`, so an update
between the lookup and the push is refused by the remote.

## Problem 5: rewriting a bundle

Decision: rewrites update the bundle's `commitGroups`, repository heads and
bases in place, and append one audit node. Nothing is hand-edited.

- `knit squash [-m <message>]`: one commit per repository on the recorded
  base, one commit group.
- `knit rebase [--squash] [-m <message>] [--offline]`: fetch each repository's
  base (the upstream target for forks) and rebase onto it; `--continue` /
  `--abort` across repositories after a conflict.
- `knit sync` recognises a rebase done with plain Git: commits reachable from
  the upstream base are the new base, not bundle work, and `baseSha` moves.

Ledger shape (compatible with the hosted history projection from issue 11):

1. A `git.observed` node with `movement: diverged|rewound`, `droppedCommits` =
   the replaced bundle commits, `baseBefore`/`baseAfter`, and a `rewrite`
   descriptor (`kind`, `supersededGroups`). The hosted projection already
   removes dropped commits, so superseded groups are hidden by default and
   remain visible as superseded history.
2. One `commit.group` node per resulting group (new ids, same messages;
   rebase maps commits by patch id), so the new commits are recorded work with
   their group, not "observed" commits.
3. `commitGroups` holds only the resulting groups; the superseded group records
   move into the audit node.

The same retirement applies when `knit commit` or `knit sync` observes a manual
rewrite (soft reset, amend, rebase): groups whose commits were all dropped move
into the observation node.

Publishing a rewrite needs `knit push --force-with-lease` (branches and bundle
artifact together), which the lease fix above makes work for forks.
