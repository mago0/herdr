//! Explicit cooperative subtree lifecycle control, separate from conversation resume.
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SubtreeControlParams {
    /// pause, status, request, checkpoint, reconcile, resume or release.
    pub action: String,
    /// Fleet-owned members/edges that must be fenced even if a supervision link was lost.
    #[serde(default)]
    pub expected: Vec<ExpectedSubtreeMember>,
    #[serde(default)]
    pub target: Option<String>,
    #[serde(default)]
    pub pause_id: Option<String>,
    /// Recorded operator authorization for resume/release; never supplied by a timer.
    #[serde(default)]
    pub authorization: Option<String>,
    /// A durable checkpoint reference, attesting owned autonomous work and wakes stopped.
    #[serde(default)]
    pub evidence: Option<String>,
    /// Only these saved jobs may run again after reconciliation.
    #[serde(default)]
    pub restore_jobs: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ExpectedSubtreeMember {
    pub pane_id: String,
    pub supervisor_pane_id: String,
    #[serde(default)]
    /// Recorded fleet worker name, not the agent kind.
    pub agent: Option<String>,
    #[serde(default)]
    pub session: Option<super::launch::AgentSessionSpec>,
}
