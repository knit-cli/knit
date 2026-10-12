# Knit 0.1.0-alpha.48

Ids that Knit creates in one process (commit groups, revert groups, revert
plans and ledger nodes) no longer repeat when several are requested in the
same clock tick. `knit revert landed --apply` across several repositories no
longer fails with "destination path already exists" when two parallel reverts
were given the same clone directory.
