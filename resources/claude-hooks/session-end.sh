#!/usr/bin/env bash
# heed claude-hooks/session-end.sh — emits session_end on Claude's SessionEnd.
# Lets the daemon mark a thread `gone` immediately on clean exit, rather
# than waiting for the 30s liveness poll cycle.
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

printf '{"event":"session_end","ts":%s,"cli":"claude","thread_id":"%s","pid":%s,"pid_start":"%s"}\n' \
  "$ts" \
  "$(escape_json "$sid")" \
  "$ppid" \
  "$(escape_json "$pid_start")" \
  >> "$event_log"
