//! Knit config: user-global (`~/.config/knit/config.json`) and workspace (`.knit/config.json`).

use super::SCHEMA_VERSION;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KnitConfig {
    pub schema_version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_bundle: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_project: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sync_remote: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sync_remotes: Vec<String>,
    #[serde(default = "default_advice")]
    pub advice: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stealth: Option<bool>,
    /// When true (default), git-pushing commands also push the bundle artifact to
    /// the configured KnitHub remote. Set false to never sync on push.
    #[serde(default = "default_push_sync")]
    pub push_sync: bool,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub remotes: BTreeMap<String, KnitRemote>,
}

impl KnitConfig {
    pub fn empty() -> Self {
        Self {
            schema_version: SCHEMA_VERSION.to_string(),
            active_bundle: None,
            active_project: None,
            sync_remote: None,
            sync_remotes: Vec::new(),
            advice: true,
            stealth: None,
            push_sync: true,
            remotes: BTreeMap::new(),
        }
    }

    pub fn new(active_bundle: String) -> Self {
        Self {
            schema_version: SCHEMA_VERSION.to_string(),
            active_bundle: Some(active_bundle),
            active_project: None,
            sync_remote: None,
            sync_remotes: Vec::new(),
            advice: true,
            stealth: None,
            push_sync: true,
            remotes: BTreeMap::new(),
        }
    }

    pub fn new_project(active_project: String) -> Self {
        Self {
            schema_version: SCHEMA_VERSION.to_string(),
            active_bundle: None,
            active_project: Some(active_project),
            sync_remote: None,
            sync_remotes: Vec::new(),
            advice: true,
            stealth: None,
            push_sync: true,
            remotes: BTreeMap::new(),
        }
    }

    pub fn stealth_enabled(&self) -> bool {
        self.stealth.unwrap_or(false)
    }
}

fn default_advice() -> bool {
    true
}

fn default_push_sync() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KnitRemote {
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
}
