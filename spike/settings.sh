#!/bin/sh
# usage: settings.sh PORT TIMEOUT  -> settings JSON with PermissionRequest + PreToolUse http hooks
cat <<J
{"hooks":{
 "PermissionRequest":[{"hooks":[{"type":"http","url":"http://127.0.0.1:$1/perm","timeout":$2}]}],
 "PreToolUse":[{"hooks":[{"type":"http","url":"http://127.0.0.1:$1/pre","timeout":$2}]}]}}
J
