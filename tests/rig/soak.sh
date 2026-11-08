#!/usr/bin/env bash
# Run every PC-side scenario N times against an isolated rig daemon; asserts on the recorded input.
# usage: soak.sh <rig-name> <mode> [N]
set -uo pipefail
R=${SOAK_STATE:-/run/user/1000/dishus-soak}; mkdir -p "$R"; RIG=$(dirname "$0")/rig.sh
NAME=${1:?}; MODE=${2:?}; N=${3:-10}
SOAK=/home/sunqd/WORK/DiShush/target/release/examples/soak
B=/run/user/1000/dr-$NAME; LOG=$B/input.log
export DISHUS_CLIENT_DIR=$R/soak-$NAME DISHUS_CLIENT_PASSWORD=soak-test-password
PASS=0; FAIL=0; declare -A FAILS
ok()   { PASS=$((PASS+1)); }
bad()  { FAIL=$((FAIL+1)); FAILS[$1]=$(( ${FAILS[$1]:-0} + 1 )); echo "  FAIL [$1] $2"; }
mark() { wc -l < "$LOG"; }
since(){ tail -n +"$(( $1 + 1 ))" "$LOG"; }
pair() { local l; l=$($RIG link "$NAME"); $SOAK pair "$l" | grep -q "RESULT paired"; }

$RIG clean "$NAME" || exit 1; rm -rf --one-file-system "$DISHUS_CLIENT_DIR"
$RIG start "$NAME" "$MODE" || exit 1
pair || { echo "initial pairing failed"; exit 1; }

EXPECT_SCRIPT="key 42 1
key 30 1
key 30 0
key 42 0
key 48 1
key 48 0
key 29 1
key 46 1
key 46 0
key 29 0
move 10 -5
move -3 7
scroll 0 240
btn Left 1
btn Left 0
btn Right 1
btn Right 0"

for i in $(seq 1 "$N"); do
  echo "== round $i"
  # 1. connect
  out=$($SOAK try); [[ $out == "RESULT connected"* ]] && ok || bad connect "$out"
  # 2. exact input script
  m=$(mark); out=$($SOAK script); sleep 0.3
  got=$(since "$m")
  [[ $got == "$EXPECT_SCRIPT" ]] && ok || bad script "got: $(echo "$got" | tr '\n' ',')"
  # 3. held key released on Bye
  m=$(mark); $SOAK hold 29 bye >/dev/null; sleep 0.3
  [[ $(since "$m") == $'key 29 1\nkey 29 0' ]] && ok || bad hold-bye "$(since "$m" | tr '\n' ',')"
  # 4. held key released when the phone vanishes (idle timeout 5 s)
  m=$(mark); t=$(date +%s.%N); $SOAK hold 125 drop >/dev/null
  rel=""; for _ in $(seq 1 100); do since "$m" | grep -q "key 125 0" && { rel=$(date +%s.%N); break; }; sleep 0.1; done
  if [[ -n $rel ]]; then ok; echo "  vanished-phone release after $(awk -v a="$rel" -v b="$t" 'BEGIN{printf "%.1f", a-b}') s"; else bad vanish "never released: $(since "$m" | tr '\n' ',')"; fi
  # 5. refused keycode (KEY_POWER) never reaches input, session ends
  m=$(mark); out=$($SOAK badkey 116)
  { [[ $out == *"bye=ProtocolError"* || $out == *"closed"* ]] && ! since "$m" | grep -q "key 116"; } && ok || bad badkey "$out $(since "$m" | tr '\n' ',')"
  # 6. video
  out=$($SOAK video 3); fr=$(grep -oP ' frames=\K[0-9]+' <<<"$out"); gp=$(grep -oP ' gaps=\K[0-9]+' <<<"$out")
  { [[ ${fr:-0} -gt 20 && ${gp:-1} -eq 0 ]]; } && ok || bad video "$out"
  # 7. renew from inside the session
  out=$($SOAK renew 7); [[ $out == "RESULT renewed"* ]] && $RIG cli "$NAME" list | grep -q " 7d " && ok || bad renew "$out / $($RIG cli "$NAME" list | tail -1)"
  # 8. newer session replaces older
  $SOAK wait 15 > "$B/wait.out" & W=$!; sleep 1.5
  out=$($SOAK try); wait $W; w=$(cat "$B/wait.out")
  [[ $out == "RESULT connected"* && $w == *"bye=Replaced"* ]] && ok || bad replace "new=$out old=$w"
  # 9. kill: cuts session, refuses reconnect AND pairing, survives restart, unlock restores
  $SOAK wait 15 > "$B/wait.out" & W=$!; sleep 1.5
  $RIG cli "$NAME" kill >/dev/null; wait $W; w=$(cat "$B/wait.out")
  r1=$($SOAK try); p=$($RIG cli "$NAME" pair --skip-capture-setup 2>&1 | head -2 | tr '\n' ' ')
  $RIG cli "$NAME" unlock >/dev/null; r2=$($SOAK try)
  [[ $w == *"bye=Killed"* && $r1 == "RESULT refused"*"locked down"* && $p == *lockdown* && $r2 == "RESULT connected"* ]] && ok || bad kill "old=$w after=$r1 pair=$p unlocked=$r2"
  # 10. revoke: cuts session immediately, reconnect refused, re-pair restores
  $SOAK wait 15 > "$B/wait.out" & W=$!; sleep 1.5
  $RIG cli "$NAME" revoke soak >/dev/null; wait $W; w=$(cat "$B/wait.out")
  r1=$($SOAK try)
  [[ $w == *"bye=Revoked"* && $r1 == "RESULT refused"* && $r1 != *"locked down"* && $r1 != *expired* ]] && ok || bad revoke "old=$w after=$r1"
  pair && r2=$($SOAK try) && [[ $r2 == "RESULT connected"* ]] && ok || bad repair "${r2:-pair failed}"
