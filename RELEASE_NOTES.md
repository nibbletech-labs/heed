## v0.4.3 — Keep Shared Codex Sessions Separate

- Independent Codex sessions hosted by one app-server no longer inherit each other’s owner or disappear from their product’s thread panel.
- Restarting the monitor discards incorrect shared-host succession links saved by older versions.
- If the Codex host process cannot be inspected, Heed keeps the sessions separate. Dedicated Codex processes and Claude session rotation retain their existing behavior.

## v0.4.2 — Codex Agent Monitoring

- Track Codex child agents and nested agents alongside Claude agents, with running and completed status and recorded tool activity.
- Recover monitoring of live Codex sessions and their agents after Heed restarts.
- Provide task names and worktree activity for Codezilla’s Haven ticket grouping.
