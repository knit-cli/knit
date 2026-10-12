use crate::model::ChangeGroup;
use chrono::Utc;
use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

/// Expand CLI repo selector flags. Each value may be one id or a comma-separated
/// list, for example `--exclude frontend,docs`.
pub fn expand_repo_selectors(values: &[String]) -> Vec<String> {
    let mut expanded = Vec::new();
    for value in values {
        for part in value.split(',') {
            let part = part.trim();
            if part.is_empty() {
                continue;
            }
            expanded.push(part.to_string());
        }
    }
    expanded
}

pub fn slugify(input: &str) -> String {
    let mut slug = String::new();
    let mut last_was_dash = false;

    for ch in input.chars().flat_map(char::to_lowercase) {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch);
            last_was_dash = false;
        } else if !last_was_dash && !slug.is_empty() {
            slug.push('-');
            last_was_dash = true;
        }
    }

    while slug.ends_with('-') {
        slug.pop();
    }

    if slug.is_empty() {
        "bundle".to_string()
    } else {
        slug
    }
}

pub fn unique_repo_id(bundle: &ChangeGroup, desired_id: &str) -> String {
    if !bundle.repos.iter().any(|repo| repo.id == desired_id) {
        return desired_id.to_string();
    }

    for index in 2.. {
        let candidate = format!("{desired_id}-{index}");
        if !bundle.repos.iter().any(|repo| repo.id == candidate) {
            return candidate;
        }
    }

    unreachable!("unbounded iterator should always find a repo id")
}

pub fn commit_group_id() -> String {
    object_id("kg")
}

pub fn revert_group_id() -> String {
    object_id("rv")
}

pub fn revert_plan_id() -> String {
    object_id("rp")
}

pub fn node_id(prefix: &str) -> String {
    object_id(prefix)
}

/// Ids from one process start at a random suffix and count up from it, so two
/// ids requested in the same clock tick (parallel revert clones, for example)
/// never repeat.
fn object_id(prefix: &str) -> String {
    static START: OnceLock<u64> = OnceLock::new();
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let start = *START.get_or_init(|| {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or_default();
        let mut hasher = RandomState::new().build_hasher();
        hasher.write_u128(nanos);
        hasher.write_u32(std::process::id());
        hasher.finish()
    });
    let date = Utc::now().format("%Y%m%d");
    let suffix = start.wrapping_add(NEXT.fetch_add(1, Ordering::Relaxed)) & 0xFF_FFFF;
    format!("{prefix}_{date}_{suffix:06x}")
}

pub fn short_sha(sha: &str) -> String {
    sha.trim().chars().take(7).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn ids_requested_at_once_from_many_threads_never_repeat() {
        let ids: Vec<String> = std::thread::scope(|scope| {
            let workers: Vec<_> = (0..8)
                .map(|_| scope.spawn(|| (0..2_000).map(|_| revert_group_id()).collect::<Vec<_>>()))
                .collect();
            workers
                .into_iter()
                .flat_map(|worker| worker.join().unwrap())
                .collect()
        });
        let unique: HashSet<_> = ids.iter().collect();
        assert_eq!(unique.len(), ids.len());
        assert!(ids.iter().all(|id| {
            let parts: Vec<_> = id.split('_').collect();
            parts.len() == 3
                && parts[0] == "rv"
                && parts[1].len() == 8
                && parts[2].len() == 6
                && parts[2].chars().all(|c| c.is_ascii_hexdigit())
        }));
    }
}
