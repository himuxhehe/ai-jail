//! PTY proxy for persistent status bar.
//!
//! When the status bar is enabled, ai-jail interposes a PTY between
//! itself and the sandbox child. The child writes to the PTY slave
//! while ai-jail owns the real terminal, allowing the status bar to
//! persist regardless of what the child does (clear, reset, vim, etc).
//!
//! All bytes are forwarded verbatim. The only post-processing is
//! detecting sequences that reset the scroll region (alt screen
//! switches, RIS, bare DECSTBM reset) and re-asserting the status
//! bar afterward.

use nix::poll::{poll, PollFd, PollFlags, PollTimeout};
use nix::sys::termios::{self, SetArg, Termios};
use std::os::unix::io::{AsRawFd, BorrowedFd, OwnedFd};
use std::sync::atomic::{AtomicI32, Ordering};

/// Stored master raw FD for async-signal-safe resize from SIGWINCH.
static MASTER_FD: AtomicI32 = AtomicI32::new(-1);

/// Resize the PTY slave to match the real terminal (minus one row
/// for the status bar). Async-signal-safe: only uses ioctl + atomics.
pub fn resize_pty() {
    let master = MASTER_FD.load(Ordering::SeqCst);
    if master < 0 {
        return;
    }
    let mut ws = unsafe { std::mem::zeroed::<nix::libc::winsize>() };
    let ret = unsafe {
        nix::libc::ioctl(
            nix::libc::STDOUT_FILENO,
            nix::libc::TIOCGWINSZ,
            &mut ws,
        )
    };
    if ret != 0 || ws.ws_row < 2 || ws.ws_col == 0 {
        return;
    }
    ws.ws_row -= 1;
    unsafe {
        nix::libc::ioctl(master, nix::libc::TIOCSWINSZ, &ws);
    }
}

fn enter_raw_mode() -> Result<Termios, String> {
    let stdin = std::io::stdin();
    let saved =
        termios::tcgetattr(&stdin).map_err(|e| format!("tcgetattr: {e}"))?;
    let mut raw = saved.clone();
    termios::cfmakeraw(&mut raw);
    termios::tcsetattr(&stdin, SetArg::TCSANOW, &raw)
        .map_err(|e| format!("tcsetattr raw: {e}"))?;
    Ok(saved)
}

fn restore_mode(saved: &Termios) {
    let stdin = std::io::stdin();
    let _ = termios::tcsetattr(&stdin, SetArg::TCSANOW, saved);
}

fn set_initial_size(fd: &OwnedFd, rows: u16, cols: u16) {
    let ws = nix::libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    unsafe {
        nix::libc::ioctl(fd.as_raw_fd(), nix::libc::TIOCSWINSZ, &ws);
    }
}

/// Scan for escape sequences that reset the scroll region.
///
/// Detected sequences:
/// - `\x1b[r` / `\x1b[;r` — bare DECSTBM reset
/// - `\x1bc` — RIS (full terminal reset)
/// - `\x1b[?1049h/l`, `\x1b[?47h/l`, `\x1b[?1047h/l` — alt screen
#[cfg(test)]
fn contains_scroll_reset(data: &[u8]) -> bool {
    let mut i = 0;
    while i < data.len() {
        if data[i] != 0x1b {
            i += 1;
            continue;
        }
        i += 1;
        if i >= data.len() {
            break;
        }
        match data[i] {
            b'c' => return true,
            b'[' => {
                i += 1;
                let ps = i;
                // Parameter bytes: 0x30..=0x3f (digits ; ? etc)
                while i < data.len() && (0x30..=0x3f).contains(&data[i]) {
                    i += 1;
                }
                let params = &data[ps..i];
                // Intermediate bytes: 0x20..=0x2f
                while i < data.len() && (0x20..=0x2f).contains(&data[i]) {
                    i += 1;
                }
                if i >= data.len() {
                    break;
                }
                let fin = data[i];
                i += 1;
                // Bare DECSTBM reset: \x1b[r or \x1b[;r
                if fin == b'r' && (params.is_empty() || params == b";") {
                    return true;
                }
                // Private mode set/reset with ? prefix
                if (fin == b'h' || fin == b'l') && params.first() == Some(&b'?')
                {
                    let modes = &params[1..];
                    for part in modes.split(|&b| b == b';') {
                        if part == b"1049" || part == b"47" || part == b"1047" {
                            return true;
                        }
                    }
                }
            }
            _ => {
                i += 1;
            }
        }
    }
    false
}

