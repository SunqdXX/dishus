#!/usr/bin/env bash
# DiShus test rig: isolated daemon instances that never touch the user's desktop input.
#   rig.sh start <name> <wayland|x11|testsrc>   start an instance (input is RECORDED, not injected)
#   rig.sh restart <name> <mode>                SIGTERM + immediate relaunch (no wait)
#   rig.sh stop <name>                          stop it (and its Xvfb); state is kept
#   rig.sh clean <name>                         stop and delete its state (refuses if mounts inside)
#   rig.sh cli <name> <dishus args...>          run the CLI against it
#   rig.sh link <name>                          open pairing, print the dishus:// link
#   rig.sh env <name>                           print the env used
set -euo pipefail
REPO=/home/sunqd/WORK/DiShush
BIN=$REPO/target/release
base() { echo "/run/user/1000/dr-$1"; }

envof() {
  local b; b=$(base "$1")
  echo "XDG_CONFIG_HOME=$b/cfg XDG_STATE_HOME=$b/state XDG_RUNTIME_DIR=$b/rt PIPEWIRE_RUNTIME_DIR=/run/user/1000"
}

cmd=${1:?}; name=${2:?}; b=$(base "$name")
# daemons of THIS instance (and their capture workers), found by their runtime dir;
# in x11 mode daemon.pid is dbus-run-session, not the daemon itself
mine() { local p; for p in $(pgrep -u "$(id -u)" -x dishus-daemon); do
           { tr '\0' '\n' <"/proc/$p/environ"; } 2>/dev/null | grep -qx "XDG_RUNTIME_DIR=$b/rt" && echo "$p"
         done; return 0; }
case "$cmd" in
  start|restart)
    # restart: SIGTERM the running daemon and launch the new one at once, without waiting
    # (the new instance must cope with the old one still shutting down)
    mode=${3:?mode}
    mkdir -p "$b"/{cfg,state,rt}; chmod 700 "$b" "$b"/*
    if [ "$cmd" = start ]; then : > "$b/input.log"; chmod 600 "$b/input.log"; fi
    if [ "$cmd" = restart ]; then pids=$(mine); [ -n "$pids" ] && kill $pids; fi
    # an old daemon may still hold daemon.log open: rename, never truncate
    [ -f "$b/daemon.log" ] && mv -f "$b/daemon.log" "$b/daemon.prev.log"
    common=(env $(envof "$name") DISHUS_LOG="${DISHUS_LOG:-info,dishus_daemon=debug}" DISHUS_INPUT="record:$b/input.log")
    case "$mode" in
      wayland)
        nohup "${common[@]}" WAYLAND_DISPLAY=/run/user/1000/wayland-1 "$BIN/dishus-daemon" >"$b/daemon.log" 2>&1 &
        echo $! > "$b/daemon.pid" ;;
      testsrc)
        nohup "${common[@]}" DISHUS_TEST_SOURCE=1 "$BIN/dishus-daemon" >"$b/daemon.log" 2>&1 &
        echo $! > "$b/daemon.pid" ;;
      x11)
        disp=${XDISP:-:99}
        if [ "$cmd" = start ]; then
          nohup Xvfb "$disp" -screen 0 1920x1080x24 -nolisten tcp >"$b/xvfb.log" 2>&1 & echo $! > "$b/xvfb.pid"
          for _ in $(seq 50); do [ -S "/tmp/.X11-unix/X${disp#:}" ] && break; sleep 0.1; done
          # moving content so frames actually change
          nohup env DISPLAY="$disp" gst-launch-1.0 -q videotestsrc pattern=ball is-live=true ! \
            video/x-raw,width=1920,height=1080,framerate=30/1 ! videoconvert ! ximagesink >"$b/content.log" 2>&1 &
          echo $! > "$b/content.pid"
        fi
        # private session bus: no portal reachable -> X11 fallback; no Wayland
        nohup env -u WAYLAND_DISPLAY -u HYPRLAND_INSTANCE_SIGNATURE -u XDG_CURRENT_DESKTOP \
          DISPLAY="$disp" XDG_SESSION_TYPE=x11 "${common[@]}" \
          dbus-run-session --config-file="$(dirname "$0")/session-noactivation.conf" -- \
          "$BIN/dishus-daemon" >"$b/daemon.log" 2>&1 &
        echo $! > "$b/daemon.pid" ;;
      *) echo "mode?"; exit 2 ;;
    esac
    # ready = the NEW instance logged its id (it can't get there while the old one lives)
    # and its control socket answers
    for _ in $(seq 200); do
      grep -q "PC id" "$b/daemon.log" 2>/dev/null && env $(envof "$name") "$BIN/dishus" status >/dev/null 2>&1 && break
      sleep 0.1
    done
    env $(envof "$name") "$BIN/dishus" status >/dev/null 2>&1 && grep -q "PC id" "$b/daemon.log" \
      || { echo "daemon failed:"; tail -20 "$b/daemon.log"; exit 1; }
    echo "${cmd}ed $name ($mode)" ;;
  stop)
    pids=$(mine); [ -n "$pids" ] && kill $pids 2>/dev/null
    for _ in $(seq 100); do [ -z "$(mine)" ] && break; sleep 0.1; done
    pids=$(mine); [ -n "$pids" ] && { echo "daemon did not exit in 10 s; SIGKILL $pids"; kill -9 $pids; }
    for f in daemon content xvfb; do
      [ -f "$b/$f.pid" ] && { kill "$(cat "$b/$f.pid")" 2>/dev/null || true; rm -f "$b/$f.pid"; }
    done
    echo "stopped $name" ;;
  pids) mine ;;
  clean)
    "$0" stop "$name" >/dev/null
    # Never delete through a mount (a FUSE portal view maps to the user's real files).
    if [ -d "$b" ]; then
      if findmnt -rn -o TARGET | grep -q "^$b"; then
        echo "REFUSING to remove $b: something is mounted inside it:"; findmnt -rn -o TARGET | grep "^$b"
        exit 1
      fi
      rm -rf --one-file-system "$b"
    fi
    echo "cleaned $name" ;;
  cli)
    shift 2; env $(envof "$name") "$BIN/dishus" "$@" ;;
  link)
    out="$b/pair.out"; : > "$out"
    nohup env $(envof "$name") "$BIN/dishus" pair --skip-capture-setup >"$out" 2>&1 &
    echo $! > "$b/pair.pid"
    for _ in $(seq 50); do grep -qo 'dishus://[^ ]*' "$out" && break; sleep 0.1; done
    grep -o 'dishus://[^ ]*' "$out" | head -1 ;;
  env) envof "$name" ;;
  *) echo "usage"; exit 2 ;;
esac
