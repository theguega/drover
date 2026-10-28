#!/bin/sh
# beforeShellExecution hook: log stdin with timestamps, hold HOLD seconds, answer deny.
LOG=${LOG:-/tmp/spike-q6/hook.log}
echo "start $(date +%s.%N) $(cat)" >> $LOG
sleep ${HOLD:-3}
echo "end   $(date +%s.%N)" >> $LOG
echo '{"permission":"deny","user_message":"blocked by drover spike","agent_message":"REASON-XYZZY: blocked by drover spike"}'
