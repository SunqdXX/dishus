#!/usr/bin/env bash
# Run once after vm-create.sh, when the desktop is up (scripts/vm-ssh.sh hostname works):
# shut down, remove the install CDs, take the "clean" snapshot that vm-reset.sh returns to.
. "$(dirname "$0")/vm-common.sh"
vm_ssh 'Stop-Computer -Force' >/dev/null 2>&1 || true
for _ in $(seq 60); do [[ $(virsh domstate "$VM") == "shut off" ]] && break; sleep 3; done
[[ $(virsh domstate "$VM") == "shut off" ]] || { echo "VM didn't shut down" >&2; exit 1; }
# The CDs aren't needed any more, and a snapshot would put an overlay on each of them.
for t in $(virsh domblklist "$VM" --details | awk '$2 == "cdrom" {print $3}'); do
  virt-xml "$VM" --remove-device --disk "target=$t" >/dev/null
done
rm -f "$DIR/config/config.iso"   # has the password in plain text
virsh snapshot-create-as "$VM" clean --description "Fresh Windows: virtio tools, OpenSSH, auto-login. No DiShus." \
  --disk-only --atomic --diskspec "sda,snapshot=external,file=$DIR/disk/win11.clean"
virsh domblklist "$VM"
