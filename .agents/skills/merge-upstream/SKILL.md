---
name: merge-upstream
description: Merge upstream herdrdev/herdr master into the fork branch and prove the fork features still work. Use when the user says "pull upstream", "merge herdr main", "sync the fork", "update the fork from upstream", or asks what upstream changed since the last merge.
---

# Merge upstream into the fork

Use this skill only in the `mago0/herdr` fork. The fork branch is `fork`; `master` mirrors
`upstream/master`. Never open an issue or a pull request on `herdrdev/herdr`.

The fork features to protect: sidebar sections drawn by plugin panes, the agent panel switch,
`agent deliver`, `agent answer`, `job`, `pane supervise`, `events read`, stall detection, blocker
text, the `+fork` version mark, and `just install-fork`.

`scripts/fork-merge.sh` splits files by owner. A file is fork-owned when a fork commit changed it.

## 1. Look before the merge

```sh
.agents/skills/merge-upstream/scripts/fork-merge.sh trial
git log --no-merges --format=%s fork..upstream/master | sort
```

`trial` fetches upstream and lists the conflicts in two groups. It changes no checkout. Tell the
user what upstream brings and whether a release tag newer than the fork base exists: a merge of
`master` puts unreleased upstream code into the fork.

## 2. Merge in a worktree

```sh
git worktree add -b merge/upstream-<date> ../herdr-worktrees/merge-upstream-<date> fork
cd ../herdr-worktrees/merge-upstream-<date>
git merge --no-commit --no-ff upstream/master
.agents/skills/merge-upstream/scripts/fork-merge.sh take-upstream
```

`take-upstream` gives upstream content to every file the fork never changed. Those conflicts come
from upstream itself: release commits are cut beside `master`, so a backport on the release line
collides with the `master` version of the same fix.

Resolve the fork-owned conflicts by hand. Read both sides first:
`git diff <merge-base> fork -- <file>` and `git diff <merge-base> upstream/master -- <file>`.
Keep the upstream structure and put the fork behavior back into it. Keep both sides of a test
conflict. `rerere` is on, so a resolution is recorded for the next merge.

## 3. Check what a clean text merge does not show

- Each code path that closes a pane, tab or workspace must still call `emit_panes_closed`. Compare
  `grep -rn emit_panes_closed src` with `grep -rn "event: EventKind::.*Closed" src`, and look for
  new sites: `git diff <merge-base> upstream/master -- src | grep "^+.*Closed"`.
- Upstream changes in `src/client/shell/sidebar.rs`, `mouse.rs` and `input.rs` meet the plugin
  sections. Read those diffs even when they merge clean.
- A method added to `CLIENT_SHELL_METHODS` or a type a bincode codec reaches is frozen. Do not
  change a frozen expectation to make a test pass.

## 4. Prove it

```sh
nix develop --command cargo build --locked
nix develop --command cargo nextest run --locked --no-fail-fast -E '<filter from baseline.md>'
nix develop --command just lint
nix develop --command just maintenance-test
.agents/skills/merge-upstream/scripts/fork-merge.sh verify
```

A test failure is a finding unless [baseline.md](baseline.md) lists it. When the list changes,
update that file in the merge branch.

Then run the hands-on check in [isolated-check.md](isolated-check.md). It covers the sidebar
plugin pane, a job, a supervision report, a delivery to a real agent, and a live handoff from the
installed fork binary to the merged one.

## 5. Finish

Propose the commit message before the commit: `merge: upstream master at <short sha>`, lowercase,
no AI co-author line. The user does the fast-forward of `fork`, `just install-fork` and the
handoff of the live servers. Do not change the live config, sessions or servers.

Report: commits merged, conflicts by group, test result against the baseline, what the hands-on
check covered, and what it did not cover.
