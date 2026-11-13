# Sourced by scripts/vm-*.sh. The Windows test VM runs under the user's own libvirt
# (qemu:///session): no root daemon, SSH reaches it through a port on 127.0.0.1.
set -euo pipefail
VM=${DISHUS_VM_NAME:-dishus-win11}
DIR=${DISHUS_VM_DIR:-$HOME/VMs/windows}
PORT=${DISHUS_VM_SSH_PORT:-2222}
export LIBVIRT_DEFAULT_URI=qemu:///session
SSH_OPTS=(-i "$DIR/ssh/id_ed25519" -o IdentitiesOnly=yes -o ConnectTimeout=15
  -o StrictHostKeyChecking=accept-new -o UserKnownHostsFile="$DIR/ssh/known_hosts")
vm_ssh() { ssh "${SSH_OPTS[@]}" -p "$PORT" dishus@127.0.0.1 "$@"; }
# -O: legacy scp. The SFTP server in Win32-OpenSSH 10.4 (preview) exits as soon as it starts.
vm_scp() { scp -O "${SSH_OPTS[@]}" -P "$PORT" "$@"; }
