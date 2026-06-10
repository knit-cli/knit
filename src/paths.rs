use std::path::Path;

/// Compare two filesystem paths for equality at the textual/component level.
///
/// On Unix this is an exact, case-sensitive component comparison (unchanged
/// historical behavior). On Windows, where the filesystem is case-insensitive
/// and `/` and `\` are interchangeable separators, paths are compared
/// case-insensitively so that e.g. `C:\repo` and `c:\repo` are treated as the
/// same repository when deduplicating tracked repos.
///
/// This does not touch the filesystem (no canonicalization), so it stays cheap
/// and side-effect free; it is a best-effort textual comparison used for
/// dedup/identity checks, not for security decisions.
pub fn same_path(left: &str, right: &str) -> bool {
    let left = Path::new(left);
    let right = Path::new(right);

    #[cfg(windows)]
    {
        // Windows paths are case-insensitive. Compare components case-folded so
        // drive-letter and directory casing differences do not produce spurious
        // duplicate repo entries.
        let normalize = |path: &Path| -> Vec<String> {
            path.components()
                .map(|component| {
                    component
                        .as_os_str()
                        .to_string_lossy()
                        .to_lowercase()
                })
                .collect()
        };
        return normalize(left) == normalize(right);
    }

    #[cfg(not(windows))]
    {
        left == right
    }
}

/// `fs::canonicalize` without Windows `\\?\` verbatim prefixes, which break
/// `strip_prefix` comparisons against non-canonicalized paths.
pub fn canonicalize(path: impl AsRef<Path>) -> std::io::Result<std::path::PathBuf> {
    dunce::canonicalize(path)
}

/// `Path::strip_prefix` that matches the platform's path identity rules: exact
/// on Unix, component-wise case-insensitive on Windows (where `C:\Repo` and
/// `c:\repo` are the same directory).
pub fn strip_path_prefix(path: &Path, prefix: &Path) -> Option<std::path::PathBuf> {
    #[cfg(not(windows))]
    {
        path.strip_prefix(prefix).ok().map(|p| p.to_path_buf())
    }
    #[cfg(windows)]
    {
        let mut path_components = path.components();
        for prefix_component in prefix.components() {
            let path_component = path_components.next()?;
            let same = match (prefix_component, path_component) {
                (
                    std::path::Component::Normal(left),
                    std::path::Component::Normal(right),
                ) => left.to_string_lossy().to_lowercase()
                    == right.to_string_lossy().to_lowercase(),
                (left, right) => {
                    left.as_os_str().to_string_lossy().to_lowercase()
                        == right.as_os_str().to_string_lossy().to_lowercase()
                }
            };
            if !same {
                return None;
            }
        }
        Some(path_components.as_path().to_path_buf())
    }
}
