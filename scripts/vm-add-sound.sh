#!/usr/bin/env bash
# Give the (shut off) VM an Intel HD Audio sound card, for testing DiShus's PC sound. Its
# output goes nowhere (audio type 'none'): nothing reaches the host's speakers. The "clean"
# snapshot has no sound card, so vm-reset.sh removes it again; run this after a reset (with
# the VM shut off), then start the VM.
. "$(dirname "$0")/vm-common.sh"
[[ $(virsh domstate "$VM") == "shut off" ]] || { echo "shut the VM down first" >&2; exit 1; }
xml=$(virsh dumpxml --inactive "$VM")
if grep -q '<sound ' <<<"$xml"; then echo "already has a sound card"; exit 0; fi
tmp=$(mktemp)
trap 'rm -f "$tmp"' EXIT
sed "s|^\( *\)<audio id='1' type='none'/>|\1<sound model='ich9'><audio id='1'/></sound>\n&|" <<<"$xml" >"$tmp"
grep -q '<sound ' "$tmp" || { echo "no <audio id='1' type='none'/> in the VM definition" >&2; exit 1; }
virsh define "$tmp" >/dev/null && echo "sound card added"
