use crate::model::ChangeGroup;
use chrono::Utc;
use std::time::{SystemTime, UNIX_EPOCH};

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
    let date = Utc::now().format("%Y%m%d");
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let mixed = nanos ^ u128::from(std::process::id());
    let suffix = format!("{:06x}", mixed & 0xFF_FFFF);
    format!("kg_{date}_{suffix}")
}

pub fn short_sha(sha: &str) -> String {
    sha.trim().chars().take(7).collect()
}
