//! The file that keeps deliveries, supervision links and jobs across a server restart.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::api::schema::SuperviseReport;

pub(super) const STORE_FILE: &str = "orchestration.json";
const STORE_VERSION: u32 = 1;

/// Panes are named by their public id, which stays the same across a restore.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct StoredState {
    #[serde(default)]
    pub(super) version: u32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(super) deliveries: Vec<StoredDelivery>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(super) links: Vec<StoredLink>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(super) jobs: Vec<StoredJob>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(super) tokens: Vec<StoredTokens>,
}

/// The tokens of one pane that have no time limit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct StoredTokens {
    pub(super) pane_id: String,
    pub(super) tokens: std::collections::BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct StoredDelivery {
    pub(super) pane_id: String,
    pub(super) text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) source: Option<String>,
    pub(super) queued_at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) expires_at_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct StoredLink {
    pub(super) pane_id: String,
    pub(super) supervisor_pane_id: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(super) report: Vec<SuperviseReport>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct StoredJob {
    pub(super) name: String,
    pub(super) command: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) cwd: Option<String>,
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub(super) env: std::collections::BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) every_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(super) on_events: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) on_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) target_pane_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) owner_pane_id: Option<String>,
    pub(super) timeout_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) until_ms: Option<u64>,
    pub(super) dedupe: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) last_news: Option<String>,
}

impl StoredState {
    pub(super) fn is_empty(&self) -> bool {
        self.deliveries.is_empty()
            && self.links.is_empty()
            && self.jobs.is_empty()
            && self.tokens.is_empty()
    }
}

/// A missing file, a file this build cannot read, and a file from a newer build all load as empty.
pub(super) fn load(path: &Path) -> StoredState {
    let Ok(text) = std::fs::read_to_string(path) else {
        return StoredState::default();
    };
    match serde_json::from_str::<StoredState>(&text) {
        Ok(state) if state.version <= STORE_VERSION => state,
        Ok(state) => {
            tracing::warn!(
                version = state.version,
                path = %path.display(),
                "orchestration state is from a newer build; ignored"
            );
            StoredState::default()
        }
        Err(err) => {
            tracing::warn!(error = %err, path = %path.display(), "orchestration state is unreadable; ignored");
            StoredState::default()
        }
    }
}

pub(super) fn save(path: &Path, state: &StoredState) -> std::io::Result<()> {
    if state.is_empty() {
        return match std::fs::remove_file(path) {
            Err(err) if err.kind() != std::io::ErrorKind::NotFound => Err(err),
            _ => Ok(()),
        };
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut state = state.clone();
    state.version = STORE_VERSION;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(&state)?)?;
    if let Err(err) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(err);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestDir(std::path::PathBuf);

    impl TestDir {
        fn new(name: &str) -> Self {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_nanos())
                .unwrap_or(0);
            let path = std::env::temp_dir().join(format!(
                "herdr-orchestration-{name}-{}-{nanos}",
                std::process::id()
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn state() -> StoredState {
        StoredState {
            version: 0,
            deliveries: vec![StoredDelivery {
                pane_id: "w1:p2".into(),
                text: "MAIL|1".into(),
                source: Some("dispatch".into()),
                queued_at_ms: 10,
                expires_at_ms: None,
            }],
            links: vec![StoredLink {
                pane_id: "w2:p1".into(),
                supervisor_pane_id: "w1:p2".into(),
                report: vec![SuperviseReport::Blocked, SuperviseReport::Exited],
            }],
            jobs: Vec::new(),
            tokens: vec![StoredTokens {
                pane_id: "w2:p1".into(),
                tokens: [("label".to_string(), "SRE-194".to_string())].into(),
            }],
        }
    }

    #[test]
    fn saved_state_loads_back_the_same() {
        let dir = TestDir::new("round-trip");
        let path = dir.path().join(STORE_FILE);
        save(&path, &state()).unwrap();
        let mut expected = state();
        expected.version = STORE_VERSION;
        assert_eq!(load(&path), expected);
    }

    #[test]
    fn empty_state_removes_the_file() {
        let dir = TestDir::new("empty");
        let path = dir.path().join(STORE_FILE);
        save(&path, &state()).unwrap();
        save(&path, &StoredState::default()).unwrap();
        assert!(!path.exists());
        save(&path, &StoredState::default()).unwrap();
    }

    #[test]
    fn a_missing_broken_or_newer_file_loads_as_empty() {
        let dir = TestDir::new("broken");
        let path = dir.path().join(STORE_FILE);
        assert_eq!(load(&path), StoredState::default());
        std::fs::write(&path, "{ not json").unwrap();
        assert_eq!(load(&path), StoredState::default());
        std::fs::write(&path, r#"{"version": 99, "links": []}"#).unwrap();
        assert_eq!(load(&path), StoredState::default());
    }

    #[test]
    fn a_file_with_only_some_sections_loads() {
        let dir = TestDir::new("partial");
        let path = dir.path().join(STORE_FILE);
        std::fs::write(
            &path,
            r#"{"version":1,"links":[{"pane_id":"w2:p1","supervisor_pane_id":"w1:p2"}]}"#,
        )
        .unwrap();
        let loaded = load(&path);
        assert_eq!(loaded.links.len(), 1);
        assert!(loaded.links[0].report.is_empty());
        assert!(loaded.deliveries.is_empty());
    }
}
