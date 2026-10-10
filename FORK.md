# This fork

`mago0/herdr` is a personal fork of [herdrdev/herdr](https://github.com/herdrdev/herdr). Branch `fork` is the fork. Branch `master` mirrors upstream and holds no fork work.

## What the fork adds

- **A sidebar section drawn by a plugin.** The client runs a plugin pane in place of the native spaces list or agents list. Config: `ui.sidebar.spaces.plugin` and `ui.sidebar.agents.plugin`, each `"<plugin id>:<pane id>"`. An empty value, or a plugin that is not installed, keeps the native section.
- **Server support for agents that supervise agents.** Prompts that are typed only when a pane is safe (`herdr agent deliver`), commands that run on an interval, an event or a file change with no pane (`herdr job`), supervisor links with reports (`herdr pane supervise`), event history with a cursor (`herdr events read`), stall detection, and the text of the dialog a blocked agent shows. The socket API reference under `docs/next` describes each.

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
