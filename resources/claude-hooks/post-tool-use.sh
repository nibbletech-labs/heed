#!/usr/bin/env bash
# heed claude-hooks/post-tool-use.sh — emits tool_use on PostToolUse.
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

tool_name=$(printf '%s' "$stdin" | sed -n 's/.*"tool_name":"\([^"]*\)".*/\1/p' | head -1)

# Per-tool target (mirrors pre-tool-use.sh).
# Scope to tool_input via an ERE pattern that tolerates one level of nested
# objects before the target field; without this, a nested object ordered
# before our field silently empties the match. Anchoring to tool_input also
# prevents tool_response echoes (e.g. Read's response containing file_path)
# from being picked up instead of the input.
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

if [ -n "$tool_target" ] && [ ${#tool_target} -gt 200 ]; then
  tool_target=$(printf '%s' "$tool_target" | cut -c1-200)
fi

escape_json() {
  printf '%s' "$1" | sed -e 's/\\/\\\\/g' -e 's/"/\\"/g'
}

# Per-tool plan-progress fields. tool_input.status drives TaskUpdate
# transitions. For TodoWrite we count "status":"..." occurrences inside
# tool_input.todos[*].
task_status=""
todos_total=""
todos_done=""

case "$tool_name" in
  TaskUpdate)
    task_status=$(printf '%s' "$stdin" | sed -nE 's/.*"tool_input":\{([^{}]|\{[^{}]*\})*"status":"([^"]*)".*/\2/p' | head -1)
    ;;
  TodoWrite)
    # Extract the full tool_input slice (with one level of nested objects, i.e.
    # the per-todo records) and count statuses within it. Counting across the
    # whole payload would also pick up tool_response.oldTodos / .newTodos.
    tool_input_slice=$(printf '%s' "$stdin" | sed -nE 's/.*"tool_input":(\{([^{}]|\{[^{}]*\})*\}).*/\1/p')
    if [ -n "$tool_input_slice" ]; then
      todos_total=$(printf '%s' "$tool_input_slice" | grep -o '"status":"[^"]*"' | wc -l | tr -d ' ')
      todos_done=$(printf '%s' "$tool_input_slice" | grep -o '"status":"completed"' | wc -l | tr -d ' ')
    fi
    ;;
esac

extra='{}'
if [ -n "$tool_name" ]; then
  extra='{"tool_name":"'"$(escape_json "$tool_name")"'"'
  if [ -n "$tool_target" ]; then
    extra="$extra"',"tool_target":"'"$(escape_json "$tool_target")"'"'
  fi
  if [ -n "$task_status" ]; then
    extra="$extra"',"task_status":"'"$(escape_json "$task_status")"'"'
  fi
  if [ -n "$todos_total" ]; then
    extra="$extra"',"todos_total":'"$todos_total"',"todos_done":'"$todos_done"
  fi
  extra="$extra"'}'
fi

printf '{"event":"tool_use","ts":%s,"cli":"claude","thread_id":"%s","pid":%s,"pid_start":"%s","cwd":"%s","transcript_path":"%s","extra":%s}\n' \
  "$ts" \
  "$(escape_json "$sid")" \
  "$ppid" \
  "$(escape_json "$pid_start")" \
  "$(escape_json "$cwd")" \
  "$(escape_json "$tpath")" \
  "$extra" \
  >> "$event_log"
