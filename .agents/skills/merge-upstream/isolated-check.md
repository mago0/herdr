# Hands-on check in an isolated session

The check must not touch the live Herdr. Each session gets its own config folder, its own tmux
server, and a shell without the `HERDR_*` variables.

## Rules

- Use a short config path such as `/tmp/hm`. A long path breaks the socket path limit.
- Do not set `XDG_DATA_HOME`: mise then loses the `fork` link inside the panes.
- A debug build reads the config folder `herdr-dev`, a release build reads `herdr`.
- Copy `config.toml`, `plugins.json` and `plugins/` from the live config folder, then rewrite the
  managed plugin paths in the copy to the new folder.
- Plugin state is in `~/.local/state/<herdr|herdr-dev>/plugins/`, outside the config folder. A
  release build session shares that folder with the live server. A plugin daemon started by the
  session can outlive it: after cleanup, `pgrep -af /tmp/hm` must print nothing.

## Session A: merged debug build

```sh
D=/tmp/hm; mkdir -p $D/herdr-dev
cp ~/.config/herdr/{config.toml,plugins.json} $D/herdr-dev/ && cp -a ~/.config/herdr/plugins $D/herdr-dev/
sed -i "s#$HOME/.config/herdr/plugins/#$D/herdr-dev/plugins/#g" $D/herdr-dev/plugins.json
cat > $D/env.sh <<EOT
unset HERDR_BIN_PATH HERDR_ENV HERDR_PANE_ID HERDR_SOCKET_PATH HERDR_TAB_ID HERDR_WORKSPACE_ID TMUX
export XDG_CONFIG_HOME=$D
H=$PWD/target/debug/herdr
EOT
tmux -L hm new-session -d -s t -x 180 -y 45 "zsh -c 'source $D/env.sh; cd $D; \$H; sleep 600'"
```

Source `env.sh` in each command shell, then check in this order:

1. `tmux -L hm capture-pane -p -t t`: the sidebar shows the tree plugin rows, not the stock list.
2. `$H job add tick --every 2s -- sh -c 'date >> /tmp/hm/tick.log'`: the file grows, `job list`
   counts the runs. Remove the job.
3. `$H pane split w1:p1 --direction right`, `$H pane run w1:p2 claude`: `agent list` shows
   `blocked` with `blocker` text and a `dialog` id for the trust dialog.
4. `$H pane supervise w1:p2 --by w1:p1 --report blocked,exited,stalled`: `orchestration.json` in
   the config folder holds the link and a queued `HERDR|blocked|...` report. The report stays
   queued while the supervisor pane is a plain shell.
5. `$H agent answer --dialog <id> w1:p2 2` answers a numbered dialog only. The trust dialog has no
   numbers: use `$H pane send-keys w1:p2 Down Enter`.
6. `$H agent deliver --source check w1:p2 "Reply with only the word pong."`: state `delivered`,
   and the pane shows the prompt and the answer.
7. `$H events read --after 0 --limit 50`: job, status, delivery and pane events are present.
8. `grep "ERROR\|panic" $D/herdr-dev/herdr-*.log` prints nothing.

## Session B: live handoff

Start a second session (`/tmp/hm2`, config folder `herdr`, tmux `-L hm2`) with
`H=~/.local/lib/herdr-fork/herdr`. Start an agent and add a job. Build the merged tree with
`cargo build --release --locked`, copy the binary to `/tmp/hm2/new/herdr`, then:

```sh
$H server live-handoff --import-exe /tmp/hm2/new/herdr
```

The attached client exits with "live update in progress". Check with the new binary: the server
process runs from `/tmp/hm2/new/herdr`, `agent list` and `job list` show the same agent and job,
and a new attach shows the pane contents.

## Cleanup

`server stop` in each session, `tmux -L <name> kill-server`, kill any process `pgrep -af /tmp/hm`
still shows, remove the folders. Confirm in a clean shell that `herdr status server` shows the live
server running.

## Not covered

Mouse clicks and the right-click menu on a plugin row, and the stalled report. Say so in the
report, or drive them by hand.
