#!/usr/bin/env bash
# Copy a Windows build into the VM as C:\dishus\<name>. Default: the newest dist/*windows*.exe.
. "$(dirname "$0")/vm-common.sh"
f=${1:-$(ls -t "$(dirname "$0")"/../dist/*windows*.exe 2>/dev/null | head -1)}
[[ -f $f ]] || { echo "usage: $0 [file]  (no Windows build found in dist/)" >&2; exit 1; }
vm_ssh 'New-Item -ItemType Directory -Force C:\dishus | Out-Null'
vm_scp "$f" "dishus@127.0.0.1:C:/dishus/"
echo "C:\\dishus\\$(basename "$f")"
