#!/usr/bin/env bash
# Screenshot of the Arch test VM's screen (the SDDM greeter); prints the PNG path.
. "$(dirname "$0")/env.sh"
exec "$(dirname "$0")/../vm-shot.sh"
