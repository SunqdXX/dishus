#!/usr/bin/env bash
# Copy files into the Arch test VM: scripts/linux-vm/push.sh <local>... <remote-dir>
. "$(dirname "$0")/env.sh"
. "$(dirname "$0")/../vm-common.sh"
args=("$@"); dest=${args[-1]}; unset 'args[-1]'
scp "${SSH_OPTS[@]}" -P "$PORT" "${args[@]}" "dishus@127.0.0.1:$dest"
