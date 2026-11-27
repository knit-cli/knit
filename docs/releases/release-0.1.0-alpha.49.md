# Knit 0.1.0-alpha.49

`knit clone` imports archived, closed and landed bundles even when a repo they
touched was renamed or removed: bundles recorded under a repo's old id resolve
to the renamed repo by its forge URL, and finished history no longer needs a
checkout. Only open bundles with a missing repo are reported as dropped. A repo
the sync remote marks missing on its forge is left out with one line instead of
a failed clone, and an anonymous clone of a private project now says to save a
remote token instead of printing a bare HTTP 404.
