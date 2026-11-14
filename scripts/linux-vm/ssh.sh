#!/usr/bin/env bash
# Run a command in the Arch test VM (or open a shell): scripts/linux-vm/ssh.sh [command...]
. "$(dirname "$0")/env.sh"
. "$(dirname "$0")/../vm-common.sh"
vm_ssh "$@"
