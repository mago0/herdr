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
    /// Workspace ids, which also stay the same across a restore.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(super) opened_for_worktrees: Vec<String>,
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
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        deserialize_with = "known_reports"
    )]
    pub(super) report: Vec<SuperviseReport>,
}

/// Reads the report kinds this build knows and leaves out the others.
fn known_reports<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Vec<SuperviseReport>, D::Error> {
    let raw = Vec::<serde_json::Value>::deserialize(deserializer)?;
    Ok(raw
        .into_iter()
        .filter_map(|value| serde_json::from_value(value).ok())
        .collect())
}

fn default_timeout_ms() -> u64 {
    super::jobs::DEFAULT_TIMEOUT_MS
}

fn default_true() -> bool {
    true
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
    #[serde(default = "default_timeout_ms")]
    pub(super) timeout_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) until_ms: Option<u64>,
    #[serde(default = "default_true")]
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
            && self.opened_for_worktrees.is_empty()
    }
}

/// A missing file loads as empty. A file this build cannot read, or one from a newer build, is
/// renamed aside before it loads as empty, so that the next save does not replace it.
pub(super) fn load(path: &Path) -> StoredState {
    let reason = match std::fs::read_to_string(path) {
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return StoredState::default(),
        // A file that is there and cannot be read as text is kept aside too.
        Err(err) => err.to_string(),
        Ok(text) => match serde_json::from_str::<StoredState>(&text) {
            Ok(state) if state.version <= STORE_VERSION => return state,
            Ok(state) => format!("it is version {} from a newer build", state.version),
            Err(err) => err.to_string(),
        },
    };
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0);
    let aside = path.with_extension(format!("json.unread-{stamp}"));
    match std::fs::rename(path, &aside) {
        Ok(()) => tracing::warn!(
            reason = %reason,
            kept = %aside.display(),
            "orchestration state was not read; the file is kept aside"
        ),
        Err(err) => tracing::warn!(
            reason = %reason,
            error = %err,
            path = %path.display(),
            "orchestration state was not read and could not be set aside"
        ),
    }
    StoredState::default()
}

pub(super) fn save(path: &Path, state: &StoredState) -> std::io::Result<()> {
    use std::io::Write;

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
    let json = serde_json::to_string_pretty(&state)?;
    let tmp = path.with_extension(format!("json.tmp-{}", std::process::id()));
    let _ = std::fs::remove_file(&tmp);
    // The file holds prompt text and job environment values, so only its owner may read it.
    let written = crate::platform::create_config_temporary(&tmp, true).and_then(|mut file| {
        file.write_all(json.as_bytes())?;
        file.sync_all()
    });
    if let Err(err) = written.and_then(|()| std::fs::rename(&tmp, path)) {
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
            opened_for_worktrees: vec!["w3".into()],
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
    fn a_missing_file_loads_as_empty() {
        let dir = TestDir::new("missing");
        let path = dir.path().join(STORE_FILE);
        assert_eq!(load(&path), StoredState::default());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[test]
    fn a_file_this_build_cannot_read_is_set_aside_and_not_lost() {
        for (name, content) in [
            ("broken", &b"{ not json"[..]),
            (
                "newer",
                &br#"{"version": 99, "links": [], "future": true}"#[..],
            ),
            // A write that was cut can leave bytes that are not text.
            ("not-text", &b"{\"links\": [\xff\xfe"[..]),
        ] {
            let dir = TestDir::new(name);
            let path = dir.path().join(STORE_FILE);
            std::fs::write(&path, content).unwrap();
            assert_eq!(load(&path), StoredState::default(), "{name}");
            assert!(
                !path.exists(),
                "{name}: the file must not stay where a save replaces it"
            );
            // A save after the refused load must leave the old content on disk.
            save(&path, &state()).unwrap();
            let kept = std::fs::read_dir(dir.path())
                .unwrap()
                .filter_map(|entry| entry.ok())
                .map(|entry| entry.path())
                .find(|kept| kept != &path)
                .unwrap_or_else(|| panic!("{name}: nothing was set aside"));
            assert_eq!(std::fs::read(kept).unwrap(), content, "{name}");
        }
    }

    #[test]
    fn a_job_or_link_saved_with_a_part_this_build_does_not_know_still_loads() {
        let dir = TestDir::new("lenient");
        let path = dir.path().join(STORE_FILE);
        std::fs::write(
            &path,
            r#"{"version":1,
                "links":[{"pane_id":"w2:p1","supervisor_pane_id":"w1:p2","report":["blocked","paused"]}],
                "jobs":[{"name":"old","command":["true"]}]}"#,
        )
        .unwrap();
        let loaded = load(&path);
        assert_eq!(loaded.links[0].report, vec![SuperviseReport::Blocked]);
        assert_eq!(loaded.jobs[0].timeout_ms, 60_000);
        assert!(loaded.jobs[0].dedupe);
        assert!(path.exists());
    }

    #[cfg(unix)]
    #[test]
    fn the_saved_file_is_readable_by_its_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TestDir::new("mode");
        let path = dir.path().join(STORE_FILE);
        save(&path, &state()).unwrap();
        save(&path, &state()).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
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