done

# 11. expiry (daemon restart with a past expiry), N times
for i in $(seq 1 "$N"); do
  $RIG stop "$NAME" >/dev/null
  sed -i 's/^expires_at = .*/expires_at = 1000/' "$B/cfg/dishus/devices.toml"
  st=$($RIG start "$NAME" "$MODE" 2>&1) || { bad expiry-start "$st"; continue; }
  r1=$($SOAK try); l=$($RIG cli "$NAME" list | tail -1)
  $RIG cli "$NAME" renew soak --days 3 >/dev/null; r2=$($SOAK try)
  [[ $r1 == "RESULT refused"*"expired"* && $l == *EXPIRED* && $r2 == "RESULT connected"* ]] && ok || bad expiry "expired-try=$r1 list=$l renewed-try=$r2"
done

# 12. quick restart: SIGTERM and relaunch at once while the old daemon is still shutting
#     down (what a careless restart does). New instance must start, keep its UDP port, and the
#     phone must reconnect.
port0=$(cat "$B/state/dishus/udp-port" 2>/dev/null)
for i in $(seq 1 "$N"); do
  st=$($RIG restart "$NAME" "$MODE" 2>&1) || { bad restart "$st"; continue; }
  r=$($SOAK try); p=$(cat "$B/state/dishus/udp-port" 2>/dev/null)
  [[ $r == "RESULT connected"* && -n $port0 && $p == "$port0" ]] && ok || bad restart "try=$r port=$p (was $port0) $(grep -E 'WARN|ERROR' "$B/daemon.log" | tail -2)"
done
n=0; for p in $($RIG pids "$NAME"); do tr '\0' ' ' <"/proc/$p/cmdline" 2>/dev/null | grep -q -- '--capture-worker' || n=$((n+1)); done
[[ $n -eq 1 ]] && ok || bad single-instance "$n daemons running for this rig"

perms=$(stat -c '%a' "$B/cfg/dishus" "$B/cfg/dishus/secret.key" "$B/cfg/dishus/devices.toml" | tr '\n' ' ')
[[ $perms == "700 600 600 " ]] && ok || bad perms "$perms"
# only this rig's processes (other rigs or the real daemon may legitimately run workers)
main=""; workers=0
for p in $($RIG pids "$NAME"); do
  if tr '\0' ' ' <"/proc/$p/cmdline" 2>/dev/null | grep -q -- '--capture-worker'; then workers=$((workers+1)); else main=$p; fi
done
rss=$(ps -o rss= -p "$main" 2>/dev/null)
echo "daemon RSS after soak: $((rss/1024)) MB; capture workers still running: $workers"
[[ $workers -eq 0 ]] && ok || bad worker-leak "$workers workers left"
$RIG stop "$NAME" >/dev/null
echo "TOTAL pass=$PASS fail=$FAIL"; for k in "${!FAILS[@]}"; do echo "  $k: ${FAILS[$k]} failures"; done
