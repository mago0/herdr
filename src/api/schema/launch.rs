//! Agent resume, and the shell state that agent start and resume wait for.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PaneShellStateParams {
    pub pane_id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PaneShellState {
    /// The pane's shell is the only foreground process and its line editor reads input.
    Ready,
    /// The shell is still starting: it runs its startup files, or its line editor does not
    /// read input yet.
    Starting,
    /// Another program runs in the foreground, or an agent owns the pane.
    Busy,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PaneShellStateInfo {
    pub pane_id: String,
    pub state: PaneShellState,
    /// Changes each time the pane prints output. Equal values in two reads mean no output came
    /// between them.
    pub output_seq: u64,
}

/// A recorded agent session, in the shape `agent.get` reports as `agent_session`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct AgentSessionSpec {
    pub agent: String,
    pub kind: crate::agent_resume::AgentSessionRefKind,
    pub value: String,
    /// Defaults to Herdr's own integration source for the agent, such as `herdr:claude`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct AgentResumeParams {
    pub name: String,
    /// The session to resume. Give this or `from`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<AgentSessionSpec>,
    /// A pane or agent whose recorded session to resume.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    /// A pane at an interactive shell prompt. Give this or `cwd`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pane_id: Option<String>,
    /// Opens a new tab with this working directory for the agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// The workspace of the tab that `cwd` opens. Defaults to the focused workspace.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    /// Native agent arguments added to the resume command, such as a model or permission mode.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    /// Startup timeout in milliseconds. Values must be greater than 3000 and at most 300000.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    /// Text typed into the agent once it is ready for input, through the delivery queue.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
}
