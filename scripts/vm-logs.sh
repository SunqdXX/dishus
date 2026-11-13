#!/usr/bin/env bash
# Fetch DiShus's logs and C:\dishus\*.log from the VM into $DIR/logs/<time>/ and print the tail.
. "$(dirname "$0")/vm-common.sh"
out="$DIR/logs/$(date +%Y%m%d-%H%M%S)"
mkdir -p "$out"
vm_scp -r 'dishus@127.0.0.1:C:/Users/dishus/AppData/Local/dishus/logs' "$out/" 2>/dev/null || echo "(no DiShus logs yet)"
vm_scp 'dishus@127.0.0.1:C:/dishus/*.log' "$out/" 2>/dev/null || true
echo "$out"
find "$out" -type f | while read -r f; do echo "== $f"; tail -n 20 "$f"; done
