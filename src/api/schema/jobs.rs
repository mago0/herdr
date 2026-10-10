use std::collections::HashMap;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct JobAddParams {
    /// Unique job name. Adding a job with an existing name replaces it.
    pub name: String,
    pub command: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub env: HashMap<String, String>,
    /// Run on this interval. At least 1000.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub every_ms: Option<u64>,
    /// Run when one of these events occurs, by dotted name.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub on_events: Vec<String>,
    /// Run when this file changes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_path: Option<String>,
    /// Agent name or pane id that receives the output of a run that exits 0.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    /// The job is removed when this pane closes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_pane_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    /// Unix time in milliseconds after which the job is removed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub until_ms: Option<u64>,
    /// Output equal to the last delivered output of this job is not delivered again. Default true.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dedupe: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct JobTarget {
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct JobInfo {
    pub name: String,
    pub command: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub every_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub on_events: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_pane_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_pane_id: Option<String>,
    pub timeout_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub until_ms: Option<u64>,
    pub dedupe: bool,
    pub running: bool,
    pub runs: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_exit: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_run_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_news: Option<String>,
}
