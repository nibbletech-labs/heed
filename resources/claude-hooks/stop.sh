#!/usr/bin/env bash
# heed claude-hooks/stop.sh — emits turn_end on Stop.
# Ported from Codezilla 1.4.0 with Heed modifications.
set -eu

stdin=$(cat || true)
ts=$(/bin/date +%s.%N)

sid=$(printf '%s' "$stdin" | sed -n 's/.*"session_id":"\([^"]*\)".*/\1/p' | head -1)
[ -z "$sid" ] && exit 0

event_log="${HOME}/.heed/events.jsonl"
mkdir -p "$(dirname "$event_log")"

tpath=$(printf '%s' "$stdin" | sed -n 's/.*"transcript_path":"\([^"]*\)".*/\1/p' | head -1)
cwd=$(printf '%s' "$stdin" | sed -n 's/.*"cwd":"\([^"]*\)".*/\1/p' | head -1)

ppid="${PPID:-0}"
pid_start=""
if [ "$ppid" != "0" ]; then
  pid_start=$(ps -o lstart= -p "$ppid" 2>/dev/null | sed 's/^ *//;s/ *$//' || true)
fi

escape_json() {
  printf '%s' "$1" | sed -e 's/\\/\\\\/g' -e 's/"/\\"/g'
}

printf '{"event":"turn_end","ts":%s,"cli":"claude","thread_id":"%s","pid":%s,"pid_start":"%s","cwd":"%s","transcript_path":"%s"}\n' \
  "$ts" \
  "$(escape_json "$sid")" \
  "$ppid" \
  "$(escape_json "$pid_start")" \
  "$(escape_json "$cwd")" \
  "$(escape_json "$tpath")" \
  >> "$event_log"
