#!/usr/bin/env bash
# Classifies the files of an upstream merge by who owns them: the fork or upstream.
set -euo pipefail

upstream_ref="${UPSTREAM_REF:-upstream/master}"
# "trial" reads the fork branch. The other commands run in the merge worktree and read HEAD,
# which holds the fork commits and any commit made on the merge branch.
case "${1:-}" in
trial) fork_ref="${FORK_REF:-fork}" ;;
*) fork_ref="${FORK_REF:-HEAD}" ;;
esac

# Files that a fork commit changed. Release tags are excluded: upstream cuts them off master.
fork_owned() {
    git log --no-merges --format= --name-only "$fork_ref" --not "$upstream_ref" --tags | sort -u
}

case "${1:-}" in
owned)
    fork_owned
    ;;
trial)
    # Conflicts of a merge that is not started yet. No checkout is changed.
    git fetch --quiet upstream
    echo "fork-only / upstream-only commits: $(git rev-list --left-right --count "$fork_ref...$upstream_ref")"
    conflicts="$(git merge-tree --write-tree --name-only --no-messages "$fork_ref" "$upstream_ref" | tail -n +2 | sort -u || true)"
    echo "== take upstream (the fork never changed these)"
    comm -23 <(echo "$conflicts") <(fork_owned)
    echo "== resolve by hand (the fork changed these)"
    comm -12 <(echo "$conflicts") <(fork_owned)
    ;;
take-upstream)
    # In a started merge: every file the fork never changed gets the upstream content.
    git diff --name-only "$upstream_ref" | sort | comm -23 - <(fork_owned) |
        while IFS= read -r file; do
            if git cat-file -e "$upstream_ref:$file" 2>/dev/null; then
                git checkout "$upstream_ref" -- "$file"
            else
                git rm -q --cached --ignore-unmatch -- "$file" && rm -f -- "$file"
            fi
            echo "upstream: $file"
        done
    ;;
verify)
    # After the merge: a file that differs from upstream must be one the fork changed.
    stray="$(git diff --name-only "$upstream_ref" | sort | comm -23 - <(fork_owned))"
    if [ -n "$stray" ]; then
        echo "differs from upstream, but no fork commit changed it:"
        echo "$stray"
        exit 1
    fi
    echo "ok: only fork-owned files differ from $upstream_ref"
    ;;
*)
    echo "usage: fork-merge.sh owned|trial|take-upstream|verify" >&2
    exit 2
    ;;
esac
