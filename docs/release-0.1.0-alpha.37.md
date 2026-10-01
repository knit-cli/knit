# Knit 0.1.0-alpha.37

Publishing can take review titles, descriptions, and draft choices from project
and bundle configuration. Dependency-based drafts recognize declared
library/consumer relationships and Cargo dependencies on bundle feature branches,
including fork URLs. Per-repository command-line overrides and a read-only preview
make the final review settings inspectable before publication.

Knit uses configured Git authors for commits it creates and warns when an
injected author environment is ignored. Publishing checks the complete review history, and explicit pushes check
outgoing commits, rejecting unexpected authors, and enforce signing when Git signing
is enabled. An explicit author override allows deliberate collaboration without
bypassing the signing requirement.

Workspace diagnostics allow archived bundles to retain historical source paths
after repositories move or are removed. Missing paths in active bundles and
invalid historical records still fail validation.
