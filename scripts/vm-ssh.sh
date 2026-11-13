#!/usr/bin/env bash
# Run a command in the VM over SSH (PowerShell), or open a shell with no arguments.
# This is a separate, non-interactive session: it can't see the desktop. Use vm-run.sh for that.
. "$(dirname "$0")/vm-common.sh"
vm_ssh "$@"
