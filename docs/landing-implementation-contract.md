# Landing implementation contract

Implementation target for the complete editable landing/recovery feature. This refines `landing-and-recovery-design.md`; examples use synthetic repositories only. The same saved document must execute locally and on a hosted runner.

## Command output

Executing a command prints its step, phase, and executable, then streams stdout and stderr as they arrive. Output does not require `interactive: true`. Noninteractive command receipts retain bounded output, including failures; interactive commands keep their direct terminal connection and do not record terminal transcripts. Capture-adapter stdout remains private JSON; its diagnostic stderr is live. In `--json` mode, live output goes to stderr so stdout remains one machine-readable result.

Landing's Git fetch, merge, push, and checkout operations also stream output and record command receipts under the step's `attempts`. Credential values are redacted across pipe-read boundaries. Git plumbing queries remain quiet, and commands outside landing keep their existing output behavior. Command labels omit argv and environment values; the full authored recipe is available in the saved plan.

## Plan format

New plans use `schemaVersion: "0.2"`, `kind: "KnitLandPlan"`. Preserve legacy `0.1` reading and semantics; never silently reinterpret legacy rollback. The existing plan fields remain: id, bundleId, sourceProjectId, createdAt, provider, targetBranch/lane/targetBranches, terminal, changedRepos, bundleHeads, requireChecks, steps. Add:

- `bundleFingerprint` and `projectFingerprint`: lowercase SHA-256 of canonical recursively-key-sorted semantic source projections (defined below), excluding machine-local paths and timestamps. Plan hash is the same algorithm over the complete saved plan document (no self hash field).
- `onFailure`: `stop` or `recover` for v0.2.
- `maxParallel`: positive integer, default 4.
- Optional `workflow`: nested `{sequence: [...]}`, `{parallel: [...]}`, or `{step: "id"}`. When present this is the ordering source; compilation yields effective `needs`. Otherwise the existing explicit `needs` graph is authoritative. Do not render list position as execution order for DAG mode.

Step types remain merge_pr, merge_branch, wait_checks, run, deploy; add first-class labels via `label` and `role` (build, verify, release, capture, custom). Existing exact argv, repoId, cwd, env, timeout, checkout and merge settings are retained. Portable cwd is repo-relative. Add `effect` (read_only, deployment, source, external) and `recovery`:

```json
{
  "mode": "command",
  "capture": {"command": ["./release", "inspect", "--json"]},
  "command": ["./release", "restore"],
  "verify": {"command": ["./release", "verify-restored"]},
  "idempotent": true
}
```

Recovery modes: command, none, manual, revert_pr. Manual requires a reason. Source merges default to revert_pr (proposes source compensation; never claims deployment restored). Read-only checks/builds default to none. Unspecified deployment/external recovery is manual, not automatically reversible. `onFailure: recover` requires executable capture, restore, and restore verification for every deployment/external effect; source PR proposals have a separately reported status.

Command specs support command argv, cwd, env, timeoutSeconds. Capture stdout must be a JSON object; it is durably stored before the forward command starts. Restore/verification receive that exact object in `KNIT_LAND_CAPTURE` and the captured JSON file path in `KNIT_LAND_CAPTURE_FILE`. Forward steps receive stable `KNIT_LAND_OPERATION_ID`, unique `KNIT_LAND_ATTEMPT_ID`, `KNIT_LAND_RUN_FILE`, and an optional JSON output-file path `KNIT_LAND_OUTPUT_FILE`. Prerequisite merge receipts and command outputs are available as JSON in `KNIT_LAND_INPUTS`. `KNIT_CHECKOUT_<REPO>` uses the native runtime uppercase/underscore suffix and points to that run’s pinned checkout; `KNIT_DEPLOY_CHECKOUT` names the operation cwd. Build/deploy steps for the same repo revision share their run-owned checkout so ordinary build outputs persist. Existing local `KNIT_ROOT`, `KNIT_BUNDLE`, and `KNIT_REPO` bindings remain available. No shell interpolation is implied for argv. Sensitive env values are runner bindings; do not persist credential values in shared artifacts.

Persist before/after every effect. Distinguish performed, already_satisfied, and uncertain. Failed command effects with a capture are eligible for declared idempotent restoration after process-tree quiescence; unknown crash effects require a probe or manual intervention. Never automatically rerun an uncertain non-idempotent command. Recover in reverse dependency order, only attributable effects. Block forward resume after recovery starts. Recovery has its own resumable receipts and reports restored/partial/manual/failed independently from source revert state.

