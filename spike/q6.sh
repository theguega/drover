#!/bin/sh
# Q6: cursor-agent headless with a project-level beforeShellExecution hook. usage: q6.sh [HOLD_SECS]
D=$(cd "$(dirname "$0")" && pwd); OUT=${OUT:-/tmp/spike-q6}; rm -rf $OUT; mkdir -p $OUT/work/.cursor; cd $OUT/work; git init -q
cat > .cursor/hooks.json <<J
{"version":1,"hooks":{"beforeShellExecution":[{"command":"env LOG=$OUT/hook.log HOLD=${1:-3} $D/cursor_hook.sh"}]}}
J
date +%s.%N > $OUT/start
cursor-agent -p --output-format stream-json --force --trust "Run this exact shell command and quote any denial reason verbatim: touch spike-file.txt" > $OUT/stream.jsonl 2> $OUT/stderr
date +%s.%N > $OUT/end
cat $OUT/hook.log | cut -c1-300; echo "file: $(ls $OUT/work)"; echo "elapsed: $(echo "$(cat $OUT/end) - $(cat $OUT/start)" | bc)"
jq -c 'select(.type=="tool_call" or .type=="result" or .type=="assistant")' $OUT/stream.jsonl | cut -c1-400
