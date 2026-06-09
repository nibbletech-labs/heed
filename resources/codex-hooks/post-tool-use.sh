#!/usr/bin/env bash
# heed codex-hooks/post-tool-use.sh — emits tool_use on PostToolUse.
# Codex has no TaskUpdate / TodoWrite tools so plan-progress fields are
# never emitted. Ported from Codezilla 1.0.0 with Heed modifications.
set -eu

stdin=$(cat || true)
ts=$(/bin/date +%s.%N)

sid=$(printf '%s' "$stdin" | sed -n 's/.*"session_id":"\([^"]*\)".*/\1/p' | head -1)
[ -z "$sid" ] && exit 0

event_log="${HOME}/.heed/events.jsonl"
mkdir -p "$(dirname "$event_log")"

tpath=$(printf '%s' "$stdin" | sed -n 's/.*"transcript_path":"\([^"]*\)".*/\1/p' | head -1)
cwd=$(printf '%s' "$stdin" | sed -n 's/.*"cwd":"\([^"]*\)".*/\1/p' | head -1)
if [ -z "$cwd" ]; then
  cwd="${PWD:-}"
fi

ppid="${PPID:-0}"
pid_start=""
if [ "$ppid" != "0" ]; then
  pid_start=$(ps -o lstart= -p "$ppid" 2>/dev/null | sed 's/^ *//;s/ *$//' || true)
fi

tool_name=$(printf '%s' "$stdin" | sed -n 's/.*"tool_name":"\([^"]*\)".*/\1/p' | head -1)

tool_target=""
case "$tool_name" in
  Bash)
    # ERE pattern tolerates one level of nested objects inside tool_input
    # before the target field (see claude-hooks/pre-tool-use.sh).
    tool_target=$(printf '%s' "$stdin" | sed -nE 's/.*"tool_input":\{([^{}]|\{[^{}]*\})*"command":"([^"]*)".*/\2/p' | head -1)
    ;;
esac

if [ -n "$tool_target" ] && [ ${#tool_target} -gt 200 ]; then
  tool_target=$(printf '%s' "$tool_target" | cut -c1-200)
fi

escape_json() {
  printf '%s' "$1" | sed -e 's/\\/\\\\/g' -e 's/"/\\"/g'
}

extra='{}'
if [ -n "$tool_name" ]; then
  extra='{"tool_name":"'"$(escape_json "$tool_name")"'"'
  if [ -n "$tool_target" ]; then
    extra="$extra"',"tool_target":"'"$(escape_json "$tool_target")"'"'
  fi
  extra="$extra"'}'
fi

printf '{"event":"tool_use","ts":%s,"cli":"codex","thread_id":"%s","pid":%s,"pid_start":"%s","cwd":"%s","transcript_path":"%s","extra":%s}\n' \
  "$ts" \
  "$(escape_json "$sid")" \
  "$ppid" \
  "$(escape_json "$pid_start")" \
  "$(escape_json "$cwd")" \
  "$(escape_json "$tpath")" \
  "$extra" \
  >> "$event_log"
