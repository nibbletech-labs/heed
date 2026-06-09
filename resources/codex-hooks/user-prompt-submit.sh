#!/usr/bin/env bash
# heed codex-hooks/user-prompt-submit.sh — emits turn_start on UserPromptSubmit.
# Ported from Codezilla 1.0.0 with Heed modifications.
#
# Codex transcript paths are typically NOT in hook stdin — the daemon's
# Codex binder resolves them later by scanning ~/.codex/sessions/.
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

escape_json() {
  printf '%s' "$1" | sed -e 's/\\/\\\\/g' -e 's/"/\\"/g'
}

printf '{"event":"turn_start","ts":%s,"cli":"codex","thread_id":"%s","pid":%s,"pid_start":"%s","cwd":"%s","transcript_path":"%s"}\n' \
  "$ts" \
  "$(escape_json "$sid")" \
  "$ppid" \
  "$(escape_json "$pid_start")" \
  "$(escape_json "$cwd")" \
  "$(escape_json "$tpath")" \
  >> "$event_log"
