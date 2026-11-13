#!/usr/bin/env bash
# Throw away every change since the "clean" snapshot and boot fresh (DiShus not installed).
. "$(dirname "$0")/vm-common.sh"
if [[ $(virsh domstate "$VM") != "shut off" ]]; then
  vm_ssh 'Stop-Computer -Force' >/dev/null 2>&1 || virsh shutdown "$VM" >/dev/null
  for _ in $(seq 60); do [[ $(virsh domstate "$VM") == "shut off" ]] && break; sleep 3; done
  [[ $(virsh domstate "$VM") == "shut off" ]] || virsh destroy "$VM" >/dev/null   # it's being thrown away anyway
fi
virsh snapshot-revert "$VM" clean
virsh start "$VM" >/dev/null
# Ready = the user's desktop is up (auto-login), not just SSH.
for _ in $(seq 60); do
  [[ -n $(vm_ssh '(Get-Process explorer -ErrorAction SilentlyContinue).Id' 2>/dev/null) ]] && { echo "ready"; exit 0; }
  sleep 10
done
echo "desktop didn't come up in 10 minutes; see scripts/vm-shot.sh" >&2; exit 1
