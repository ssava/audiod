#!/usr/bin/env bash
# audiod HDA server lifecycle — run as root:
#   sudo scripts/audiod-hda.sh          # (re)start the server
#   sudo scripts/audiod-hda.sh stop     # stop server + give the slot back to the kernel
#   sudo scripts/audiod-hda.sh log      # follow the server log
#
# Clients run as your normal user, e.g.:
#   LD_PRELOAD=./target/release/libaudshim.so mpv --no-video /tmp/tone_a.wav
set -u

cd "$(dirname "$0")/.."
SLOT="${AUDIOD_SLOT:-0000:00:1b.0}"
DRV=/sys/bus/pci/drivers/snd_hda_intel
LOG=/tmp/audiod.log

unbind() {
    if [ -e "$DRV/$SLOT" ]; then
        echo "unbinding snd_hda_intel from $SLOT"
        echo "$SLOT" > "$DRV/unbind" || { echo "unbind failed" >&2; exit 1; }
        sleep 0.3
    else
        echo "slot $SLOT already free of snd_hda_intel"
    fi
}

kill_server() {
    if pgrep -x audiod > /dev/null; then
        pkill -x audiod
        for _ in $(seq 20); do pgrep -x audiod > /dev/null || break; sleep 0.1; done
        pgrep -x audiod > /dev/null && { echo "audiod refuses to die"; exit 1; }
    fi
    rm -f /tmp/audiod.sock
}

case "${1:-start}" in
start)
    kill_server
    unbind
    RUST_LOG="${AUDIOD_RUST_LOG:-debug}" nohup ./target/release/audiod -d "hda:$SLOT" > "$LOG" 2>&1 &
    sleep 1
    if ! pgrep -x audiod > /dev/null; then
        echo "audiod failed to start:" >&2
        tail -20 "$LOG" >&2
        exit 1
    fi
    tail -5 "$LOG"
    echo "--- audiod running (pid $(pgrep -x audiod)), log: $LOG"
    ;;
stop)
    kill_server
    if [ -d "$DRV" ] && ! [ -e "$DRV/$SLOT" ]; then
        echo "rebinding $SLOT to snd_hda_intel (restores normal desktop audio)"
        echo "$SLOT" > "$DRV/bind" 2>/dev/null || echo "rebind failed (ok if driver unloaded)"
    fi
    echo "audiod stopped"
    ;;
log)
    exec tail -n 50 -f "$LOG"
    ;;
*)
    echo "usage: sudo $0 [start|stop|log]" >&2
    exit 2
    ;;
esac