Interrupted inverse reconciliation uses the declared `recovery.probe` with `KNIT_LAND_PHASE=probe-restore` and `KNIT_LAND_RECONCILE_ATTEMPT_ID` identifying the outstanding inverse attempt. Its JSON result must declare `quiesced: true`, the matching `attemptId`, and the original `phase` (`restore` or `verify-restored`). A generic forward probe cannot attest inverse quiescence. Missing or mismatched evidence blocks retry and ownership release; idempotence alone does not permit overlapping restorations.

Project metadata extends existing `landing.deployments[]` with label, build/verify command specs, `sourceRepos`, and recovery. `sourceRepos` explicitly lists additional repositories consumed by the operation (for example binaries embedded in an image). Generation preserves these dependencies across build/deploy/verify, requires their merge producers where present, and pins unchanged inputs to the recorded bundle revision or a run-wide resolved recipe base. Mutable source checkout paths are never accepted as a substitute for an immutable input revision. Support `landing.steps[]` for additional explicit command steps. Environment overrides remain `landing.targets` and `landing.lanes`. Trigger repositories are distinct from dependencies; builds consume all declared prerequisite merges. Preserve arbitrary accepted project fields when editing recipes.

### Portable source identity

Bundle fingerprint projection: `{id, projectId, repos, publications, changedRepos}`. Repositories sorted by id contain only present `id, remote, baseBranch, baseSha, featureBranch, headSha`; publications contain only present `repoId, provider, kind, number, url, baseBranch, headBranch` with deterministic ordering. `changedRepos` is sorted publish scope. Project projection: `{id, landing, repos}` with repo entries sorted by id containing only present `id, remote, baseBranch`. Optional missing or null fields are omitted within entries; outer projection keys are always present. Publication status, paths, timestamps, local credentials, and sync receipts do not change source identity. Whole-plan hashes still include the entire immutable saved document.

## Opt-in repository release policies (executor 0.4)

Three optional plan keys extend v0.2 — `execution`, `preflight`, `integrationSources` — and any one of them requires `requiredExecutorVersion: "0.4"`. Generation, `knit land validate`, and both executors refuse every other combination, and a schema 0.1 document naming any of these keys is refused outright: legacy typed execution would silently discard the unknown field and run without it. Projects declare `landing.execution` and `landing.preflight`; `landing.lanes.<name>` and `landing.targets.<branch>` may override each key individually. An absent key in a scope inherits the project root; an explicit `null` disables the root policy for that scope. Generation copies the resolved policy into the plan. The matching `requiredCapabilities` markers are `repository-sequence` for `execution`, `mergeability-preflight` for `preflight`, and `integration-sources` for `integrationSources`. Saved revisions stay immutable: adopting or changing a policy authors a new reviewed revision. Plans without these keys generate and execute exactly as before, and remain byte-compatible.

`execution.mode = "repository_sequence"` with `repoOrder` compiles every repository's merge/build/deploy/verify into one explicit workflow sequence, groups in declared order, in-group steps ordered by declared needs. `repoOrder` entries are nonempty and unique. Generation and validation reject, by name: an undeclared repository, a step depending on a later repository (`needs` or `requires`), a dependency cycle, and a parallel cross-repository workflow posing as a sequence — the saved workflow must actually serialize the named repositories. The declared order is enforced, never advisory.

`preflight.mergeability = "all"` orders execution: every pending `merge_branch` is simulated against a freshly resolved target tip in an isolated temporary checkout before any remote ref moves, any lock beyond local ownership is claimed, or any effectful command runs. One conflict anywhere fails the whole preflight. On resume, still-pending merges are revalidated; succeeded steps keep their receipts, and a durably performed merge is reconciled from its recorded revision rather than replayed against the tip its own merge moved. Each simulation pins its target tip on the run receipt as `expectedTarget`; execution re-fetches the target, requires exact equality (missing or unreachable remote is a typed no-effect refusal: quiesced, safely retryable), asserts the merge result descends from the expected tip, and publishes as a conditional `--force-with-lease=<branch>:<expected>` update. The guard can only ever extend history; drift is refused, never overwritten. Reconcile target drift and resume the existing run: pending merges are revalidated, while completed merges and deployments retain their receipts. Changing a source pin requires a new plan and run. With both policies enabled, all preflight first, then each repository's merge, build, deploy, and configured verify, is this plan's opted-in shape — not universal deployment behavior. Without the policy the executor performs no global branch scans; explicit integration sources still require provenance verification. Existing required-check and executor validation remain in effect.

`integrationSources` maps a repository to an explicit `{branch, sha}` merged instead of the reviewed bundle head. Source selection is independent of the destination. It applies only to `merge_branch` steps; a repository merging its recorded review refuses it by name. The bundle fingerprint, recorded pins, and review objects stay untouched. Provenance is verified independently of review state at authoring, at preflight, and at execution: the live branch tip must equal the pinned full lowercase SHA (40 or 64 hex digits) and the pin must contain the reviewed bundle head as an ancestor; drift refuses the run before any effect. Required checks are evaluated against the effective pinned head, so verdicts recorded at the reviewed feature head read as stale, never green for code that will not run.

