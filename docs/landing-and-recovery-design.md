# Editable landing and recovery plans

Status: architecture and acceptance contract, 2026-09-24. The companion `landing-implementation-contract.md` defines the concrete v0.2 interfaces. Delivery includes the executor, hosted storage, editor, synchronization, recovery, and runtime verification together. All example repositories, commands, and environments are synthetic.

## Problem and decision

A landing plan must be the executable description of how a bundle reaches an environment: merges, builds, checks, deployments, releases, and recovery. A separate analysis that lists merges and mentions deployment configuration is insufficient. The plan an operator edits and reviews must be exactly the plan the selected runner executes.

Keep Knit responsible for the plan format, generation, validation, execution, and receipts. A hosted service stores revisions, provides editing and runner selection, and presents progress. Analysis annotates a plan revision; it must not produce a second competing execution definition.

There are three distinct objects:

1. **Project recipes:** reusable deployment/release knowledge, keyed by repository and environment.
2. **Bundle plan:** a generated, editable, versioned snapshot of selected recipes and the desired forward and recovery operations.
3. **Run:** an immutable reference to a plan revision plus actual before-state, outputs, effect receipts, and progress. A recovery run references this run and its captured state.

## Baseline before this change

The previous local implementation provides these foundations:

- `knit land` creates or shows `.knit/land-plans/<bundle>.land.json`; apply is explicit.
- Plans contain `merge_pr`, `merge_branch`, `wait_checks`, `run`, and `deploy` steps. Commands can implement compilation or verification, but these purposes are not first-class labels.
- `needs` determines dependencies. Independent steps run concurrently in dependency waves. A whole wave finishes before the next wave starts. Array position alone is not execution order.
- Project `landing.merge` and `landing.deployments`, with target/lane variants, generate defaults. Explicit merge ordering generates dependency edges; otherwise independent merges may run together. Deployment commands already have repository, cwd, environment, timeout, and dependency metadata.
- Failed runs can resume. Current rollback creates revert PRs for successful PR merge steps. It does not land those PRs, restore deployed artifacts, undo command effects, or automatically undo intermediate branch merges.
- The hosted artifact path and the local workspace executor have different behavior. The former does not consume the local editable plan. This must be resolved before a graphical editor can truthfully claim that its edits affect execution.

Implementation anchors: `src/commands/land/{types,plan,execute,rollback,display}.rs`, `src/model/project.rs`, and `schemas/land-plan.schema.json`.

## Authoring and scheduling

Add a versioned authoring format with ordered `sequence` and explicit `parallel` groups. Moving an operation in a sequence changes execution; moving it into a parallel group changes concurrency. Nested sequences inside a parallel group allow independent per-repository pipelines. Do not reinterpret old `steps[].needs` plans: preserve their existing semantics and scheduling mode.

The authoring tree is the stored source of truth. Knit deterministically compiles it into an execution graph. The compiled graph is a derived, hash-checked projection, never a second independently editable definition. Compilation preserves explicit group barriers. Existing DAG plans retain their original edges in an advanced graph mode; importing an arbitrary DAG must not silently impose new barriers or weaken dependencies.

The v0.2 executor retains the existing wave scheduler and exposes its `needs` faithfully. A new ready-step scheduler, if introduced later, needs an explicit scheduling version: the current wave barrier is observable behavior, not a rendering detail.

Authoring fragment stored in the v0.2 plan’s `workflow` field:

```json
{
  "sequence": [
    {"step": "capture-before"},
    {"parallel": [{"step": "merge-api"}, {"step": "merge-web"}]},
    {"parallel": [{"step": "build-api"}, {"step": "build-web"}]},
    {"step": "deploy-api"},
    {"step": "deploy-web"},
    {"step": "verify-environment"}
  ]
}
```

Each referenced operation has a stable id, a human label, operation kind, repo, target environment, runner requirement, working directory, exact argv, timeout, nonsecret environment bindings, output contract, resource locks, and recovery declaration. Provider operations show their actual parameters instead of fabricating shell commands.

