//! The shell state that agent start waits for.

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
