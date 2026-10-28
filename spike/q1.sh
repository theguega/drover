#!/bin/sh
# usage: q1.sh MODE TIMEOUT  (env: OUT, PORT, PROMPT, EXTRA claude flags)
# Headless stream-json run with the PermissionRequest + PreToolUse http hooks; logs in $OUT.
D=$(cd "$(dirname "$0")" && pwd); OUT=${OUT:-/tmp/spike-out}; PORT=${PORT:-8791}
PROMPT=${PROMPT:-Run this exact bash command with the Bash tool, then report what happened: touch spike-file.txt}
mkdir -p $OUT/work; rm -f $OUT/hook.log $OUT/work/spike-file.txt
LOG=$OUT/hook.log MODE=${1:-allow} python3 $D/hook_server.py $PORT & SRV=$!
sleep 1
"$D/settings.sh" $PORT ${2:-30} > $OUT/settings.json
cd $OUT/work; date +%s > $OUT/start
jq -cn --arg p "$PROMPT" '{type:"user",message:{role:"user",content:$p}}' |
  claude -p --input-format stream-json --output-format stream-json --verbose --model haiku \
  --settings $OUT/settings.json --permission-mode default $EXTRA > $OUT/stream.jsonl 2> $OUT/stderr
date +%s > $OUT/end; kill $SRV
