#!/usr/bin/env bash
# heed claude-hooks/pre-tool-use.sh — emits pre_tool_use on PreToolUse.
# Ported from Codezilla 1.4.0 with Heed modifications (see user-prompt-submit.sh).
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

# Extract tool_name with a minimal regex (avoid jq dependency).
tool_name=$(printf '%s' "$stdin" | sed -n 's/.*"tool_name":"\([^"]*\)".*/\1/p' | head -1)

# Per-tool target: a short user-facing string identifying *what* the tool is
# acting on. Drives the "Reading package.json" / "Running npm test" subtitles.
# Meta tools (AskUserQuestion, *PlanMode, TodoWrite, TaskUpdate) have no
# meaningful target; the reducer ignores tool_target for those.
#
# Scope to tool_input via an ERE pattern that tolerates one level of nested
# objects (e.g. tool_input.options:{...}) before the target field — without
# this, a single nested object ordered before our field silently empties the
# match. Doesn't handle quoted '{'/'}' inside string values, same as before.
tool_target=""
case "$tool_name" in
  Read|Write|Edit)
    tool_target=$(printf '%s' "$stdin" | sed -nE 's/.*"tool_input":\{([^{}]|\{[^{}]*\})*"file_path":"([^"]*)".*/\2/p' | head -1)
    ;;
  Bash)
    tool_target=$(printf '%s' "$stdin" | sed -nE 's/.*"tool_input":\{([^{}]|\{[^{}]*\})*"command":"([^"]*)".*/\2/p' | head -1)
    ;;
  Grep|Glob)
    tool_target=$(printf '%s' "$stdin" | sed -nE 's/.*"tool_input":\{([^{}]|\{[^{}]*\})*"pattern":"([^"]*)".*/\2/p' | head -1)
    ;;
  TaskCreate)
    tool_target=$(printf '%s' "$stdin" | sed -nE 's/.*"tool_input":\{([^{}]|\{[^{}]*\})*"subject":"([^"]*)".*/\2/p' | head -1)
    ;;
  WebFetch)
    tool_target=$(printf '%s' "$stdin" | sed -nE 's/.*"tool_input":\{([^{}]|\{[^{}]*\})*"url":"([^"]*)".*/\2/p' | head -1)
    ;;
esac

# Truncate so we don't ship megabyte values through the event log.
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

printf '{"event":"pre_tool_use","ts":%s,"cli":"claude","thread_id":"%s","pid":%s,"pid_start":"%s","cwd":"%s","transcript_path":"%s","extra":%s}\n' \
  "$ts" \
  "$(escape_json "$sid")" \
  "$ppid" \
  "$(escape_json "$pid_start")" \
  "$(escape_json "$cwd")" \
  "$(escape_json "$tpath")" \
  "$extra" \
  >> "$event_log"