/// Tracks whether the child output stream is mid-escape-sequence.
/// We must not inject our status bar redraw while the terminal is
/// processing a partial CSI/OSC — doing so garbles the output.
#[derive(Clone, Copy)]
enum SeqState {
    Normal,
    Esc, // saw \x1b
    Csi, // saw \x1b[, accumulating params
    Osc, // saw \x1b], until BEL or ST
}

impl SeqState {
    fn update(&mut self, data: &[u8]) {
        for &b in data {
            *self = match *self {
                SeqState::Normal => {
                    if b == 0x1b {
                        SeqState::Esc
                    } else {
                        SeqState::Normal
                    }
                }
                SeqState::Esc => match b {
                    b'[' => SeqState::Csi,
                    b']' => SeqState::Osc,
                    0x20..=0x2f => SeqState::Esc,
                    _ => SeqState::Normal,
                },
                SeqState::Csi => match b {
                    0x20..=0x3f => SeqState::Csi,
                    0x1b => SeqState::Esc,
                    _ => SeqState::Normal,
                },
                SeqState::Osc => match b {
                    0x07 => SeqState::Normal,
                    0x1b => SeqState::Esc,
                    _ => SeqState::Osc,
                },
            };
        }
    }
}

fn io_loop(master: &OwnedFd) {
    let stdin_fd = std::io::stdin().as_raw_fd();
    let master_raw = master.as_raw_fd();
    let stdin_bfd = unsafe { BorrowedFd::borrow_raw(stdin_fd) };
    let master_bfd = unsafe { BorrowedFd::borrow_raw(master_raw) };
    let mut buf = [0u8; 8192];
    let mut seq = SeqState::Normal;

    loop {
        let mut fds = [
            PollFd::new(stdin_bfd, PollFlags::POLLIN),
            PollFd::new(master_bfd, PollFlags::POLLIN),
        ];

        match poll(&mut fds, PollTimeout::from(100_u16)) {
            Ok(0) => {
                // Timeout — if we deferred a redraw, do it now
                if !matches!(seq, SeqState::Normal) {
                    seq = SeqState::Normal;
                    crate::statusbar::redraw();
                }
                continue;
            }
            Err(nix::errno::Errno::EINTR) => continue,
            Err(_) => break,
            Ok(_) => {}
        }

        // Check master (child output) first for responsiveness
        if let Some(revents) = fds[1].revents() {
            if revents.contains(PollFlags::POLLIN) {
                match nix::unistd::read(master_raw, &mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        write_all_raw(nix::libc::STDOUT_FILENO, &buf[..n]);
                        seq.update(&buf[..n]);
                        if matches!(seq, SeqState::Normal) {
                            crate::statusbar::redraw();
                        }
                    }
                    Err(nix::errno::Errno::EINTR) => {}
                    Err(nix::errno::Errno::EIO) => break,
                    Err(_) => break,
                }
            }
            if revents.contains(PollFlags::POLLHUP)
                || revents.contains(PollFlags::POLLERR)
            {
                loop {
                    match nix::unistd::read(master_raw, &mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            write_all_raw(nix::libc::STDOUT_FILENO, &buf[..n]);
                        }
                    }
                }
                break;
            }
        }

        // Check stdin (user input)
        if let Some(revents) = fds[0].revents() {
            if revents.contains(PollFlags::POLLIN) {
                match nix::unistd::read(stdin_fd, &mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        write_all_raw(master_raw, &buf[..n]);
                    }
                    Err(nix::errno::Errno::EINTR) => {}
                    Err(_) => break,
                }
            }
        }
    }
}

/// Write all bytes to a raw fd using libc::write (works in all
/// contexts including pre_exec).
fn write_all_raw(fd: i32, data: &[u8]) {
    let mut off = 0;
    while off < data.len() {
        let n = unsafe {
            nix::libc::write(
                fd,
                data[off..].as_ptr() as *const nix::libc::c_void,
                data.len() - off,
            )
        };
        if n <= 0 {
            break;
        }
        off += n as usize;
    }
}