`knit land source --plan PLAN --repo api --branch compatibility [--sha SHA] [--repo-root PATH] --out NEWPLAN` authors a pin: it resolves the remote branch to its exact current SHA, verifies the reviewed bundle head is contained, and writes a NEW plan with the pin, executor 0.4, and the `integration-sources` capability. The input plan is never modified; `--out` must not exist, must not be the input, and must not be a plan revision. Saved revisions and run snapshots stay immutable, so reselecting a source authors another new plan; an existing run keeps its original plan hash and source pin. Conflicts between an integration branch and its target are the operator's to resolve in their own compatibility flow; the landing never merges anything into a repository's feature branch to clear the way.

`knit land preflight --plan PLAN [--from-artifact BUNDLE] [--project-file PROJECT] [--repo-roots ROOTS] --json` is read-only: static graph validation reported separately as `structural`, distinct from the `live` half — fresh target tips, provenance, and the conflict simulation only when the plan opted in; without the policy pending merges are listed informationally, while explicit integration sources still require provenance verification. Report shape: `{valid, planHash, bundleId, structural, live, checks[], errors[], handoff}` with per-step checks `{repoId, sourceBranch, sourceSha, targetBranch, targetSha, status, conflicts[]}`; exit status follows `valid`.

For executor-0.4 plans the command environment is authoritative: `KNIT_LAND_ENVIRONMENT` (the lane name or target branch landed into), `KNIT_SOURCE_SHA` (the selected integration input — pinned integration SHA or reviewed bundle head; not the merge output), `KNIT_TARGET_BRANCH` (the repository's resolved merge destination), and `KNIT_REV` (the exact merged revision the step's pinned checkout runs from). Recipe env is applied first, so a recipe can supplement the environment but never rewrite these pins. Older plans retain their previous handling of these four variables. The existing `KNIT_DEPLOY_CHECKOUT` variable is the actual command working directory resolved from the pinned checkout and recipe `cwd`, and also applies to older plans.

## CLI and machine interface

The CLI owner implements the following (additional explicit flags are fine, breaking renames require coordination):

- `knit land plan --from-artifact BUNDLE --project-file PROJECT --out PLAN [--json]` generates the same v0.2 plan as local `knit land`. Global `--target`/`--lane` apply.
- `knit land validate --plan PLAN [--from-artifact BUNDLE] [--project-file PROJECT] --json` emits `{valid, errors, waves, recovery}`. With source inputs supplied, validate fingerprints and terminal coverage too. Validation does not run commands.
- `knit land apply --plan PLAN --from-artifact BUNDLE --project-file PROJECT --repo-roots ROOTS --run-out RUN --out BUNDLE_OUT` executes the exact saved plan. ROOTS is JSON mapping repo IDs to absolute runner-owned paths. Commands require all needed bindings before any mutation. Hosted execution never silently falls back to merge-only.
- `knit land recover --plan PLAN --run RUN --from-artifact BUNDLE --repo-roots ROOTS --run-out RUN_OUT --out BUNDLE_OUT` executes the recovery graph. With no apply flag it may preview; the hosted wrapper passes `--apply` explicitly.
- Local `knit land`, `apply`, `resume`, and recovery support the same v0.2 documents, durable runs, and graph rendering. Old plans remain supported.

Machine results and run/bundle output files must be flushed on failure as well as success. Logs may go to stderr; `--json` output is exclusively JSON. Runs pin plan hash and embed or reference an immutable plan snapshot. Commands run from isolated pinned checkouts or under exclusive checkout locks. Local filesystem locks serialize environment ownership on one machine; synchronized plans also require hosted ownership before execution.

## Hosted API

All paths below are under the existing authenticated API prefix. IDs use existing bundle/project resolution conventions. Responses use the existing `{data: ...}` envelope.

Plan record: `{id, bundleId, revision, hash, parentHash, plan, insertedAt, updatedAt}`. Revisions are immutable; save uses compare-and-swap against the destination's latest revision. Track destinations independently. Recipe changes and source bundle changes mark plans stale.

