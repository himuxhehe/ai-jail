//! Persistent terminal status bar using ANSI scroll region.
//!
//! Shrinks the scrollable area by one row and renders a fixed
//! status line on the last row. `redraw()` is async-signal-safe
//! so it can be called from the SIGWINCH handler.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

static ACTIVE: AtomicBool = AtomicBool::new(false);
/// true = dark (bright white on black), false = light (black on bright white)
static STYLE_DARK: AtomicBool = AtomicBool::new(true);

const MAX_DIR: usize = 4096;
/// Project directory bytes (written once in `setup`, read in
/// signal handler after `ACTIVE` is set).
static mut DIR_BUF: [u8; MAX_DIR] = [0u8; MAX_DIR];
static DIR_LEN: AtomicUsize = AtomicUsize::new(0);

// " ai-jail ─ " (11 visible columns)
const PREFIX: &[u8] = b" ai-jail \xe2\x94\x80 ";
const PREFIX_VIS: usize = 11;

// "─" as UTF-8
const BOX_DASH: [u8; 3] = [0xe2, 0x94, 0x80];

fn term_size() -> Option<(u16, u16)> {
    let mut ws = unsafe { std::mem::zeroed::<nix::libc::winsize>() };
    let ret = unsafe {
        nix::libc::ioctl(
            nix::libc::STDOUT_FILENO,
            nix::libc::TIOCGWINSZ,
            &mut ws,
        )
    };
    if ret == 0 && ws.ws_row > 0 && ws.ws_col > 0 {
        Some((ws.ws_row, ws.ws_col))
    } else {
        None
    }
}

/// Async-signal-safe write to stdout.
fn raw_write(bytes: &[u8]) {
    let mut off = 0;
    while off < bytes.len() {
        let n = unsafe {
            nix::libc::write(
                nix::libc::STDOUT_FILENO,
                bytes[off..].as_ptr() as *const nix::libc::c_void,
                bytes.len() - off,
            )
        };
        if n <= 0 {
            break;
        }
        off += n as usize;
    }
}

/// Write a u16 as decimal digits into `buf`. Returns byte count.
fn write_u16(n: u16, buf: &mut [u8]) -> usize {
    if n == 0 {
        buf[0] = b'0';
        return 1;
    }
    let mut digits = [0u8; 5];
    let mut len = 0;
    let mut v = n;
    while v > 0 {
        digits[len] = b'0' + (v % 10) as u8;
        len += 1;
        v /= 10;
    }
    for i in 0..len {
        buf[i] = digits[len - 1 - i];
    }
    len
}

/// Set up the status bar. Call before spawning the child.
/// `style` must be `"dark"` or `"light"`.
pub fn setup(project_dir: &std::path::Path, style: &str) {
    use std::os::unix::ffi::OsStrExt;

    STYLE_DARK.store(style != "light", Ordering::SeqCst);

    let dir_bytes = project_dir.as_os_str().as_bytes();
    let len = dir_bytes.len().min(MAX_DIR);

    // SAFETY: single-threaded at this point (before child spawn).
    unsafe {
        DIR_BUF[..len].copy_from_slice(&dir_bytes[..len]);
    }
    DIR_LEN.store(len, Ordering::SeqCst);

    let Some((rows, cols)) = term_size() else {
        return;
    };
    if rows < 2 {
        return;
    }

    ACTIVE.store(true, Ordering::SeqCst);
    draw(rows, cols);

    // Ensure cursor is within the scroll region. draw() restores
    // the saved cursor, but if it was on the last row (now the
    // status bar), child output would land there. Move to the
    // bottom of the scroll region.
    let mut cb = [0u8; 16];
    let mut cp = 0;
    cb[cp..cp + 2].copy_from_slice(b"\x1b[");
    cp += 2;
    cp += write_u16(rows - 1, &mut cb[cp..]);
    cb[cp..cp + 3].copy_from_slice(b";1H");
    cp += 3;
    raw_write(&cb[..cp]);
}

