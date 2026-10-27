//! Workspaces that Herdr opened as the parent of a worktree and that no person has taken up yet.
//!
//! The mark goes away when the workspace is renamed or gets a second pane.

use crate::api::schema::{EventData, EventEnvelope, EventKind};
use crate::app::App;

impl App {
    /// Marks a workspace that `worktree create` or `worktree open` opened for the repository's
    /// main checkout.
    pub(crate) fn mark_opened_for_worktrees(&mut self, ws_idx: usize) {
        self.ensure_orchestration_loaded();
        let Some(workspace) = self.state.workspaces.get(ws_idx) else {
            return;
        };
        if self
            .orchestration
            .opened_for_worktrees
            .insert(workspace.id.clone())
        {
            self.orchestration.dirty = true;
        }
    }

    pub(crate) fn is_opened_for_worktrees(&self, ws_idx: usize) -> bool {
        self.state.workspaces.get(ws_idx).is_some_and(|workspace| {
            self.orchestration
                .opened_for_worktrees
                .contains(&workspace.id)
        })
    }

    /// The workspace whose mark an event clears: a rename, or a pane that makes it two. A closed
    /// workspace loses its mark here.
    pub(crate) fn worktree_parent_taken_up(&mut self, event: &EventEnvelope) -> Option<usize> {
        if self.orchestration.opened_for_worktrees.is_empty() {
            return None;
        }
        let workspace_id = match &event.data {
            EventData::WorkspaceClosed { workspace_id, .. } => {
                if self.orchestration.opened_for_worktrees.remove(workspace_id) {
                    self.orchestration.dirty = true;
                }
                return None;
            }
            EventData::WorkspaceRenamed { workspace_id, .. } => workspace_id,
            EventData::PaneCreated { pane } => &pane.workspace_id,
            EventData::PaneMoved { pane, .. } => &pane.workspace_id,
            _ => return None,
        };
        if !self
            .orchestration
            .opened_for_worktrees
            .contains(workspace_id)
        {
            return None;
        }
        let ws_idx = self
            .state
            .workspaces
            .iter()
            .position(|workspace| &workspace.id == workspace_id)?;
        let renamed = matches!(event.data, EventData::WorkspaceRenamed { .. });
        (renamed || self.state.workspaces[ws_idx].public_pane_numbers.len() > 1).then_some(ws_idx)
    }