- `GET /bundles/:bundle_id/landing-plans`: all revisions (newest first).
- `GET /bundles/:bundle_id/landing-plans/latest`: latest record or null; optional target/lane selection.
- `POST /bundles/:bundle_id/landing-plans/generate`: `{targetBranch?, lane?, expectedRevision?, preview?}`. Calls Knit generation with stored bundle+project. `preview:true` returns an unsaved candidate record; otherwise saves with CAS (initial expectedRevision 0).
- `POST /bundles/:bundle_id/landing-plans`: `{plan, expectedRevision}` saves a Knit-validated immutable revision; conflict => 409.
- `POST /bundles/:bundle_id/landing-plans/validate`: `{plan}` invokes Knit validation against current source inputs.
- `GET /landing-plans/:id`: exact immutable record.
- `POST /bundles/:bundle_id/runs/landing`: existing endpoint, metadata `{planId, planHash}` pins exact saved revision at queue time. Worker never loads a different current plan. Existing callers without planId resolve/generate and pin a canonical plan before enqueue; no separate legacy hosted executor.
- `POST /runs/:id/recover`: queues recovery for that run with original plan and captured state. Run progress exposes persisted step results, captures as nonsecret references, failure, and recovery outcomes. Import partial output even when process exit is nonzero.
- `GET /projects/:project_id/landing-recipes`: `{landing, hash}`.
- `PUT /projects/:project_id/landing-recipes`: `{landing, expectedHash}` validates and merges only the landing key into metadata.knitProject with CAS.

Plan validation result: `{valid: boolean, errors: string[], waves: string[][], recovery: {automatic: boolean, manualSteps: string[]}}`. Additional fields are allowed. UI uses this result and its own immediate validation for edit feedback, but server validation is authoritative.

## Sync and run ownership API

- `GET /projects/:project_id/landing-artifacts`: `{plans: [{bundleSlug, revision, hash, parentHash, plan}], runs: [{bundleSlug, run}]}`. Read permissions and repository visibility apply.
- `POST /projects/:project_id/landing-artifacts`: accepts the same envelope; immutable hash/id and ancestry checks, conflicts 409; imported plans validated by Knit. Do not trust client claims of successful hosted runs or allow existing run receipts to be overwritten.
- `POST /projects/:project_id/landing-ownership`: `{bundleSlug, planHash, environment, owner, action, runIdentity?}` returns `{id, token}`. Unique active ownership conservatively serializes each project, so destination aliases cannot evade exclusion. `action` is apply/resume/recover; `runIdentity` names the prior Knit run for resume/recovery and must match the most recent execution generation. No automatic lease expiry may allow concurrent unmanaged shell commands. Ownership remains held until explicit completion or operator-verified recovery/handoff.
- `DELETE /projects/:project_id/landing-ownership/:id`: bearer ownership token in JSON `{token, quiescent: true}`; only owner can release after local/hosted process quiesces. Completion imports run receipts with top-level `ownership: {id, token}` before release. Retrying an interrupted claim supplies the previously issued private `id` and `token`; owner labels alone never authorize reclamation. Tokens are credentials and must not be written into shared plans or logs.

The CLI sync owner adds `knit sync push --plans` / `pull --plans` (includes related runs and associated project landing recipes), default routine sync coverage, conflict preservation, local `.knit/land-plans/` and `.knit/land-runs/` materialization, and a private sync index for hashes/revisions and ownership context. Synced-plan apply/recover must claim ownership; network failure is blocking for shared-environment execution. Never steal an existing lock automatically. Non-synced local-only plans continue under local locking and a durable latest-run generation. Recipe sync uses the same landing-recipes CAS endpoint, changes only the local project landing key, refreshes generated project guidance, and preserves both sides of divergent edits. If a conflict requires manual resolution, retain the local edits, adopt/pull the remote candidate as the new base, then reapply edits and push; do not silently adopt a conflicting parent revision.

## UI acceptance

Replace the bundle plan page with the actual saved executable plan. Generate/regenerate with diff; save revisions; select target/lane; edit visually and raw JSON; reorder, parallelize, add/delete steps; full command/cwd/timeout/recovery inspector; configure failure policy; show validation, recipe/source staleness, execution steps/logs, recovery coverage and outcomes; run exact saved revision; download plan and offer actionable local sync/execution instructions. Editing an active plan creates a new revision and does not mutate its run.

Provide a project/repository recipe editor using the same landing metadata, including build, deployment, forward verification, capture, restore and restore verification. Existing source-revert functionality must be clearly distinguished from restoring services. Use the real API, not an in-memory-only demo.

## Completion

End-to-end verification must include a synthetic command deployment that changes real disposable state, then fails partially, and restores and verifies the previous state; independent operations overlap; plan edits change execution; stale/conflicting plans refuse; hosted and local run the same plan; run/plan sync round-trips. Run appropriate repo suites, review all outgoing artifacts for confidentiality, publish ready PRs via Knit, and start/verify the bundle runtime. Production release uses Knit landing after checks and concrete deployment plan inspection; do not claim production completion without that evidence.
