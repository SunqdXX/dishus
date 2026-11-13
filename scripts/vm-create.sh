#!/usr/bin/env bash
# Create the Windows 11 test VM and start its unattended install (about an hour on an HDD).
# Needs in $DIR/iso: win11.iso (a Windows 11 ISO from Microsoft), virtio-win.iso, OpenSSH-Win64.msi. See docs/WINDOWS-TESTING.md.
. "$(dirname "$0")/vm-common.sh"
here=$(cd "$(dirname "$0")" && pwd)
iso=${DISHUS_VM_ISO:-$DIR/iso/win11.iso}
edition=${DISHUS_VM_EDITION:-Windows 11 Pro}
# Microsoft's generic Pro install key: installs Pro without activating it.
key=${DISHUS_VM_KEY-VK7JG-NPHTM-C97JM-9MPGT-3V66T}
for f in "$iso" "$DIR/iso/virtio-win.iso" "$DIR/iso/OpenSSH-Win64.msi"; do
  [[ -f $f ]] || { echo "missing $f" >&2; exit 1; }
done
if virsh dominfo "$VM" >/dev/null 2>&1; then echo "$VM already exists" >&2; exit 1; fi

mkdir -p "$DIR"/{disk,config,ssh,logs,screenshots}
[[ -f $DIR/ssh/id_ed25519 ]] || ssh-keygen -q -t ed25519 -N '' -C dishus-vm -f "$DIR/ssh/id_ed25519"
[[ -f $DIR/config/password ]] || (umask 077; tr -dc 'A-Za-z0-9' </dev/urandom | head -c 20 >"$DIR/config/password")

# The config CD: autounattend.xml (Setup finds it on any CD), the first-logon script, the key.
cd_dir="$DIR/config/cd"
rm -rf "$cd_dir" && mkdir -p "$cd_dir"
# Setup only accepts a UI language the ISO has: read it from the volume id (..._EN-GB_...).
v=$(xorriso -indev "$iso" -toc 2>/dev/null | grep -oE "_[A-Z]{2}-[A-Z]{2}_" | head -1 | tr -d _)
[[ -n $v ]] || { echo "can't tell the ISO's language" >&2; exit 1; }
lang="${v:0:2}"; lang="${lang,,}-${v:3:2}"   # EN-GB -> en-GB
sed -e "s/@PASSWORD@/$(cat "$DIR/config/password")/g" -e "s/@LANG@/$lang/g" \
  -e "s/@EDITION@/$edition/g" -e "s/@KEY@/$key/g" "$here/vm/autounattend.xml" >"$cd_dir/autounattend.xml"
[[ -n $key ]] || sed -i '/<ProductKey>/d' "$cd_dir/autounattend.xml"
cp "$here/vm/setup.ps1" "$DIR/iso/OpenSSH-Win64.msi" "$cd_dir/"
cp "$DIR/ssh/id_ed25519.pub" "$cd_dir/authorized_keys"
xorriso -as mkisofs -quiet -J -R -V DISHUSCFG -o "$DIR/config/config.iso" "$cd_dir"
rm -rf "$cd_dir"   # holds the password in plain text

# SATA disk and e1000e NIC: Windows Setup has drivers for both, so nothing can stall in WinPE.
# The virtio drivers come with the guest tools at first logon.
virt-install --name "$VM" --osinfo win11 \
  --memory 8192 --vcpus 4 --cpu host-passthrough \
  --boot uefi \
  --tpm backend.type=emulator,backend.version=2.0,model=tpm-crb \
  --disk "path=$DIR/disk/win11.qcow2,size=60,format=qcow2,bus=sata,boot.order=2" \
  --disk "path=$iso,device=cdrom,bus=sata,boot.order=1" \
  --disk "path=$DIR/iso/virtio-win.iso,device=cdrom,bus=sata" \
  --disk "path=$DIR/config/config.iso,device=cdrom,bus=sata" \
  --network "passt,model=e1000e,portForward0.proto=tcp,portForward0.address=127.0.0.1,portForward0.range0.start=$PORT,portForward0.range0.to=22" \
  --video virtio --graphics vnc,listen=127.0.0.1 \
  --noautoconsole

# The Windows ISO waits for "Press any key to boot from CD": press F12 for 12 s.
# F12: the prompt takes any key, Setup ignores it (Enter leaked into Setup and hit Cancel).
for _ in $(seq 12); do virsh send-key "$VM" KEY_F12 >/dev/null 2>&1 || true; sleep 1; done
echo "Installing (about an hour). Watch: scripts/vm-shot.sh. When scripts/vm-ssh.sh hostname answers and the desktop is up, run scripts/vm-finish.sh."
