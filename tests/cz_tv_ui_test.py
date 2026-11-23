#!/usr/bin/env python3
"""Drive the real cz-tv binary headlessly, on any Linux box, as a NON-root user.

The card is not always on the desk, and `tests/cz_tv_hw_test.sh` needs both the
hardware and root (for /dev/uinput). This runs the SAME binary and the same
drawing and key-handling code against two stand-ins for the two device nodes,
chosen because each is byte-for-byte the interface the app already uses:

  * the framebuffer is an ordinary 108800-byte FILE, pointed at with the app's
    own APPLAUNCH_LINUX_FBDEV_DEVICE. The ioctls fail on a regular file, so
    fb.rs falls back to exactly the panel's geometry - 320x170, RGB565, 640
    byte stride - and mmaps it. What the app paints lands in the file.
  * the keypad is a FIFO, pointed at with the app's own CZ_KBD. kbd.rs polls
    and reads 24-byte input_event records off it, which is all a real evdev
    node ever gives it, so the decode path under test is the real one.

HOME is a sandbox directory, so `--add` and config writes cannot touch the
running user's own channels.

  tests/cz_tv_ui_test.py --out /tmp/ui SLEEP:1 GRAB:home ENTER GRAB:all ESC ESC

--seed <dir> copies a directory over the sandbox HOME first, which is how a
playlist gets somewhere the Add screen will find it.

Vocabulary: GRAB:<name> (screenshot), SLEEP:<secs>, ENTER/ESC/TAB/UP/DOWN/
LEFT/RIGHT/BKSP/PGUP/PGDN/HOME/END, and any other word is typed character by
character. Writes <name>.png and <name>.txt (the glyphs, read back out of the
framebuffer) per grab, plus a montage. Exit code is the app's.
"""
import os, shutil, struct, subprocess, sys, time
from pathlib import Path

W, H, STRIDE, BPP = 320, 170, 640, 2
FB_BYTES = STRIDE * H
CW, CH = 7, 14

# The codes the card's own keypad sends, from kbd.rs. Punctuation is on its Sym
# layer and the codes have nothing to do with a US layout (Sym+3 is '#' and
# sends 39, which US reads as ';'), so it is injected the way the card sends it.
CHARS = {c: 2 + i for i, c in enumerate("1234567890")}
CHARS.update({c: 16 + i for i, c in enumerate("qwertyuiop")})
CHARS.update({c: 30 + i for i, c in enumerate("asdfghjkl")})
CHARS.update({c: 44 + i for i, c in enumerate("zxcvbnm")})
CHARS.update({" ": 57, "!": 26, "@": 27, "#": 39, "$": 40, "%": 41, "^": 43,
              "&": 51, "*": 52, "(": 53, ")": 94, "~": 55, "`": 69, "_": 70,
              "-": 71, "+": 72, "=": 73, "[": 74, "]": 75, "{": 76, "}": 77,
              ";": 79, ":": 80, "'": 81, '"': 82, "<": 83, ">": 85, "\\": 86,
              "|": 89, ",": 90, ".": 91, "/": 92, "?": 93})
NAMED = {"ESC": 1, "BKSP": 14, "BACKSPACE": 14, "TAB": 15, "ENTER": 28,
         "UP": 103, "DOWN": 108, "LEFT": 105, "RIGHT": 106, "PGUP": 104,
         "PGDN": 109, "HOME": 102, "END": 107}
EV_KEY = 0x01


