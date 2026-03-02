# enki

Multi-agent coding orchestrator. Rust workspace with crates: `core`, `acp`, `tui`, `cli`.

## Logs

When the user mentions "the logs" or "check the logs", read:

```
~/.enki/logs/enki.log
```

Each TUI session is separated by a `══════════════════ SESSION START ══════════════════` line. The most recent session is at the bottom of the file.

For targeted reading: `tail -n 200 ~/.enki/logs/enki.log` gets the end of the most recent session. Use `grep "ERROR\|WARN" ~/.enki/logs/enki.log` to find problems quickly.

Log levels: `ERROR` = failures, `INFO` = lifecycle events (worker spawned/merged/failed, sessions), `DEBUG` = ACP subprocess args, worktree paths, prompt sizes, session kills.