    /// Clears the mark and tells subscribers with `workspace.updated`.
    pub(crate) fn clear_opened_for_worktrees(&mut self, ws_idx: usize) {
        let Some(workspace) = self.state.workspaces.get(ws_idx) else {
            return;
        };
        if !self
            .orchestration
            .opened_for_worktrees
            .remove(&workspace.id)
        {
            return;
        }
        self.orchestration.dirty = true;
        self.emit_event(EventEnvelope {
            event: EventKind::WorkspaceUpdated,
            data: EventData::WorkspaceUpdated {
                workspace: self.workspace_info(ws_idx),
            },
        });
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::path::{Path, PathBuf};

    use crate::api::schema::{
        EventData, Method, Request, ResponseResult, SuccessResponse, WorktreeCreateParams,
    };
    use crate::app::App;
    use crate::config::Config;
    use crate::workspace::Workspace;

    fn temp_path(name: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or(0);
        std::env::temp_dir().join(format!(
            "herdr-worktree-parents-{name}-{}-{nanos}",
            std::process::id()
        ))
    }

    fn git(repo: &Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .status()
            .unwrap();
        assert!(status.success(), "git {}", args.join(" "));
    }

    fn committed_repo(name: &str) -> PathBuf {
        let repo = temp_path(name);
        std::fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "--quiet"]);
        git(&repo, &["config", "user.email", "herdr@example.invalid"]);
        git(&repo, &["config", "user.name", "Herdr Test"]);
        std::fs::write(repo.join("README.md"), "test\n").unwrap();
        git(&repo, &["add", "README.md"]);
        git(&repo, &["commit", "--quiet", "-m", "initial"]);
        repo
    }

    fn app(worktree_root: &Path) -> App {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &Config::default(),
            crate::app::AppPolicy::TEST,
            None,
            api_rx,
            crate::api::EventHub::default(),
        );
        app.state.default_shell = "/usr/bin/true".into();
        app.state.worktree_directory = worktree_root.to_path_buf();
        app
    }

    fn create_worktree(app: &mut App, repo: &Path, branch: &str) -> String {
        let (respond_to, responses) = std::sync::mpsc::channel();
        let request = Request {
            id: "req".into(),
            method: Method::WorktreeCreate(WorktreeCreateParams {
                cwd: Some(repo.display().to_string()),
                branch: Some(branch.into()),
                ..WorktreeCreateParams::default()
            }),
        };
        assert!(app.handle_deferred_worktree_api_request(request, respond_to, false));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            if let Ok(response) = responses.try_recv() {
                let success: SuccessResponse = serde_json::from_str(&response).unwrap();
                let ResponseResult::WorktreeCreated { workspace, .. } = success.result else {
                    panic!("expected worktree_created, got {response}");
                };
                return workspace.workspace_id;
            }
            // Only the end of the git command: the test shell exits at once, and its exit would
            // close the workspaces under test.
            if let Ok(event) = app.event_rx.try_recv() {
                if matches!(event, crate::events::AppEvent::WorktreeAddFinished(_)) {
                    app.handle_internal_event(event);
                }
                continue;
            }
            assert!(std::time::Instant::now() < deadline, "no worktree response");
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    fn marked(app: &App) -> Vec<String> {
        let mut ids: Vec<String> = (0..app.state.workspaces.len())
            .filter(|&ws_idx| app.workspace_info(ws_idx).opened_for_worktrees)
            .map(|ws_idx| app.public_workspace_id(ws_idx))
            .collect();
        ids.sort();
        ids
    }

    fn clean_up(mut app: App, dirs: &[&Path]) {
        for (_, runtime) in app.terminal_runtimes.drain() {
            runtime.shutdown();
        }
        for dir in dirs {
            let _ = std::fs::remove_dir_all(dir);
        }
    }

    #[tokio::test]
    async fn a_parent_herdr_opens_for_a_worktree_is_marked_and_the_worktree_is_not() {
        let repo = committed_repo("opened");
        let root = temp_path("opened-root");
        let mut app = app(&root);

        let worktree_id = create_worktree(&mut app, &repo, "worktree/opened");
        let parent_idx = app
            .state
            .workspaces
            .iter()
            .position(|ws| ws.id != worktree_id)
            .unwrap();
        let parent_id = app.public_workspace_id(parent_idx);
        assert_eq!(marked(&app), vec![parent_id.clone()]);
        let created = app
            .event_hub
            .events_after(0)
            .into_iter()
            .find_map(|(_, event)| match event.data {
                EventData::WorkspaceCreated { workspace }
                    if workspace.workspace_id == parent_id =>
                {
                    Some(workspace)
                }
                _ => None,
            })
            .unwrap();
        assert!(created.opened_for_worktrees);

        // A second worktree finds the parent open and leaves the mark as it is.
        create_worktree(&mut app, &repo, "worktree/second");

        assert_eq!(marked(&app), vec![parent_id]);
        clean_up(app, &[&root, &repo]);
    }

    #[tokio::test]
    async fn a_parent_that_was_already_open_or_that_a_person_opened_is_not_marked() {
        let repo = committed_repo("existing");
        let root = temp_path("existing-root");
        let mut app = app(&root);
        let mut parent = Workspace::test_new("main");
        parent.identity_cwd = repo.clone();
        app.state.workspaces = vec![parent];
        app.state.ensure_test_terminals();
        app.state.active = Some(0);

        create_worktree(&mut app, &repo, "worktree/existing");
        assert_eq!(app.state.workspaces.len(), 2);
        assert!(marked(&app).is_empty());

        let response = app.handle_api_request(
            serde_json::from_value(serde_json::json!({
                "id": "req",
                "method": "workspace.create",
                "params": { "cwd": repo.display().to_string() },
            }))
            .unwrap(),
        );
        let response: serde_json::Value = serde_json::from_str(&response).unwrap();
        assert!(response["result"]["workspace"]["workspace_id"].is_string());
        assert!(response["result"]["workspace"]
            .get("opened_for_worktrees")
            .is_none());
        assert!(marked(&app).is_empty());
        clean_up(app, &[&root, &repo]);
    }
}