Validate unknown references, duplicates, cycles, empty groups, output references used before production, unavailable runners, conflicting locks, and recovery coverage. An operation that builds a merged revision must consume the recorded merge result, not a moving branch name. A pre-merge build must identify the exact candidate tree and prove equivalence to the landed revision before promotion.

Parallelism means independent operations *eligible* to run together. Runner capacity and locks can serialize them; the view must display that limitation. Default locks include the target environment/service and mutable checkout. Read-only builds should use isolated pinned checkouts. Never run two different bundles' deployments into the same environment concurrently without an explicit supported strategy.

## Project and repository metadata

Extend the existing Knit project landing metadata rather than inventing a hosted-only deployment configuration. The hosted project stores and exports that same versioned document. A repository settings form is an editor for the corresponding entry in the project document.

A recipe includes:

- Repository and environment selection, trigger repositories, and dependency contracts.
- Build/release command, exact source input, and immutable output artifact identity.
- Current-deployment inspection and snapshot command/provider operation.
- Deploy/promote command or external workflow trigger, plus completion observation.
- Health/verification operation, timeout, and success conditions.
- Restore operation taking the captured prior artifact/configuration, and verification of that restoration.
- Runner capability requirements, checkout resolution, credential references, concurrency limits, and resource locks.

The resolved order of configuration is project defaults, repository recipe, environment override, and bundle-specific edit. Generation freezes the resolved result and records its source revision. Changing a project recipe does not silently change a reviewed bundle plan. Regeneration presents an operation-by-operation diff and preserves or explicitly resolves user edits.

Keep independent actions separate: a script that builds, migrates, deploys, and verifies remains opaque to Knit until split into declared operations. The UI must not invent internal parallelism or rollback coverage for an opaque script.

## Plan page

The page is an editor and run entry point:

- Header: destination, plan revision, selected runner, local/remote synchronization state, and stale-input indicator.
- Actions: Generate default, regenerate with diff, add step/group, save revision, validate, sync, and execute the saved revision.
- Forward/recovery switch: both are part of the same reviewed plan. Recovery exists before execution, with captured values resolved at run time.
- Main canvas: ordered groups with visible arrows; operations in a parallel group appear side by side under a clear “Parallel” label. Nested chains remain visible. Unknown durations are not drawn as estimated timings.
- Step inspector: full command/provider operation, repo, cwd, timeout, inputs/outputs, dependencies, runner, locks, and reverse action. Essential commands wrap; they are not truncated to an ellipsis.
- Editing: drag-and-drop, keyboard move before/after, move into/out of a parallel group, duplicate, delete, and raw JSON editing of the same document. A move shows the execution dependency changes before saving; invalid moves explain the violated dependency.
- Failure policy: stop and offer recovery, or automatically execute the reviewed recovery plan. Display recovery coverage and unresolved manual operations beside this choice.
- Run mode: the selected immutable plan, individual step progress/logs, actual artifact identities, and recovered/remaining effects. Editing creates another revision; it never changes an active run.

Keep readiness analysis and live preflight visibly separate from execution. A passed recorded check is not a current live approval, and a branch push is not evidence that a deployment finished.

## Recovery semantics

Every plan must have a recovery section. Every state-changing operation must declare one of: executable compensation, retained effect with an explicit explanation, or unsupported/manual recovery. Checks and disposable build artifacts can declare no deployed effect. The generator cannot infer an inverse of an arbitrary command.

Offer automatic restoration for plans whose targeted deployment effects have complete executable recovery coverage. Always offer the recovery view after failure, even when some required operations are manual. A plan with unsupported operations must say “partial recovery” and cannot promise restoration of the whole environment. Do not interpret a timeout or nonzero exit as proof that no side effect happened.

Before any mutation, durably capture the relevant prior state: deployed image/release digest, routing configuration, runtime config revision, schema compatibility/backup identifiers where applicable, and destination branch SHA. Persist the operation intent before invoking it and a provider receipt immediately after observing it. Snapshots contain references, not plaintext credentials.