/// Run the command through a PTY proxy. Creates PTY pair, enters
/// raw mode, spawns child with PTY slave as stdio, runs IO loop,
/// waits for child, restores terminal. Returns exit code.
pub fn run(cmd: &mut std::process::Command) -> Result<i32, String> {
    use std::os::unix::process::CommandExt;

    let (rows, cols) = real_term_size().unwrap_or((24, 80));
    if rows < 2 {
        return Err("Terminal too small for status bar".into());
    }

    // Create PTY pair
    let pty =
        nix::pty::openpty(None, None).map_err(|e| format!("openpty: {e}"))?;
    let master = pty.master;
    let slave = pty.slave;

    // Set FD_CLOEXEC on master so child doesn't inherit it
    let master_raw = master.as_raw_fd();
    unsafe {
        let flags = nix::libc::fcntl(master_raw, nix::libc::F_GETFD);
        nix::libc::fcntl(
            master_raw,
            nix::libc::F_SETFD,
            flags | nix::libc::FD_CLOEXEC,
        );
    }

    // Set initial PTY size (rows-1 for status bar)
    set_initial_size(&master, rows - 1, cols);

    // Store master raw FD for signal handler
    MASTER_FD.store(master_raw, Ordering::SeqCst);

    // Enter raw mode on real stdin
    let saved = enter_raw_mode()?;

    // Configure child to use PTY slave as stdin/stdout/stderr
    let slave_raw = slave.as_raw_fd();
    unsafe {
        cmd.pre_exec(move || {
            if nix::libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            if nix::libc::ioctl(slave_raw, nix::libc::TIOCSCTTY, 0) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            nix::libc::dup2(slave_raw, 0);
            nix::libc::dup2(slave_raw, 1);
            nix::libc::dup2(slave_raw, 2);
            if slave_raw > 2 {
                nix::libc::close(slave_raw);
            }
            Ok(())
        });
    }

    // Spawn child
    let child = cmd
        .spawn()
        .map_err(|e| format!("Failed to start sandbox: {e}"))?;

    let pid = child.id() as i32;
    crate::signals::set_child_pid(pid);

    // Close slave in parent — child has its own copy
    drop(slave);

    // Run IO loop (blocks until child exits / master HUP)
    io_loop(&master);

    // Clean up
    MASTER_FD.store(-1, Ordering::SeqCst);
    drop(master);
    restore_mode(&saved);

    // Wait for child
    let exit_code = crate::signals::wait_child(pid);

    // Prevent double-wait
    std::mem::forget(child);

    Ok(exit_code)
}

fn real_term_size() -> Option<(u16, u16)> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_bare_decstbm_reset() {
        assert!(contains_scroll_reset(b"\x1b[r"));
    }

    #[test]
    fn detect_semicolon_decstbm_reset() {
        assert!(contains_scroll_reset(b"\x1b[;r"));
    }

    #[test]
    fn ignore_parameterized_decstbm() {
        assert!(!contains_scroll_reset(b"\x1b[1;24r"));
    }

    #[test]
    fn detect_ris() {
        assert!(contains_scroll_reset(b"\x1bc"));
    }

    #[test]
    fn detect_alt_screen_1049h() {
        assert!(contains_scroll_reset(b"\x1b[?1049h"));
    }

    #[test]
    fn detect_alt_screen_1049l() {
        assert!(contains_scroll_reset(b"\x1b[?1049l"));
    }

    #[test]
    fn detect_alt_screen_47h() {
        assert!(contains_scroll_reset(b"\x1b[?47h"));
    }

    #[test]
    fn detect_alt_screen_1047l() {
        assert!(contains_scroll_reset(b"\x1b[?1047l"));
    }

    #[test]
    fn ignore_show_cursor() {
        assert!(!contains_scroll_reset(b"\x1b[?25h"));
    }

    #[test]
    fn ignore_sgr_color() {
        assert!(!contains_scroll_reset(b"\x1b[38;2;255;100;0m"));
    }

    #[test]
    fn ignore_clear_screen() {
        assert!(!contains_scroll_reset(b"\x1b[2J"));
    }

    #[test]
    fn detect_embedded_in_output() {
        let data = b"hello\x1b[?1049hworld";
        assert!(contains_scroll_reset(data));
    }

    #[test]
    fn no_false_positive_plain_text() {
        assert!(!contains_scroll_reset(b"just plain text\n"));
    }

    #[test]
    fn detect_alt_screen_combined_modes() {
        // Some programs set multiple modes at once
        assert!(contains_scroll_reset(b"\x1b[?1049;2004h"));
    }
}
