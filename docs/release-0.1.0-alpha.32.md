# Knit 0.1.0-alpha.32

## Library releases and consumer updates

A project can describe a library merge, manual release confirmation, and
consumer dependency updates as one landing plan. The run pauses while a person
releases the library or pushes a consumer update, then resumes its original
immutable plan.

```json
{
  "landing": {
    "merge": { "waitForChecks": true, "requiredChecksOnly": false },
    "dependencies": [{
      "library": "core-lib",
      "consumers": "*",
      "release": { "instructions": "Publish the library release." },
      "bump": {
        "instructions": "Commit and push the dependency update.",
        "paths": ["Cargo.toml", "Cargo.lock", "**/Cargo.toml"]
      }
    }]
  }
}
```

The dependency steps are generated only for bundles that change the library.
Wildcard consumers include repositories tracked directly by the bundle. To
name a repository in project configuration, first register it with
`knit project add <id> <path> --observe` if it should remain outside the default
bundle scope.

```sh
knit land plan --force
knit land apply
knit land resume --acknowledge release-core-lib --note "Release published"
# Commit and push the consumer update, then:
knit land resume
```

Accepted consumer updates must extend the reviewed head and obey the configured
path restrictions. Later verification and merge use the accepted revision.
Review checks run again before merging. Acknowledgements and accepted revisions
are saved in the run receipt; the plan hash is unchanged.

Custom `landing.steps[]` also accept `whenChanged`. A manual step can use
`acknowledge: "resume"`; an `await_update` step waits for a review update.
Gates require executor protocol `0.6` and capability `landing-gates`.
Execution and continuation are local; hosted views display the saved plan,
pause instructions, acknowledgements and accepted revisions. Gated review
merges require an atomic head condition, currently supported through the
GitHub and GitLab API merge/squash routes.

## Bundle history rewrites

- `knit squash [-m <message>]` consolidates feature work into one commit group.
- `knit rebase [--squash] [--offline]` moves feature work onto refreshed bases.
- `knit rebase --continue` and `knit rebase --abort` resolve an interrupted
  multi-repository rewrite.
- `knit sync` reconciles supported manual rewrites, retiring superseded group
  members while preserving audit history.

Rewrites update recorded heads and bases. Preflight checks run before checkout
mutation, and recovery state records progress across repositories. A pending
rewrite blocks recording or publishing partially rewritten work.

## Fork pushes

`knit push --force-with-lease` resolves the actual push destination and sends an
explicit, fixed lease. Split fetch/push remotes and narrow fetch refspecs are
supported. A remote tip must have a matching destination receipt or be known
to the bundle ledger or feature-branch reflog; unknown updates are refused.
Transport aliases do not authorize borrowing observations from another
destination. Retries retain the original lease.

See [the reference](reference.md) for configuration details and
[the design](landing-gates-and-bundle-rewrites.md) for the alternatives and
ownership rules.
