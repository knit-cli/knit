//! Org-level repo universe (`KnitOrg`).

use super::SCHEMA_VERSION;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const ORG_KIND: &str = "KnitOrg";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KnitOrg {
    pub schema_version: String,
    pub kind: String,
    pub id: String,
    pub name: String,
    pub created_at: String,
    pub updated_at: String,
    #[serde(default)]
    pub repos: Vec<OrgRepoEntry>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub metadata: BTreeMap<String, serde_json::Value>,
}

impl KnitOrg {
    pub fn new(id: String, name: String, now: String) -> Self {
        Self {
            schema_version: SCHEMA_VERSION.to_string(),
            kind: ORG_KIND.to_string(),
            id,
            name,
            created_at: now.clone(),
            updated_at: now,
            repos: Vec::new(),
            metadata: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OrgRepoEntry {
    pub id: String,
    pub path: String,
    pub remote: Option<String>,
    pub base_branch: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub metadata: BTreeMap<String, serde_json::Value>,
}
