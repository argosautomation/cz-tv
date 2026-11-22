// Framebuffer text UI for the M5Stack Cardputer Zero.
// 320x170 RGB565 panel on /dev/fb0. No SDL/Qt: we keep an in-RAM RGB565 buffer
// and blit it with one write(). Geometry is read from the fb ioctls, with the
// known-good panel values as a fallback so a published app never dies on a
// surprising fix_screeninfo layout.

use crate::font::{CH, CW, GLYPHS};
use std::fs::File;
use std::os::unix::io::AsRawFd;

// libc types ioctl()'s request as c_int on musl and c_ulong on glibc, so the
// constants use libc::Ioctl - that is what lets `cargo test` run on the x86-64
// host as well as cross-compile for the card.
const FBIOGET_VSCREENINFO: libc::Ioctl = 0x4600;
const FBIOGET_FSCREENINFO: libc::Ioctl = 0x4602;

pub type Color = u16; // RGB565

pub const fn rgb(r: u8, g: u8, b: u8) -> Color {
    (((r as u16) >> 3) << 11) | (((g as u16) >> 2) << 5) | ((b as u16) >> 3)
}

pub const BLACK: Color = rgb(0, 0, 0);
pub const WHITE: Color = rgb(235, 235, 235);
pub const CYAN: Color = rgb(0, 200, 210);
pub const AMBER: Color = rgb(255, 180, 40);
pub const GREY: Color = rgb(120, 120, 120);
pub const BLUE: Color = rgb(20, 40, 120);
pub const RED: Color = rgb(220, 60, 60);

pub struct Screen {
    _file: File,
    map: *mut u8,
    map_len: usize,
    pub w: usize,
    pub h: usize,
    stride: usize, // bytes per line on the device
    buf: Vec<Color>,
    pub cols: usize,
    pub rows: usize,
}

fn rd_u32(b: &[u8], off: usize) -> u32 {
    u32::from_ne_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}

