# This fork

`mago0/herdr` is a personal fork of [herdrdev/herdr](https://github.com/herdrdev/herdr). Branch `fork` is the fork. Branch `master` mirrors upstream and holds no fork work.

## What the fork adds

- **A sidebar section drawn by a plugin.** The client runs a plugin pane in place of the native spaces list or agents list. Config: `ui.sidebar.spaces.plugin` and `ui.sidebar.agents.plugin`, each `"<plugin id>:<pane id>"`. An empty value, or a plugin that is not installed, keeps the native section.
- **Server support for agents that supervise agents.** Prompts that are typed only when a pane is safe (`herdr agent deliver`), commands that run on an interval, an event or a file change with no pane (`herdr job`), supervisor links with reports (`herdr pane supervise`), event history with a cursor (`herdr events read`), stall detection, and the text of the dialog a blocked agent shows. The socket API reference under `docs/next` describes each.
- **Supervision that outlives a supervisor.** When a supervisor pane closes, the panes it supervised move up to the nearest open supervisor above it, which gets one `HERDR|orphaned|...` report for each. See below.
- **Agent launches that need no workaround.** `agent start` waits until a new pane's shell can take a whole command. `herdr agent resume` starts an agent again on a session it recorded. `--prompt-file` gives either one a first prompt. See below.

## Agent launch

Herdr types an agent's command into the pane's shell. Three things make that safe for a tool that starts or recovers agents:

- **Shell wait.** `agent start` and `agent resume` wait until the shell's line editor reads input and the pane has printed nothing for 200 ms, for at most 10 seconds. Text typed while the shell still runs its startup files can be read by a startup step, such as a terminal probe, and the shell then runs `laude ...` in place of `claude ...`. `pane.shell_state` reports `ready`, `starting` or `busy`.
- **Resume.** `herdr agent resume <name>` and `agent.resume` take a session as `agent get` reports it in `agent_session` (`--agent-session JSON`), a kind with `--session-id` or `--session-path`, or `--from` a pane that records one. Herdr builds the command with the same planner it uses to restore panes after a restart, for 18 agent kinds. Native arguments after `--` go into that command. The agent starts in `--pane`, or in a new tab that `--cwd` opens.
- **First prompt.** `--prompt-file FILE` (or `prompt` on the socket) queues the text as a delivery with source `herdr:launch`. It is typed once the agent is ready, and dropped when the launch fails.

## Supervision trees

Supervision links (`herdr pane supervise`) form a tree. When a supervisor pane closes, Herdr moves each pane it supervised up to the nearest supervisor above it whose pane is still open. The moved link keeps its report kinds. `pane get` and `agent get` then show the new supervisor in `supervisor_pane_id`.

The new supervisor gets one line for each moved pane, through the same queue and the same `pane.supervise_report` event as the other reports:

```text
HERDR|orphaned|<agent>|<pane>|<former supervisor pane>
```

In the event, `kind` is `orphaned` and `detail` is the pane ID of the former supervisor. A moved pane that is blocked at that time, with a link that reports `blocked`, is reported blocked again to its new supervisor.

This is the default for every link. A link cannot ask for `orphaned`, and a link with no open supervisor above it ends, as before. The moved links are ordinary links in `orchestration.json`. A supervisor that closes just before a restart or a live handoff is handled before the state is saved, and the `orphaned` lines wait in the saved queue for the next server.

## Well-known pane tokens

Pane tokens are one map for each pane (`pane.report_metadata`). Two of them have a fixed meaning, so any plugin can write or read them:

- `label`: a short tag shown at the right edge of the pane's row.
- `link`: an `http` or `https` address that a sidebar opens for the row.

The client's pane label dialog writes `label` under the source `herdr`. An empty label clears it.

## Workspaces opened for worktrees

`worktree create` and `worktree open` open the repository's main checkout as a parent workspace when none is open. Herdr marks that workspace with `opened_for_worktrees: true` in `workspace get` and `workspace list`. The mark goes away when the workspace is renamed or gets a second pane, and then `workspace.updated` is emitted. A sidebar can hide marked workspaces.

## Sidebar tree

The default for `ui.sidebar.spaces.plugin` is `herdr-tree:tree`: agents as a tree by supervisor. The plugin is not part of this repository. Install it once:

```sh
herdr plugin install mago0/herdr-plugins/tree
```

The install builds the plugin, which needs Go. Until the plugin is installed the native spaces list stays, and the client says so one time with this command. To keep the native list and no notice, set:

```toml
[ui.sidebar.spaces]
plugin = ""
```

The fork starts a sidebar plugin pane again when its program file changes, so a new build of the plugin needs no restart.

## Build and install

`nix develop --command just install-fork` builds the release binary and copies it to `~/.local/lib/herdr-fork/herdr`. Put that folder on `PATH`, or point a version manager at it. The flake has the Rust and Zig versions the build needs.

A running server keeps the binary it started with. `herdr server live-handoff --import-exe ~/.local/lib/herdr-fork/herdr` moves it to the new one and keeps its panes.

## Version

A fork build reports the upstream version it is made from with `+fork` after it, for example `herdr 0.9.3+fork`. Update checks, plugin version limits and the release a remote machine gets still use the upstream number.
