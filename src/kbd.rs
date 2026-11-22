// Raw /dev/input reader for the tca8418c keypad. The driver emits ordinary PC
// keycodes, so a plain decode is correct. 24-byte input_event on 64-bit:
//   sec(i64) usec(i64) type(u16) code(u16) value(i32)

use std::fs::File;
use std::io::Read;
use std::os::unix::io::AsRawFd;

pub const KEY_ESC: u16 = 1;
pub const KEY_BACKSPACE: u16 = 14;
pub const KEY_TAB: u16 = 15;
pub const KEY_ENTER: u16 = 28;
pub const KEY_UP: u16 = 103;
pub const KEY_DOWN: u16 = 108;
pub const KEY_LEFT: u16 = 105;
pub const KEY_RIGHT: u16 = 106;
pub const KEY_PAGEUP: u16 = 104;
pub const KEY_PAGEDOWN: u16 = 109;
pub const KEY_HOME: u16 = 102;
pub const KEY_END: u16 = 107;
// The keypad does have a real Fn layer with media keys on it - Fn+A = KEY_MUTE,
// Fn+S = KEY_VOLUMEDOWN, Fn+D = KEY_VOLUMEUP, from the tca8418's linux,keymap2
// in the device tree - but this app does not read them: labwc binds them for
// the whole session already. See the note at the bottom of audio.rs.

const EV_KEY: u16 = 0x01;
const EV_SIZE: usize = 24;

pub struct Keyboard {
    file: File,
}

pub enum Ev {
    Down(u16),
    Char(char),
}

// Printable keys, used for type-to-search. Two tables, because the keypad has
// two: the base one, and the Sym one (linux,keymap1 in the overlay) that the
// Sym key switches the whole keypad over to. A Sym code has nothing to do with
// the symbol printed on the key - Sym+3 prints '#' and sends 39, which a US
// layout reads as ';' - and no Sym code is ever sent by the base or Fn table,
// so one lookup over both is unambiguous.
fn base_char(code: u16) -> Option<char> {
    Some(match code {
        2 => '1', 3 => '2', 4 => '3', 5 => '4', 6 => '5', 7 => '6', 8 => '7',
        9 => '8', 10 => '9', 11 => '0', 12 => '-', 16 => 'q', 17 => 'w',
        18 => 'e', 19 => 'r', 20 => 't', 21 => 'y', 22 => 'u', 23 => 'i',
        24 => 'o', 25 => 'p', 30 => 'a', 31 => 's', 32 => 'd', 33 => 'f',
        34 => 'g', 35 => 'h', 36 => 'j', 37 => 'k', 38 => 'l', 44 => 'z',
        45 => 'x', 46 => 'c', 47 => 'v', 48 => 'b', 49 => 'n', 50 => 'm',
        57 => ' ',
        // Sym layer.
        26 => '!', 27 => '@', 39 => '#', 40 => '$', 41 => '%', 43 => '^',
        51 => '&', 52 => '*', 53 => '(', 94 => ')',
        55 => '~', 69 => '`', 70 => '_', 71 => '-', 72 => '+', 73 => '=',
        74 => '[', 75 => ']', 76 => '{', 77 => '}', 78 => '+',
        79 => ';', 80 => ':', 81 => '\'', 82 => '"',
        83 => '<', 85 => '>', 86 => '\\', 89 => '|', 90 => ',', 91 => '.',
        92 => '/', 93 => '?',
        _ => return None,
    })
}

impl Keyboard {
    pub fn open() -> std::io::Result<Keyboard> {
        let path = std::env::var("CZ_KBD")
            .unwrap_or_else(|_| "/dev/input/cardputer-zero-internal".to_string());
        let file = File::options().read(true).open(&path)?;
        // Non-blocking so poll() can time out and let the UI redraw.
        unsafe {
            let fd = file.as_raw_fd();
            let fl = libc::fcntl(fd, libc::F_GETFL);
            libc::fcntl(fd, libc::F_SETFL, fl | libc::O_NONBLOCK);
        }
        Ok(Keyboard { file })
    }

    // Block up to `timeout_ms`, then drain and decode available key-down events.
    pub fn poll(&mut self, timeout_ms: i32) -> Vec<Ev> {
        let fd = self.file.as_raw_fd();
        let mut pfd = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
        let r = unsafe { libc::poll(&mut pfd, 1, timeout_ms) };
        let mut out = Vec::new();
        if r <= 0 {
            return out;
        }
        let mut buf = [0u8; EV_SIZE * 64];
        let n = match self.file.read(&mut buf) {
            Ok(n) => n,
            Err(_) => return out,
        };
        let mut off = 0;
        while off + EV_SIZE <= n {
            let etype = u16::from_ne_bytes([buf[off + 16], buf[off + 17]]);
            let code = u16::from_ne_bytes([buf[off + 18], buf[off + 19]]);
            let value =
                i32::from_ne_bytes([buf[off + 20], buf[off + 21], buf[off + 22], buf[off + 23]]);
            off += EV_SIZE;
            if etype != EV_KEY {
                continue;
            }
            if value == 1 || value == 2 {
                // press or autorepeat
                out.push(Ev::Down(code));
                if let Some(c) = base_char(code) {
                    out.push(Ev::Char(c));
                }
            }
        }
        out
    }
}
