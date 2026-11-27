# Knit 0.1.0-alpha.47

Landing ownership locks and each project's latest-run generation now live in
`landing-ownership/` next to the user-level config (`$KNIT_HOME`,
`$XDG_CONFIG_HOME/knit`, or `~/.config/knit`) instead of the OS temp
directory. Temp cleanup can no longer drop a lock that must survive a crash or
the record that a previous landing left unresolved effects, and each user on a
shared Linux machine gets their own records instead of failing with "landing
resource is owned" in a `/tmp` folder another user created. Records left in the
temp directory by earlier releases still block until reconciled, so upgrading
in the middle of a landing is safe.

Pinned checkouts for landing commands use a shorter directory name, keeping
deep workspaces under Windows' 260-character path limit.
