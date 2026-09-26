#!/bin/sh
# Part of the APK.audio project — http://APK.audio — made by Anthony Kuzub
# MIT Licence. Free, for everyone, for ever. Full text in LICENSE at the root.
#
# aes67-loopback — make sure the host has the virtual sound card that puts the
# AES67 network into the desktop's sound settings. Runs ONCE, before
# Plugin-AES67, as the one-shot container Plugin-AES67-Loopback.
#
# WHAT IT LOADS: the kernel's own `snd-aloop` driver, which creates a card
# (id AES67 here) with two linked sides:
#     hw:CARD=AES67,DEV=0   ← PipeWire takes this side the moment it appears
#     hw:CARD=AES67,DEV=1   ← Plugin-AES67 opens the side that is left
# Audio played into one side comes out of the other side's capture. So the
# plugin's receivers PLAY into DEV=1 and your apps RECORD it from DEV=0; your
# apps PLAY into DEV=0 and the plugin's senders CAPTURE it from DEV=1.
#
# WHY A SEPARATE CONTAINER: loading a kernel module needs CAP_SYS_MODULE and
# root. Plugin-AES67 has neither and must not — it is on the network. This
# container has that one capability, the host's /lib/modules read-only, no
# network, and it exits in under a second.
#
# NEVER FAILS THE LAUNCH: every path exits 0. A host that cannot or will not
# load the module still gets a working plugin on a real sound card, and this
# log says why the loopback is missing.
#
#   APK_AES67_LOOPBACK             on (default) | off
#   APK_AES67_LOOPBACK_ID          card id, default AES67
#   APK_AES67_LOOPBACK_SUBSTREAMS  substreams per side, default 1

set -u
MODE="${APK_AES67_LOOPBACK:-on}"
ID="${APK_AES67_LOOPBACK_ID:-AES67}"
SUBS="${APK_AES67_LOOPBACK_SUBSTREAMS:-1}"

say() { echo "🔁 [aes67-loopback] $*"; }

# The card list from /sys, NOT /proc/asound: Docker masks /proc/asound in
# every unprivileged container, so a grep there always says "no card".
# Prints `cardN: <id>` for the card whose id is $1.
card_line() {
    for c in /sys/class/sound/card*; do
        [ -r "$c/id" ] && [ "$(cat "$c/id")" = "$1" ] && echo "${c##*/} [$1]" && return 0
    done
    return 1
}

case "$MODE" in
    off|0|false|no)
        say "disabled (APK_AES67_LOOPBACK=$MODE) — Plugin-AES67 uses the cards it is given"
        exit 0 ;;
esac

if card_line "$ID" >/dev/null; then
    say "card $ID is already there: $(card_line "$ID" | sed 's/^ *//')"
    say "desktop (PipeWire) side: hw:CARD=$ID,DEV=0 · plugin side: hw:CARD=$ID,DEV=1 — pick it on /config"
    exit 0
fi

if [ -d /sys/module/snd_aloop ]; then
    # Loaded by somebody else, with other options (a /etc/modprobe.d entry,
    # a hand modprobe). Unloading it could cut off whoever is using it.
    say "snd-aloop is already loaded, but not as card $ID — leaving it as it is:"
    for c in /sys/class/sound/card*; do [ -r "$c/id" ] && echo "    ${c##*/}: $(cat "$c/id")"; done
    say "pick hw:CARD=Loopback,DEV=1 on /config, or set APK_AES67_LOOPBACK_ID to its id"
    exit 0
fi

if modprobe snd-aloop id="$ID" pcm_substreams="$SUBS" enable=1 2>/tmp/modprobe.err; then
    # The card registers a moment after modprobe returns.
    for _ in 1 2 3 4 5 6 7 8 9 10; do card_line "$ID" >/dev/null && break; sleep 0.2; done
    say "loaded snd-aloop: $(card_line "$ID" | sed 's/^ *//')"
    say "desktop (PipeWire) side: hw:CARD=$ID,DEV=0 · plugin side: hw:CARD=$ID,DEV=1 — pick it on /config"
else
    say "could NOT load snd-aloop: $(cat /tmp/modprobe.err 2>/dev/null)"
    say "on the host, once:  sudo modprobe snd-aloop id=$ID pcm_substreams=$SUBS"
    say "and to keep it across reboots:"
    say "  echo snd-aloop | sudo tee /etc/modules-load.d/aes67.conf"
    say "  echo 'options snd-aloop id=$ID pcm_substreams=$SUBS' | sudo tee /etc/modprobe.d/aes67.conf"
fi
exit 0