Acquire environment ownership before capture. Every receipt distinguishes an effect performed by this run, an already-satisfied pre-existing effect, and an uncertain effect. Compensate only effects attributable to this run unless the reviewed plan explicitly authorizes a wider restoration scope. A PR already merged before execution is not automatically this run's work to undo.

Define a command adapter protocol: a stable logical effect id across retries, a separate attempt id, structured receipt output, and a probe returning absent/applied/partial/unknown. Provider adapters must support deduplication or authoritative probing. Arbitrary commands without that contract cannot promise automatic crash reconciliation; uncertain results require intervention. Forward and recovery commands receive ownership tokens, but token delivery alone is insufficient: the destination must reject stale tokens, or the runner must guarantee termination and quiescence before ownership can transfer.

On failure:

1. Stop scheduling new forward work; cancel running work where supported.
2. Reconcile in-flight operations and wait for quiescence. Recovery must not race an unfinished deployment or external workflow.
3. Instantiate the recovery plan from actual effect receipts, including partially applied or uncertain failed operations. Use a provider probe to resolve uncertainty; require intervention if it cannot be resolved.
4. Compensate in reverse dependency order. A failed, manual, or uncertain dependent inverse blocks prerequisite compensation while independent recovery branches may continue. Use an alternative order only if the reviewed recovery graph supplies a stronger service-specific order. Run independent restoration branches concurrently only when their locks and compatibility allow it.
5. Verify the captured deployment identities and health. Record partial failure honestly and permit recovery retry from its own receipts.

Distinguish deployment restoration from source restoration. Redeploying a retained old artifact may restore service quickly while source reverts still await review. Opening a revert PR is only “source revert proposed.” Landing it is a separate state-changing step. Source reverts may trigger CI/CD, so recovery must coordinate or fence those triggers to avoid overwriting the restored deployment.

A branch revert must preserve unrelated commits and use expected-head checks; never reset the shared branch to an old SHA. Database migrations require an explicit tested reverse/restore procedure or a compatibility policy. Published packages and irreversible external effects require honest manual recovery declarations. The UI must never display “restored” just because revert PRs exist.

Use separate outcome axes: forward run failed/succeeded; service unchanged/restored/partially restored/unknown; source unchanged/revert proposed/reverted/partial. Recovery failure is not landing success. Preserve original logs and both forward and recovery receipts.

Starting recovery permanently blocks forward resume of that run. A later forward attempt creates a new run with current-state inspection and fresh captures. Recovery retries retain their original prior-state target and resume only unfinished compensation operations. Reject recovery of a superseded run before mutation: restoring A after another run B changed the environment would erase B’s deployment. Record an execution generation or compare the current deployed identity through the adapter. Source proposal uncertainty remains uncertain across retries until reconciled; do not create duplicate revert PRs. Journal finalization separately: ledger persistence, archival, cleanup, and synchronization are idempotent operations that remain resumable after forward steps finish.

## Local/hosted identity and synchronization

Store bundle plans and runs as first-class versioned artifacts, indexed by project, bundle, destination, plan id, revision, and content hash. Include source bundle snapshot/hash, repo heads, recipe revision, required executor version, and recovery definition. Multiple destinations must not overwrite a single plan slot.

Add plans/runs to Knit's native sync protocol and project export/localization. The commands `knit sync push --plans` and `knit sync pull --plans` extend that existing family. Local materialization preserves revision and hash. A downloaded plan resolves repo ids and secret references through local runner bindings, rather than carrying someone else's absolute paths or credentials.

Use compare-and-swap revisions for edits; divergent local and hosted edits require a diff/merge, not last-write-wins. Runs pin the exact plan hash, target, bundle heads, and recovery revision. Revalidate runtime state before execution, including changes since the plan was generated.

One runner owns execution through a non-expiring ownership record. Destination aliases must resolve to the same exclusion domain; conservative project-wide ownership is valid until disjoint resource identities are explicitly supported. A process crash does not prove its child commands or external workflows stopped. Moving execution from hosted to local requires an explicit handoff after quiescence and receipt reconciliation. Copying a JSON file must not start a second deployment. Offline execution can be supported for a separately owned environment; a disconnected local process cannot safely share an unfenced production destination.

