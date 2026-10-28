#!/bin/sh
# Q4: answer allow + updatedPermissions(session). Same Bash call 2x in one process (2 user turns),
# then once more in a new process with --resume. Counts PermissionRequest per phase. usage: q4.sh [MODE]
D=$(cd "$(dirname "$0")" && pwd); OUT=${OUT:-/tmp/spike-q4}; PORT=${PORT:-8795}; MODE=${1:-allow-session}
P='Run this exact bash command with the Bash tool and say done: touch spike-file.txt'
mkdir -p $OUT/work; rm -f $OUT/hook.log; cd $OUT/work
LOG=$OUT/hook.log MODE=$MODE python3 $D/hook_server.py $PORT & SRV=$!; sleep 1
"$D/settings.sh" $PORT 30 > $OUT/settings.json
msg() { jq -cn --arg p "$1" '{type:"user",message:{role:"user",content:$p}}'; }
C="claude -p --input-format stream-json --output-format stream-json --verbose --model haiku --settings $OUT/settings.json --permission-mode default"
n() { grep -c '"event": "PermissionRequest"' $OUT/hook.log; }
{ msg "$P"; sleep 20; msg "$P"; sleep 15; } | $C > $OUT/s1.jsonl 2>/dev/null
echo "same process, 2 identical calls: PermissionRequest count = $(n)"
SID=$(jq -r 'select(.type=="system")|.session_id' $OUT/s1.jsonl | head -1)
rm -f spike-file.txt
msg "$P" | $C --resume $SID > $OUT/s2.jsonl 2>/dev/null
echo "after --resume $SID, 3rd identical call: PermissionRequest count = $(n)"
echo "tool results:"; cat $OUT/s1.jsonl $OUT/s2.jsonl | jq -r 'select(.type=="user")|.message.content[]?|select(.type=="tool_result")|.content' | cut -c1-120
kill $SRV