/// Tear down the status bar. Call after child exits.
pub fn teardown() {
    if !ACTIVE.load(Ordering::SeqCst) {
        return;
    }
    ACTIVE.store(false, Ordering::SeqCst);

    let rows = term_size().map(|(r, _)| r).unwrap_or(24);

    // Reset scroll region, move to last row, clear it.
    let mut buf = [0u8; 64];
    let mut pos = 0;

    // \x1b[r
    buf[pos..pos + 3].copy_from_slice(b"\x1b[r");
    pos += 3;
    // \x1b[{rows};1H
    buf[pos] = b'\x1b';
    pos += 1;
    buf[pos] = b'[';
    pos += 1;
    pos += write_u16(rows, &mut buf[pos..]);
    buf[pos..pos + 3].copy_from_slice(b";1H");
    pos += 3;
    // \x1b[2K (clear line)
    buf[pos..pos + 4].copy_from_slice(b"\x1b[2K");
    pos += 4;

    raw_write(&buf[..pos]);
}

/// Redraw on resize. Async-signal-safe.
pub fn redraw() {
    if !ACTIVE.load(Ordering::SeqCst) {
        return;
    }
    let Some((rows, cols)) = term_size() else {
        return;
    };
    if rows < 2 {
        return;
    }
    draw(rows, cols);
}

/// Render scroll region + status line. Async-signal-safe.
fn draw(rows: u16, cols: u16) {
    let dir_len = DIR_LEN.load(Ordering::SeqCst);
    let cols = cols as usize;

    // Max output: ~50 bytes escapes + cols*3 bytes content
    let mut buf = [0u8; 8192];
    let mut pos = 0;

    macro_rules! put {
        ($b:expr) => {{
            let b: &[u8] = $b;
            let end = (pos + b.len()).min(buf.len());
            buf[pos..end].copy_from_slice(&b[..end - pos]);
            pos = end;
        }};
    }

    // 1. Save cursor (must be before DECSTBM which moves to home)
    put!(b"\x1b7");

    // 2. Set scroll region: \x1b[1;{rows-1}r
    put!(b"\x1b[1;");
    pos += write_u16(rows - 1, &mut buf[pos..]);
    put!(b"r");

    // 3. Move to last row: \x1b[{rows};1H
    put!(b"\x1b[");
    pos += write_u16(rows, &mut buf[pos..]);
    put!(b";1H");

    // 4. Style: dark = bold bright white on black,
    //          light = bold black on bright white
    if STYLE_DARK.load(Ordering::SeqCst) {
        put!(b"\x1b[1;97;40m");
    } else {
        put!(b"\x1b[1;30;107m");
    }

    // 5. Build visible content
    let mut vis = 0;

    // Prefix
    if PREFIX_VIS <= cols {
        put!(PREFIX);
        vis += PREFIX_VIS;
    }

    // Project dir (truncated if needed; reserve 1 for trailing
    // space before fill)
    let max_dir = if cols > vis + 1 { cols - vis - 1 } else { 0 };
    let dir_vis = dir_len.min(max_dir);
    if dir_vis > 0 {
        let slice = unsafe { &DIR_BUF[..dir_vis] };
        put!(slice);
        vis += dir_vis;
    }

    // Separator space
    if vis < cols {
        put!(b" ");
        vis += 1;
    }

    // Fill with "─"
    while vis < cols && pos + 3 <= buf.len() {
        put!(&BOX_DASH);
        vis += 1;
    }

    // 6. Reset attributes
    put!(b"\x1b[0m");

    // 7. Restore cursor
    put!(b"\x1b8");

    raw_write(&buf[..pos]);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_u16_zero() {
        let mut buf = [0u8; 5];
        let n = write_u16(0, &mut buf);
        assert_eq!(&buf[..n], b"0");
    }

    #[test]
    fn write_u16_single_digit() {
        let mut buf = [0u8; 5];
        let n = write_u16(7, &mut buf);
        assert_eq!(&buf[..n], b"7");
    }

    #[test]
    fn write_u16_multi_digit() {
        let mut buf = [0u8; 5];
        let n = write_u16(1024, &mut buf);
        assert_eq!(&buf[..n], b"1024");
    }

    #[test]
    fn write_u16_max() {
        let mut buf = [0u8; 5];
        let n = write_u16(65535, &mut buf);
        assert_eq!(&buf[..n], b"65535");
    }

    #[test]
    fn active_default_false() {
        assert!(!ACTIVE.load(Ordering::SeqCst));
    }
}