The hosted runner must call Knit's plan executor with the stored plan rather than merge the artifact and run its own deployment loop. Runners without required capabilities reject before the first mutation. They must never silently skip command or recovery steps.

## Compatibility and delivery

Before exposing more powerful editing, close existing correctness gaps: apply must recheck terminal coverage after manual plan edits; resume must bind to complete plan content rather than only the set of step ids; hosted failure handling must persist partial execution receipts even when the merge process or a later deployment fails; and interrupted finalization must remain resumable after steps succeed. These are prerequisites for trustworthy editing and recovery. Trigger repositories (`whenChanged`) and prerequisite operations (`needs`) are separate concepts; generation must make that distinction visible and enforce declared build inputs.

The following are implementation dependencies, not separate partial releases. All are required for delivery.

1. **Canonical plan and honest rendering.** Expose generation/validation as a machine-readable Knit contract. Persist the actual plan, render its dependencies and commands, and show the legacy rollback limitation. Retain existing plan semantics. This is required together with editing and execution; merely drawing arrows over the analysis is insufficient.
2. **Editing, runner parity, and sync.** Add revisioned plan APIs, visual/raw editing, project recipe forms, and local/hosted round trips. Establish environment ownership, checkout exclusion, and fenced handoff before enabling multiple runners. Make both entry points execute the exact saved plan. Introduce ordered group authoring as a versioned extension.
3. **Deployment recovery.** Add captures, effect provenance, structured command receipts/probes, compensation execution, quiescence, and provider-specific restore/verify recipes. Ship the first supported provider end to end before claiming generic rollback. Recovery controls stay truthful for other providers.

Continue to load old plans. Legacy `onFailure: rollback` retains “create revert PRs” behavior and is labeled accordingly; it must not silently gain permission to deploy or merge reverts. New automatic compensation is an explicit policy in the new format. Old executors must reject unsupported plan versions/required capabilities before any side effects; permissive deserialization must not silently discard recovery fields.

## Acceptance scenarios

- Generate once; the UI and local CLI show identical operation ids, commands, dependencies, targets, and recovery definitions.
- Move a step in ordered authoring; the compiled dependency graph and actual execution change together. Reject a build moved before its required source exists.
- Independent builds overlap; a downstream deploy waits for its prerequisites. Existing wave plans keep their barriers.
- A hosted runner missing a tool, checkout, or restore capability refuses before merging anything.
- A project recipe update marks affected plans stale and offers a diff without replacing saved bundle edits.
- Save online, pull locally, edit, push, and execute the same hashed revision. Conflicting edits do not silently overwrite each other.
- One deployment succeeds and another fails after applying changes: capture both effects, restore the correct prior artifacts, and verify service state.
- A process crashes after a provider accepts deployment but before success is recorded: reconcile by operation id before retry or recovery.
- A merge already completed before this run is not compensated as this run's effect.
- A crash between successful steps, ledger persistence, archival, and synchronization can resume finalization without rerunning deployment.
- Recovery waits for parallel forward effects to quiesce. A second machine cannot take over while the first still has valid ownership.
- A restore command fails: preserve completed recovery receipts, block its prerequisite inverses, and retry only unfinished recovery work.
- Two destination aliases resolve to the same environment: only one runner can claim execution.
- A newer run changes the deployment: historical recovery refuses before overwriting it.
- Cancellation before spawn does not start a forward mutation; recovery uses explicit cancellation semantics and completes verification.
- Missing, failed, or stale named required checks block both local and hosted execution before mutation.
- Partial or complete recovery prevents forward resume of the same run; a new forward run captures fresh state.
- A revert PR is created but remains open: the UI says so and does not mark the environment restored.
- An irreversible migration or release is explicitly reported as incomplete recovery coverage, rather than receiving a fabricated reverse command.

Use shared synthetic fixtures across the CLI, hosted service, and UI for execution parity. Test recovery through a disposable fake deployment provider with injected failures and inspect actual state, not merely expected command strings.
