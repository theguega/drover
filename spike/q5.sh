#!/bin/sh
# Q5: interactive claude in a tmux pane with --settings. Hook answers deny, so no dialog needs a human.
D=$(cd "$(dirname "$0")" && pwd); OUT=${OUT:-/tmp/spike-q5}; PORT=${PORT:-8797}
mkdir -p $OUT/work; rm -f $OUT/hook.log
LOG=$OUT/hook.log MODE=deny python3 $D/hook_server.py $PORT & SRV=$!; sleep 1
"$D/settings.sh" $PORT 30 > $OUT/settings.json
tmux kill-session -t spike5 2>/dev/null
tmux new-session -d -s spike5 -x 150 -y 50 -c $OUT/work "claude --model haiku --permission-mode default --settings $OUT/settings.json; echo exit=\$? ; sleep 60"
sleep 8; tmux send-keys -t spike5 Down Enter; sleep 4   # trust prompt defaults to "No, exit"
tmux send-keys -t spike5 'Run this exact bash command with the Bash tool: touch spike-file.txt. If denied, quote the reason.'; sleep 2
tmux send-keys -t spike5 Enter; sleep 30; tmux capture-pane -t spike5 -p > $OUT/pane.txt; tmux kill-session -t spike5; kill $SRV
jq -c 'if .event then {path,event,tool:.body.tool_name} else . end' $OUT/hook.log; grep -n -i -E 'REASON|denied|touch' $OUT/pane.txt; ls $OUT/work
