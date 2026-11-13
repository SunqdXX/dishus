#!/usr/bin/env bash
# Screenshot the VM's screen from the host (nothing runs in the guest). Prints the PNG path.
. "$(dirname "$0")/vm-common.sh"
f="$DIR/screenshots/$(date +%Y%m%d-%H%M%S)"
virsh screenshot "$VM" "$f.ppm" >/dev/null
magick "$f.ppm" "$f.png" && rm "$f.ppm"
echo "$f.png"
