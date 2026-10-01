//! Read templates from the recorded base commit in a local checkout.
use crate::{git::git_output_optional, ids::short_sha, model::RepoEntry};
use anyhow::Result;
use std::path::Path;

pub(super) fn upstream_template(
    repo: &RepoEntry,
    checkout: Option<&Path>,
) -> Result<Option<(String, String)>> {
    let (Some(checkout), Some(base)) = (checkout, repo.base_sha.as_deref()) else {
        return Ok(None);
    };
    for path in [
        ".github/pull_request_template.md",
        ".github/PULL_REQUEST_TEMPLATE.md",
        "docs/pull_request_template.md",
        "docs/PULL_REQUEST_TEMPLATE.md",
        "pull_request_template.md",
    ] {
        if let Some(text) = git_output_optional(checkout, ["show", &format!("{base}:{path}")])? {
            return Ok(Some((format!("upstream:{path}@{}", short_sha(base)), text)));
        }
    }
    Ok(None)
}
