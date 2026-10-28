#!/bin/sh
# usage: show.sh OUT  -> hook log (short) and the tool/result/text lines of the stream
jq -c 'if .event then {t,path,event,tool:.body.tool_name,input:.body.tool_input,sid:.body.session_id} else . end' $1/hook.log
jq -c 'select(.type=="assistant" or .type=="user" or .type=="result") | {type, c:(.message.content // .result)}' $1/stream.jsonl | cut -c1-400
ls $1/work
