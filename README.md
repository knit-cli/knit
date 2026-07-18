# Knit

Knit is a CLI for authoring and coordinating multi-repo feature work. Think of it as "git for cross-repo feature work": a **bundle** is the cross-repo analogue of a git branch, holding a small set of related repositories. Knit creates a coordinated checkout per repo, commits staged changes across them all at once, and records the result in a language-neutral JSON artifact. From that bundle it can open one pull request per repo and land them as a single set, or merge the feature branches into local targets when there is no code host. Parallel features stay isolated: the same source repo can appear in many bundles at once, each on its own branch and worktree.

Knit shells out to `git`. It does not use libgit2 and it does not try to replace git — everyday verbs (`status`, `diff`, `add`, `commit`, `push`, `log`) just run across every repo in the bundle at once.

<!-- image: docs/assets/hero.gif — short screencast: knit bundle → edit → commit → land -->

## Install

Brew:
```sh
brew tap marc-merino/knit && brew install knit   # macOS / Linux binaries
```
Cargo:
```sh
cargo install knit-cli --version 0.1.0-alpha.4   # from crates.io (the binary is named `knit`;
                                                 # cargo needs the explicit version while only pre-releases exist)
# or from a checkout:
cargo install --path .
```

## The loop

```sh
knit init demo                          # a project remembers which repos belong together
knit project add backend ../backend
knit project add frontend ../frontend

knit bundle "my feature" --cd           # branch + isolated worktree per repo; your shell
                                        # (or coding agent) lands inside the checkout
# ...edit code in the per-repo worktrees...
knit status                             # one view across every repo
knit commit --all -m "Add my feature"   # one logical commit, recorded across repos

knit check run ci                       # record a test verdict pinned to these exact commits
knit publish create                     # one PR per repo, cross-linked
knit land && knit land apply            # merge, deploy, archive, remove worktrees

knit tag v1 --bundle my-feature         # after verifying main: pin the post-land mains as one
                                        # named set, exported as git tags knit/v1 in every repo
```

The [quickstart](docs/quickstart.md) walks this loop end to end with two toy repos in about ten minutes.

## Why

- **Bundles are branches that span repos.** One feature unit across N repositories: shared status, combined diff, one logical commit, one PR per repo landed as a set.
- **Parallel by construction.** The same repo can sit in many bundles at once, each on its own branch and generated worktree. Run one coding agent per bundle — they cannot collide, and each bundle worktree root carries its own `AGENTS.md` so agents wake up oriented.
- **An append-only ledger, not just branches.** Every bundle is a JSON artifact recording commits, observed changes, check verdicts, landings, and reverts. Other tools read it; nothing is locked in a database.
- **Verdicts you can trust.** `knit check` pins pass/fail to the exact per-repo commits it ran against — a verdict goes stale the moment the bundle moves, and projects can require green-and-fresh checks before landing. With five agent bundles in flight, "which one is ready?" has a ledger answer, not a chat claim.
- **Known-good markers across repos.** After landing and verifying main, `knit tag` pins the post-merge mains as one immutable named set — the whole-system snapshot a monorepo gets from a single SHA — recorded on the ledger and exported as plain git tags any host, CI, or clone can read.
- **Local-first, host optional.** Everything works against plain git repos. A KnitHub deployment adds hosted dashboards, history sync, `knit clone` to rebuild a workspace anywhere, and Urdir/Gloss cross-repo reviews on top of the same artifact.
- **Review-ready artifact.** The same bundle artifact gives `urdir` enough cross-repo context to prepare review analysis that `gloss` can display and explain.

## Concepts in one breath

- **Project** — reusable template of repos that belong together (`knit init`, `knit project add`).
- **Bundle** — the branch-like feature unit; `.bundle.json` is its source of truth.
- **Worktrees** — generated per-repo checkouts under `.knit/worktrees/<bundle>/`; all edits happen there.
- **Ledger** — append-only node chain inside the bundle; undo is `knit revert`, outside commits fold in via `knit sync`.
- **Publish/land vs merge** — PRs landed as a set when you have a code host; direct local branch integration when you don't.
- **Checks** — named verdicts recorded on the ledger, pinned to exact heads; optionally gate landing.
- **Tags** — immutable cross-repo known-good markers pinning post-land origin bases, exported as `knit/<name>` git tags.

The full versions live in the [quickstart](docs/quickstart.md#concepts) and the [reference](docs/reference.md).

## Docs

- **[docs/quickstart.md](docs/quickstart.md)** — install to landed multi-repo change in ten minutes, plus the concepts in full.
- **[docs/runtime-setup.md](docs/runtime-setup.md)** — configure any dockerized project for `knit run up`: zero-config to shared dev database, step by step.
- **[docs/reference.md](docs/reference.md)** — the complete behavior reference: every command, projects and views, bundle lifecycle, publish/land/merge, checks, history and `related`, revert, storage layout, limitations, roadmap.
- **[docs/architecture.md](docs/architecture.md)** — module boundaries and test layout.
- **[docs/change-group-schema.md](docs/change-group-schema.md)** — the bundle (`ChangeGroup`) schema.
- **[dist/README.md](dist/README.md)** — how releases are cut (binaries, crates.io, Homebrew/Scoop/winget).

## Knit, Urdir, and Gloss

Knit, Urdir, and Gloss share a simple handoff. Knit owns authoring and workspace mechanics — repos, worktrees, feature branches, commit groups, ledger updates. Urdir reads a bundle later and produces cross-repo review analysis. Gloss displays and explains that review artifact; on KnitHub the three meet in one page.