impl Screen {
    pub fn open() -> std::io::Result<Screen> {
        let dev = std::env::var("APPLAUNCH_LINUX_FBDEV_DEVICE")
            .unwrap_or_else(|_| "/dev/fb0".to_string());
        let file = File::options().read(true).write(true).open(&dev)?;
        let fd = file.as_raw_fd();

        // var: xres@0, yres@4, bits_per_pixel@24
        let mut var = [0u8; 256];
        let mut w = 320usize;
        let mut h = 170usize;
        let mut bpp = 16u32;
        unsafe {
            if libc::ioctl(fd, FBIOGET_VSCREENINFO, var.as_mut_ptr()) == 0 {
                let xr = rd_u32(&var, 0) as usize;
                let yr = rd_u32(&var, 4) as usize;
                bpp = rd_u32(&var, 24);
                if xr > 0 && yr > 0 && xr <= 4096 && yr <= 4096 {
                    w = xr;
                    h = yr;
                }
            }
        }
        // fix: smem_len@24, line_length@48
        let mut fix = [0u8; 256];
        let mut stride = w * 2;
        let mut smem_len = h * stride;
        unsafe {
            if libc::ioctl(fd, FBIOGET_FSCREENINFO, fix.as_mut_ptr()) == 0 {
                let sl = rd_u32(&fix, 24) as usize;
                let ll = rd_u32(&fix, 48) as usize;
                if ll >= w * 2 && ll <= w * 8 {
                    stride = ll;
                }
                if sl >= h * stride && sl <= 64 * 1024 * 1024 {
                    smem_len = sl;
                }
            }
        }
        if bpp != 16 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                format!("expected 16bpp RGB565, panel reports {bpp}"),
            ));
        }
        let page = 4096;
        let map_len = (smem_len + page - 1) / page * page;
        let map = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                map_len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd,
                0,
            )
        };
        if map == libc::MAP_FAILED {
            return Err(std::io::Error::last_os_error());
        }
        Ok(Screen {
            _file: file,
            map: map as *mut u8,
            map_len,
            w,
            h,
            stride,
            buf: vec![BLACK; w * h],
            cols: w / CW,
            rows: h / CH,
        })
    }

    pub fn clear(&mut self, c: Color) {
        for p in self.buf.iter_mut() {
            *p = c;
        }
    }

    pub fn fill_rect(&mut self, x: usize, y: usize, w: usize, h: usize, c: Color) {
        for yy in y..(y + h).min(self.h) {
            let base = yy * self.w;
            for xx in x..(x + w).min(self.w) {
                self.buf[base + xx] = c;
            }
        }
    }

    pub fn glyph(&mut self, px: usize, py: usize, ch: char, fg: Color) {
        let code = ch as u32;
        if !(32..=126).contains(&code) {
            return;
        }
        let base = (code as usize - 32) * CH;
        for gy in 0..CH {
            let bits = GLYPHS[base + gy];
            let yy = py + gy;
            if yy >= self.h {
                break;
            }
            let row = yy * self.w;
            for gx in 0..CW {
                if bits & (1 << (6 - gx)) != 0 {
                    let xx = px + gx;
                    if xx < self.w {
                        self.buf[row + xx] = fg;
                    }
                }
            }
        }
    }

    // Draw text at character cell (col,row). Clips to screen.
    pub fn text(&mut self, col: usize, row: usize, s: &str, fg: Color) {
        let py = row * CH;
        for (i, ch) in s.chars().enumerate() {
            let px = (col + i) * CW;
            if px >= self.w {
                break;
            }
            self.glyph(px, py, ch, fg);
        }
    }

    // A full-width row painted with a background then text (for the selection bar).
    pub fn row_bar(&mut self, row: usize, s: &str, fg: Color, bg: Color) {
        self.fill_rect(0, row * CH, self.w, CH, bg);
        self.text(0, row, s, fg);
    }

    pub fn flush(&mut self) {
        let row_bytes = self.w * 2;
        unsafe {
            if self.stride == row_bytes {
                let total = self.h * row_bytes;
                if total <= self.map_len {
                    std::ptr::copy_nonoverlapping(
                        self.buf.as_ptr() as *const u8,
                        self.map,
                        total,
                    );
                }
            } else {
                for y in 0..self.h {
                    let src = self.buf.as_ptr().add(y * self.w) as *const u8;
                    let dst = self.map.add(y * self.stride);
                    if (y + 1) * self.stride <= self.map_len {
                        std::ptr::copy_nonoverlapping(src, dst, row_bytes);
                    }
                }
            }
        }
    }

    // The launcher blanks the backlight when idle; undo that. Best effort.
    pub fn wake() {
        backlight_on();
    }
}

impl Drop for Screen {
    fn drop(&mut self) {
        unsafe {
            libc::munmap(self.map as *mut libc::c_void, self.map_len);
        }
    }
}

// ------------------------------------------------------------- backlight
// Turn the panel's backlight up to its own maximum. The scale is NOT 0-255:
// this panel reports max_brightness=100, so writing 255 fails with EINVAL and
// the screen stays dark - which is invisible to a /dev/fb0 screenshot, because
// a framebuffer grab shows the pixels whether or not the light is on. Read the
// max from sysfs and write that. Best effort: the node is root:video 0664, and
// `phil` is in `video`.
pub fn backlight_on() {
    let dirs = match std::fs::read_dir("/sys/class/backlight") {
        Ok(d) => d,
        Err(_) => return,
    };
    for e in dirs.flatten() {
        let base = e.path();
        let max = std::fs::read_to_string(base.join("max_brightness"))
            .ok()
            .and_then(|s| s.trim().parse::<u32>().ok())
            .filter(|m| *m > 0)
            .unwrap_or(100);
        let _ = std::fs::write(base.join("brightness"), max.to_string());
        // bl_power: 0 = on, 4 = FB_BLANK_POWERDOWN.
        let _ = std::fs::write(base.join("bl_power"), b"0");
    }
}
