#!/usr/bin/env python3
"""Drive the Cardputer apps headlessly by typing into a uinput keyboard.

Needs root for /dev/uinput.  Prints the event node it created so the app under
test can be pointed at it with CZ_KBD.
"""
import sys, time, json, os
from evdev import UInput, ecodes as e

CHARS = {c: getattr(e, "KEY_" + c.upper()) for c in "abcdefghijklmnopqrstuvwxyz0123456789"}
CHARS[" "] = e.KEY_SPACE
# Punctuation has to be injected the way the real keypad sends it, off its Sym
# table (linux,keymap1), NOT as the US code for the same character: the card
# has no punctuation at all on its base layer, and e.g. KEY_COMMA is what it
# sends for "&".  Codes from czui._SYM.
CHARS.update({"!": 26, "@": 27, "#": 39, "$": 40, "%": 41, "^": 43, "&": 51,
              "*": 52, "(": 53, ")": 94, "~": 55, "`": 69, "_": 70, "-": 71,
              "+": 72, "=": 73, "[": 74, "]": 75, "{": 76, "}": 77, ";": 79,
              ":": 80, "'": 81, '"': 82, "<": 83, ">": 85, "\\": 86, "|": 89,
              ",": 90, ".": 91, "/": 92, "?": 93})
NAMED = {"ENTER": e.KEY_ENTER, "TAB": e.KEY_TAB, "ESC": e.KEY_ESC, "F1": e.KEY_F1,
         "F2": e.KEY_F2, "UP": e.KEY_UP, "DOWN": e.KEY_DOWN, "BACKSPACE": e.KEY_BACKSPACE,
         "END": e.KEY_END, "HOME": e.KEY_HOME,
         "RIGHT": e.KEY_RIGHT, "LEFT": e.KEY_LEFT, "F5": e.KEY_F5,
         # The card's own keypad has these on its Fn layer (Fn+A / Fn+S / Fn+D)
         # and reports them as the standard media codes.
         "MUTE": e.KEY_MUTE, "VOLUP": e.KEY_VOLUMEUP, "VOLDOWN": e.KEY_VOLUMEDOWN}

caps = {e.EV_KEY: sorted(set(list(CHARS.values()) + list(NAMED.values()) + [e.KEY_LEFTSHIFT]))}
NAME = os.environ.get("CZ_UINPUT_NAME", "cz-test-kbd")
ui = UInput(caps, name=NAME, version=1)
time.sleep(0.6)

# The launcher picks its keypad by device NAME, so a test run may have to call
# itself tca8418c. That makes two devices with one name, so take the highest
# numbered one - ours was created a moment ago.
cands = []
for d in os.listdir("/sys/class/input"):
    if not d.startswith("event"):
        continue
    try:
        with open(f"/sys/class/input/{d}/device/name") as f:
            if f.read().strip() == NAME:
                cands.append(d)
    except OSError:
        pass
node = "/dev/input/" + max(cands, key=lambda d: int(d[5:])) if cands else ""
print(json.dumps({"node": node}), flush=True)

def tap(code, hold=0.03):
    ui.write(e.EV_KEY, code, 1); ui.syn(); time.sleep(hold)
    ui.write(e.EV_KEY, code, 0); ui.syn(); time.sleep(0.02)

for arg in sys.argv[1:]:
    if arg.startswith("HOLD:"):          # HOLD:TAB:3.5
        _, key, secs = arg.split(":")
        code = NAMED[key]
        ui.write(e.EV_KEY, code, 1); ui.syn()
        time.sleep(float(secs))
        ui.write(e.EV_KEY, code, 0); ui.syn()
    elif arg.startswith("SLEEP:"):
        time.sleep(float(arg.split(":", 1)[1]))
    elif arg in NAMED:
        tap(NAMED[arg])
    else:
        for ch in arg:
            if ch.isupper() and ch.lower() in CHARS:
                ui.write(e.EV_KEY, e.KEY_LEFTSHIFT, 1); ui.syn()
                tap(CHARS[ch.lower()])
                ui.write(e.EV_KEY, e.KEY_LEFTSHIFT, 0); ui.syn()
            elif ch in CHARS:
                tap(CHARS[ch])
            time.sleep(0.01)
ui.close()