def rgb565_to_png(raw: bytes, path: Path):
    from PIL import Image
    img = Image.new("RGB", (W, H))
    px = img.load()
    for y in range(H):
        row = raw[y * STRIDE:y * STRIDE + W * BPP]
        for x in range(W):
            v = row[x * 2] | (row[x * 2 + 1] << 8)
            r, g, b = (v >> 11) & 0x1F, (v >> 5) & 0x3F, v & 0x1F
            px[x, y] = (r * 255 // 31, g * 255 // 63, b * 255 // 31)
    img.save(path)
    return img


class Kbd:
    """The writer end of the FIFO. Opening it unblocks the app's own open()."""

    def __init__(self, path):
        self.fd = os.open(path, os.O_WRONLY)

    def _ev(self, code, value):
        t = time.time()
        return struct.pack("qqHHi", int(t), int(t % 1 * 1e6), EV_KEY, code, value)

    def tap(self, code):
        os.write(self.fd, self._ev(code, 1) + self._ev(code, 0))
        time.sleep(0.06)

    def send(self, word):
        if word in NAMED:
            self.tap(NAMED[word])
            return
        for ch in word:
            if ch not in CHARS:
                raise SystemExit(f"cannot type {ch!r} on the card's keypad")
            self.tap(CHARS[ch])

    def close(self):
        os.close(self.fd)


def main(argv):
    binary = "target/debug/cz-tv"
    out = Path("/tmp/cztv-ui")
    seed = None
    script = []
    it = iter(argv)
    for a in it:
        if a == "--bin":
            binary = next(it)
        elif a == "--out":
            out = Path(next(it))
        elif a == "--seed":
            seed = Path(next(it))
        else:
            script.append(a)
    binary = str(Path(binary).resolve())

    if out.exists():
        shutil.rmtree(out)
    (out / "home").mkdir(parents=True)
    if seed:
        shutil.copytree(seed, out / "home", dirs_exist_ok=True)
    fb = out / "fb.raw"
    fb.write_bytes(b"\0" * FB_BYTES)
    fifo = out / "kbd"
    os.mkfifo(fifo)

    env = dict(os.environ)
    env.update(HOME=str(out / "home"), CZ_KBD=str(fifo),
               APPLAUNCH_LINUX_FBDEV_DEVICE=str(fb))
    env.pop("XDG_RUNTIME_DIR", None)      # no pactl, no player, no desktop
    log = open(out / "app.log", "wb")
    app = subprocess.Popen([binary], env=env, stdout=log, stderr=log,
                           stdin=subprocess.DEVNULL)
    kbd = Kbd(fifo)                        # blocks until the app opens the read end

    grabs = []
    try:
        for step in script:
            if app.poll() is not None:
                print(f"app exited early (rc {app.returncode}) before {step}")
                break
            if step.startswith("SLEEP:"):
                time.sleep(float(step.split(":", 1)[1]))
            elif step.startswith("GRAB:"):
                name = step.split(":", 1)[1]
                raw = fb.read_bytes()
                img = rgb565_to_png(raw, out / f"{name}.png")
                (out / f"{name}.txt").write_text(read_text(img))
                grabs.append(name)
                print(f"grab {name}")
            else:
                kbd.send(step)
    finally:
        kbd.close()
        time.sleep(0.3)
        if app.poll() is None:
            app.terminate()
            try:
                app.wait(timeout=5)
            except subprocess.TimeoutExpired:
                app.kill()
        log.close()

    if grabs:
        montage(out, grabs)
    print(f"--- app.log ---\n{(out / 'app.log').read_text()}", end="")
    print(f"screens: {out}")
    return 0


def read_text(img):
    """Read the glyphs back out of the picture.

    The 7x14 font is 1bpp and every row of text sits on a CH boundary, so a
    cell is either background or has ink in it. This does not identify the
    letter - it reports WHERE text is, which is what an automated check can
    assert on without a human looking. The PNG is still the evidence.
    """
    px = img.load()
    lines = []
    for row in range(H // CH):
        cells = ""
        for col in range(W // CW):
            ink = 0
            bg = px[col * CW, row * CH + CH - 1]
            for y in range(row * CH, row * CH + CH):
                for x in range(col * CW, col * CW + CW):
                    if px[x, y] != bg:
                        ink += 1
            cells += "#" if ink > 2 else " "
        lines.append(cells.rstrip())
    return "\n".join(lines) + "\n"


def montage(out: Path, names):
    from PIL import Image
    imgs = [Image.open(out / f"{n}.png") for n in names]
    sheet = Image.new("RGB", (W, (H + 4) * len(imgs)), (40, 40, 40))
    for i, im in enumerate(imgs):
        sheet.paste(im, (0, i * (H + 4)))
    sheet.save(out / "montage.png")


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
