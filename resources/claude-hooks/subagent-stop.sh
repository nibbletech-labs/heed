#!/usr/bin/env bash
# heed claude-hooks/subagent-stop.sh — emits subagent_stop on SubagentStop (HD-15).
set -eu

stdin=$(cat || true)
ts=$(/bin/date +%s.%N)

sid=$(printf '%s' "$stdin" | sed -n 's/.*"session_id":"\([^"]*\)".*/\1/p' | head -1)
[ -z "$sid" ] && exit 0

event_log="${HOME}/.heed/events.jsonl"
mkdir -p "$(dirname "$event_log")"

# HD-15: an event from an agent inside the session (a subagent or an
# in-process teammate) carries agent_id / agent_type; session_id is then the
# parent session's. Read them from the part before tool_input so an id inside
# a tool's input or response is never mistaken for the caller's.
head=${stdin%%\"tool_input\":*}
agent_id=$(printf '%s' "$head" | grep -o '"agent_id":"[^"]*"' | head -1 | sed 's/.*:"\(.*\)"/\1/' || true)
agent_type=""
if [ -n "$agent_id" ]; then
  agent_type=$(printf '%s' "$head" | grep -o '"agent_type":"[^"]*"' | head -1 | sed 's/.*:"\(.*\)"/\1/' || true)
fi

# Opt-in raw capture for diagnosing hook payloads: touch ~/.heed/debug-hooks.
if [ -e "${HOME}/.heed/debug-hooks" ]; then
  printf '%s\n' "$stdin" >> "${HOME}/.heed/hook-debug.jsonl"
fi

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

[ -z "$agent_id" ] && exit 0

agent_fields=""
if [ -n "$agent_id" ]; then
  agent_fields=',"agent_id":"'"$(escape_json "$agent_id")"'"'
  if [ -n "$agent_type" ]; then
    agent_fields="$agent_fields"',"agent_type":"'"$(escape_json "$agent_type")"'"'
  fi
fi

printf '{"event":"subagent_stop","ts":%s,"cli":"claude","thread_id":"%s","pid":%s,"pid_start":"%s","cwd":"%s","transcript_path":"%s"%s}\n' \
  "$ts" \
  "$(escape_json "$sid")" \
  "$ppid" \
  "$(escape_json "$pid_start")" \
  "$(escape_json "$cwd")" \
  "$(escape_json "$tpath")" \
  "$agent_fields" \
  >> "$event_log"
