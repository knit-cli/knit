# Knit 0.1.0-alpha.45

Retrying a landing that failed no longer strands its landing lock. A failed
run keeps the lock so its effects can be finished or recovered, and
`knit land apply` of the same plan now continues that run, skipping the
steps it already finished, instead of starting a second run the execution
authority would refuse to record. When the run's journal is not in the
workspace, Knit stops before doing anything and names `knit land resume --run`.
