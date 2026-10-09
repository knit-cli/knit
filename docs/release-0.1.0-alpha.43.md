# Knit 0.1.0-alpha.43

Reviews merged on the host count as done. Landing plans leave them out, and a
bundle whose reviews have all merged closes itself once its landing plan has
nothing left to run: the saved plan, or without one, the plan the project's
landing recipes would generate. `knit publish sync`, `knit land`, and
`knit publish sync --from-artifact` (with `--plan` or `--project-file`) apply
the rule, so a bundle another maintainer merged no longer waits for a landing
nobody can run. `knit land remove <step>` drops a step from the saved plan, and
pulling an archived bundle removes its clean worktrees.
