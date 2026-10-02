#!/usr/bin/env bash
# heed codex-hooks/session-end.sh — emits session_end on Codex's SessionEnd.
set -eu

stdin=$(cat || true)
ts=$(/bin/date +%s.%N)

sid=$(printf '%s' "$stdin" | sed -n 's/.*"session_id":"\([^"]*\)".*/\1/p' | head -1)
[ -z "$sid" ] && exit 0

event_log="${HOME}/.heed/events.jsonl"
mkdir -p "$(dirname "$event_log")"

ppid="${PPID:-0}"
pid_start=""
if [ "$ppid" != "0" ]; then
  pid_start=$(ps -o lstart= -p "$ppid" 2>/dev/null | sed 's/^ *//;s/ *$//' || true)
fi

escape_json() {
  printf '%s' "$1" | sed -e 's/\\/\\\\/g' -e 's/"/\\"/g'
}

# A Codex session started from inside a Claude Code session (a worker it
# launched with `codex exec`, or via the Codex plugin) inherits that session's
# id; record it so consumers can show the worker under the Claude session.
spawned_by=""
if [ -n "${CLAUDE_CODE_SESSION_ID:-}" ]; then
  spawned_by=',"spawned_by":"claude:'"$(printf '%s' "$CLAUDE_CODE_SESSION_ID" | sed -e 's/\\/\\\\/g' -e 's/"/\\"/g')"'"'
fi

printf '{"event":"session_end","ts":%s,"cli":"codex","thread_id":"%s","pid":%s,"pid_start":"%s"%s}\n' \
  "$ts" \
  "$(escape_json "$sid")" \
  "$ppid" \
  "$(escape_json "$pid_start")" \
  "$spawned_by" \
  >> "$event_log"
