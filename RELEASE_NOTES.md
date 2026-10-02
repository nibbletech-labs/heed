## v0.4.4 — Codex Worker Tracking

- Codex sessions started from inside a Claude Code session now record which session started them, so apps can show them as that session's workers.
- Ownership records for sessions that haven't run in two weeks are now cleared instead of building up forever.

## v0.4.3 — Reliable Codex Session Tracking

- Codex sessions keep their own agents and activity when several sessions run at once.
- Restarting Heed clears incorrect session links saved by older versions.

## v0.4.2 — Codex Agent Monitoring

- Track Codex child agents and nested agents alongside Claude agents, with running and completed status and recorded tool activity.
- Recover monitoring of live Codex sessions and their agents after Heed restarts.
- Provide task names and worktree activity for Codezilla’s Haven ticket grouping.
