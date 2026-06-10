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
