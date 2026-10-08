# Knit 0.1.0-alpha.41

Reviews show their landing gates: what still stands between each review and
its merge, and who has to act. `knit land check` and `knit publish status
--live` list the open gates under each review and call a review `ready` only
when none is open. On GitHub they come from the base branch's rulesets and
public branch protection, the review decision, required checks (one that never
reported is pending, not passed), workflow runs waiting for a maintainer to
approve them, commit signatures, and whether this account can merge; other
hosts report what their review objects expose. `knit publish sync` records the
gates in each publication, so `knit land` shows them under each merge step and
hosted dashboards show them without asking the host.
