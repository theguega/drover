#!/bin/sh
# Q2 long hold: server answers allow after $1 s, hook timeout $2 s. Prints elapsed and whether the file got created.
D=$(cd "$(dirname "$0")" && pwd); PORT=$3 OUT=/tmp/spike-long-$1 $D/q1.sh sleep:$1 $2
echo "hold=$1 timeout=$2 elapsed=$(( $(cat /tmp/spike-long-$1/end)-$(cat /tmp/spike-long-$1/start) ))s file=$(ls /tmp/spike-long-$1/work)"
