#!/usr/bin/env bash
# Create the Arch Linux test VM for login-screen mode: SDDM with an X11 greeter, like the
# owner's PC. User-mode libvirt (qemu:///session), files under ~/VMs/arch-sddm, SSH on
# 127.0.0.1:2223 as user `dishus` (key in ~/VMs/arch-sddm/ssh/). Run once; then
# scripts/linux-vm/ssh.sh. Delete with: virsh -c qemu:///session undefine dishus-arch
# (then remove ~/VMs/arch-sddm). Uses 2 GB RAM and 2 cores while running.
set -euo pipefail
. "$(dirname "$0")/env.sh"
. "$(dirname "$0")/../vm-common.sh"

mkdir -p "$DIR"/{disk,ssh,config,screenshots}
img="$DIR/disk/base.qcow2"
if [ ! -f "$img" ]; then
  base=https://geo.mirror.pkgbuild.com/images/latest
  curl -fL --retry 3 -o "$img.part" "$base/Arch-Linux-x86_64-cloudimg.qcow2"
  curl -fsSL -o "$DIR/disk/base.SHA256" "$base/Arch-Linux-x86_64-cloudimg.qcow2.SHA256"
  want=$(awk '{print $1}' "$DIR/disk/base.SHA256")
  got=$(sha256sum "$img.part" | awk '{print $1}')
  [ "$want" = "$got" ] || { echo "checksum mismatch" >&2; exit 1; }
  mv "$img.part" "$img"
fi
[ -f "$DIR/ssh/id_ed25519" ] || ssh-keygen -q -t ed25519 -N '' -C dishus-vm -f "$DIR/ssh/id_ed25519"

# The VM disk is a copy-on-write layer over the downloaded image.
qemu-img create -q -f qcow2 -b "$img" -F qcow2 "$DIR/disk/vm.qcow2" 20G

cat > "$DIR/config/user-data" <<EOF
#cloud-config
hostname: dishus-arch
users:
  - name: dishus
    groups: [wheel]
    sudo: "ALL=(ALL) NOPASSWD:ALL"
    shell: /bin/bash
    lock_passwd: false
    # login password at the greeter: "test" (a throwaway VM, reachable only from this PC)
    plain_text_passwd: test
    ssh_authorized_keys:
      - $(cat "$DIR/ssh/id_ed25519.pub")
package_update: false
runcmd:
  - [pacman-key, --init]
  - [pacman-key, --populate, archlinux]
  - [pacman, -Syu, --noconfirm, --needed, sddm, xorg-server, xorg-xhost, xorg-xwininfo,
     openbox, xterm, gstreamer, gst-plugins-base, gst-plugins-good, gst-plugins-ugly,
     gst-plugins-bad, x264, imagemagick, acl]
  - [systemctl, enable, --now, sddm]
  - [touch, /var/lib/dishus-vm-ready]
EOF
printf 'instance-id: dishus-arch-1\nlocal-hostname: dishus-arch\n' > "$DIR/config/meta-data"
xorriso -as mkisofs -quiet -o "$DIR/config/cidata.iso" -V cidata -J -r \
  "$DIR/config/user-data" "$DIR/config/meta-data"

virt-install --name "$VM" --osinfo archlinux \
  --memory 2048 --vcpus 2 --cpu host-passthrough \
  --disk "path=$DIR/disk/vm.qcow2,format=qcow2,bus=virtio" \
  --disk "path=$DIR/config/cidata.iso,device=cdrom" \
  --network "passt,model=virtio,portForward0.proto=tcp,portForward0.address=127.0.0.1,portForward0.range0.start=$PORT,portForward0.range0.to=22" \
  --video virtio --graphics vnc,listen=127.0.0.1 \
  --import --noautoconsole
echo "created $VM; first boot installs SDDM (a few minutes). Check: scripts/linux-vm/ssh.sh test -f /var/lib/dishus-vm-ready"
