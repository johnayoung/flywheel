#!/bin/sh
# flywheel hub sample plugin. stdin: one JSON document,
# {"version":1,"type":"event"|"action","plugin","event"|"action","at","hub","payload"}.
# stdout: nothing, plain text (logged), or a JSON response:
# {"toast":"text"|{"message","tone"}} and, for actions, {"paste":{"text","run"}}.
set -eu
input=$(cat)
case "$FW_HUB_PLUGIN_TRIGGER" in
  event:*)
    # One line per event (grows without bound: a sample, not a logger).
    printf '%s\n' "$input" >> "$FW_HUB_PLUGIN_DIR/events.jsonl"
    ;;
  action:hello)
    printf '{"toast":{"message":"hello from the %s plugin","tone":"ok"}}\n' "$FW_HUB_PLUGIN"
    ;;
  action:git-status)
    printf '{"paste":{"text":"git status --short","run":true}}\n'
    ;;
  *)
    echo "unexpected trigger $FW_HUB_PLUGIN_TRIGGER" >&2
    exit 1
    ;;
esac
