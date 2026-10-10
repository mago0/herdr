# Test baseline

Tests that fail or cannot run on this machine without a fork change. A failure outside this list is
a finding.

## Always exclude

```
not (test(deep_scrollback_resize_preserves_unicode_and_hyperlinks) | test(client_shell_text_input_renders_only_when_resetting_scrollback))
```

- `deep_scrollback_resize_preserves_unicode_and_hyperlinks` hangs.
- `client_shell_text_input_renders_only_when_resetting_scrollback` aborts with SIGABRT in a
  debug-only libghostty check and puts a crash notice on the desktop.

## Known failures

- `detect::tests::foreground_job_detects_agent_behind_shell_wrapper`
- `pane::terminal::migration_tests::primary_screen_replay_honors_ed3_for_droid_at_chunk_boundaries`

## Last result

Upstream `2563803d`: 4145 run, 4143 passed, the 2 known failures.
