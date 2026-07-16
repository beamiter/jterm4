use gtk4::glib;
use nix::libc;
use nix::pty::{openpty, OpenptyResult};
use nix::unistd::{self, ForkResult, Pid};
use std::borrow::Cow;
use std::ffi::CString;
use std::io::{self, Read as _};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};

use crate::state::terminate_terminal_process;

enum PtyMsg {
    Data(Vec<u8>),
    Exit(i32),
}

pub struct OwnedPty {
    master: std::sync::Arc<std::sync::Mutex<Option<OwnedFd>>>,
    /// Terminal input is written by a dedicated worker. A full PTY kernel buffer
    /// therefore backpressures that worker rather than GTK's main thread.
    input_tx: mpsc::Sender<Vec<u8>>,
    pid: Pid,
    /// Tracks explicit bracketed-paste frames whose start, body, and end are
    /// delivered through separate `write_bytes` calls.
    outgoing_bracketed_paste: AtomicBool,
    /// Mirrors the shell's DECSET/DECRST 2004 state observed on PTY output so
    /// multiline insertion can be protected at the central input boundary.
    shell_bracketed_paste: Arc<AtomicBool>,
}

// Raw GLib FFI for g_unix_fd_add_full (not exposed by glib-rs 0.22)
extern "C" {
    fn g_unix_fd_add_full(
        priority: i32,
        fd: i32,
        condition: u32,
        function: extern "C" fn(fd: i32, condition: u32, user_data: *mut std::ffi::c_void) -> i32,
        user_data: *mut std::ffi::c_void,
        notify: extern "C" fn(data: *mut std::ffi::c_void),
    ) -> u32;
}

const G_IO_IN: u32 = 1;
// A block command may continuously repaint a spinner or progress bar. Keep PTY
// delivery at idle priority so GTK can dispatch pointer/button events first.
const G_PRIORITY_DEFAULT_IDLE: i32 = 200;
/// Bound queued output. Once this queue fills, the reader blocks and the kernel
/// PTY buffer provides natural backpressure to a runaway producer.
const PTY_QUEUE_CAPACITY: usize = 8;
/// Smaller chunks cap the amount of VTE feeding performed in one UI callback.
const PTY_READ_CHUNK_BYTES: usize = 32 * 1024;
/// Keep a continuously-ready PTY from monopolizing GTK's main loop. The first
/// chunk is dispatched immediately; queued follow-ups are paced at this rate.
const PTY_DISPATCH_INTERVAL: std::time::Duration = std::time::Duration::from_millis(8);

const BRACKETED_PASTE_START: &[u8] = b"\x1b[200~";
const BRACKETED_PASTE_END: &[u8] = b"\x1b[201~";
const BRACKETED_PASTE_ENABLE: &[u8] = b"\x1b[?2004h";
const BRACKETED_PASTE_DISABLE: &[u8] = b"\x1b[?2004l";

/// Observe DECSET/DECRST 2004 in output whose escape sequence may be split
/// across adjacent PTY read chunks.  Only a short suffix is retained.
fn observe_bracketed_paste_mode(current: bool, tail: &mut Vec<u8>, data: &[u8]) -> bool {
    let mut combined = Vec::with_capacity(tail.len() + data.len());
    combined.extend_from_slice(tail);
    combined.extend_from_slice(data);

    let mut enabled = current;
    let mut index = 0usize;
    while index < combined.len() {
        let rest = &combined[index..];
        if rest.starts_with(BRACKETED_PASTE_ENABLE) {
            enabled = true;
            index += BRACKETED_PASTE_ENABLE.len();
        } else if rest.starts_with(BRACKETED_PASTE_DISABLE) {
            enabled = false;
            index += BRACKETED_PASTE_DISABLE.len();
        } else {
            index += 1;
        }
    }

    let bridge_len = BRACKETED_PASTE_ENABLE
        .len()
        .max(BRACKETED_PASTE_DISABLE.len())
        .saturating_sub(1);
    let keep_from = combined.len().saturating_sub(bridge_len);
    tail.clear();
    tail.extend_from_slice(&combined[keep_from..]);
    enabled
}

/// Protect insertion-only input from becoming several unintended submissions.
///
/// Explicit submissions (a payload ending in CR) and explicitly framed paste
/// data pass through unchanged.  Otherwise multiline input is bracketed when
/// the shell advertises DECSET 2004, and safely reduced to the first logical
/// line when it does not.  Single-line typing is never rewritten.
fn sanitize_input_chunk(
    data: &[u8],
    paste_active: bool,
    shell_supports_bracketed_paste: bool,
) -> (Cow<'_, [u8]>, bool) {
    let starts_paste = data.starts_with(BRACKETED_PASTE_START);
    let ends_paste = data.ends_with(BRACKETED_PASTE_END);
    let protected_by_paste = paste_active || starts_paste;
    let next_paste_active = if ends_paste {
        false
    } else if starts_paste {
        true
    } else {
        paste_active
    };

    if protected_by_paste || data.ends_with(b"\r") {
        return (Cow::Borrowed(data), next_paste_active);
    }

    let Some(first_break) = data.iter().position(|&byte| byte == b'\r' || byte == b'\n') else {
        return (Cow::Borrowed(data), next_paste_active);
    };

    if shell_supports_bracketed_paste {
        let mut wrapped = Vec::with_capacity(
            BRACKETED_PASTE_START.len() + data.len() + BRACKETED_PASTE_END.len(),
        );
        wrapped.extend_from_slice(BRACKETED_PASTE_START);
        wrapped.extend_from_slice(data);
        wrapped.extend_from_slice(BRACKETED_PASTE_END);
        return (Cow::Owned(wrapped), next_paste_active);
    }

    (Cow::Owned(data[..first_break].to_vec()), next_paste_active)
}

struct FdWatchData<F: FnMut() -> bool> {
    callback: F,
}

extern "C" fn fd_watch_callback<F: FnMut() -> bool>(
    _fd: i32,
    _condition: u32,
    user_data: *mut std::ffi::c_void,
) -> i32 {
    let data = unsafe { &mut *(user_data as *mut FdWatchData<F>) };
    if (data.callback)() {
        1
    } else {
        0
    }
}

extern "C" fn fd_watch_destroy<F: FnMut() -> bool>(user_data: *mut std::ffi::c_void) {
    unsafe {
        drop(Box::from_raw(user_data as *mut FdWatchData<F>));
    }
}

fn unix_fd_add_local<F: FnMut() -> bool + 'static>(fd: RawFd, func: F) {
    let data = Box::new(FdWatchData { callback: func });
    let ptr = Box::into_raw(data) as *mut std::ffi::c_void;
    unsafe {
        g_unix_fd_add_full(
            G_PRIORITY_DEFAULT_IDLE,
            fd,
            G_IO_IN,
            fd_watch_callback::<F>,
            ptr,
            fd_watch_destroy::<F>,
        );
    }
}

/// Write a complete byte slice, retrying interrupted and partial writes.
///
/// This function is intentionally used only by the background writer thread:
/// blocking on a full PTY is correct backpressure there, but would freeze GTK if
/// performed by a key, paste, or block-recall callback.
fn write_all_fd(fd: RawFd, mut data: &[u8]) -> io::Result<()> {
    while !data.is_empty() {
        let written = unsafe { libc::write(fd, data.as_ptr().cast::<libc::c_void>(), data.len()) };
        if written > 0 {
            data = &data[written as usize..];
            continue;
        }
        if written == 0 {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "PTY write returned zero",
            ));
        }
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::Interrupted {
            continue;
        }
        return Err(error);
    }
    Ok(())
}

fn spawn_fd_writer(fd: OwnedFd) -> io::Result<mpsc::Sender<Vec<u8>>> {
    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    std::thread::Builder::new()
        .name("jterm4-pty-writer".to_string())
        .spawn(move || {
            for data in rx {
                if let Err(error) = write_all_fd(fd.as_raw_fd(), &data) {
                    log::warn!("PTY input writer stopped: {error}");
                    break;
                }
            }
        })?;
    Ok(tx)
}

impl OwnedPty {
    fn close_master_fd(&self) {
        if let Ok(mut guard) = self.master.lock() {
            guard.take();
        }
    }

    pub fn spawn(argv: &[&str], cwd: Option<&str>, env_extra: &[(&str, &str)]) -> io::Result<Self> {
        let argv_owned: Vec<String> = argv.iter().map(|value| (*value).to_string()).collect();
        let host_bridge = crate::host::is_flatpak();
        let executable_argv = crate::host::wrap_argv(&argv_owned, cwd, env_extra);
        if executable_argv.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "empty PTY argv",
            ));
        }

        let initial_size = nix::pty::Winsize {
            ws_row: 24,
            ws_col: 80,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        let OpenptyResult { master, slave } =
            openpty(Some(&initial_size), None).map_err(io::Error::other)?;

        match unsafe { unistd::fork() } {
            Ok(ForkResult::Child) => {
                drop(master);
                let slave_fd = slave.as_raw_fd();

                unsafe {
                    if libc::setsid() < 0 {
                        eprintln!("setsid() failed: {}", std::io::Error::last_os_error());
                        std::process::exit(1);
                    }
                    libc::ioctl(slave_fd, libc::TIOCSCTTY, 0);
                    libc::dup2(slave_fd, 0);
                    libc::dup2(slave_fd, 1);
                    libc::dup2(slave_fd, 2);
                }
                drop(slave);

                if !host_bridge {
                    if let Some(dir) = cwd {
                        let _ = std::env::set_current_dir(dir);
                    }
                }
                for (key, val) in env_extra {
                    unsafe { std::env::set_var(key, val) };
                }
                unsafe { std::env::set_var("TERM", "xterm-256color") };

                let c_argv: Vec<CString> = executable_argv
                    .iter()
                    .map(|argument| CString::new(argument.as_str()).unwrap())
                    .collect();
                let _ = unistd::execvp(&c_argv[0], &c_argv);
                std::process::exit(127);
            }
            Ok(ForkResult::Parent { child }) => {
                drop(slave);
                let writer_fd = match master.try_clone() {
                    Ok(fd) => fd,
                    Err(error) => {
                        terminate_terminal_process(child.as_raw());
                        return Err(error);
                    }
                };
                let input_tx = match spawn_fd_writer(writer_fd) {
                    Ok(tx) => tx,
                    Err(error) => {
                        terminate_terminal_process(child.as_raw());
                        return Err(error);
                    }
                };
                Ok(OwnedPty {
                    master: std::sync::Arc::new(std::sync::Mutex::new(Some(master))),
                    input_tx,
                    pid: child,
                    outgoing_bracketed_paste: AtomicBool::new(false),
                    shell_bracketed_paste: Arc::new(AtomicBool::new(false)),
                })
            }
            Err(error) => Err(io::Error::other(error)),
        }
    }

    pub fn pid_i32(&self) -> i32 {
        self.pid.as_raw()
    }

    /// Raw master-side fd, or -1 if the PTY has already been closed.
    ///
    /// The descriptor remains owned by this `OwnedPty`; callers only borrow the
    /// integer long enough for non-mutating probes such as `tcgetpgrp(3)`.
    pub fn master_fd_raw(&self) -> i32 {
        self.master
            .lock()
            .ok()
            .and_then(|guard| guard.as_ref().map(AsRawFd::as_raw_fd))
            .unwrap_or(-1)
    }

    pub fn write_bytes(&self, data: &[u8]) {
        let paste_active = self.outgoing_bracketed_paste.load(Ordering::Relaxed);
        let shell_supports_bracketed_paste = self.shell_bracketed_paste.load(Ordering::Relaxed);
        let (safe_data, next_paste_active) =
            sanitize_input_chunk(data, paste_active, shell_supports_bracketed_paste);
        self.outgoing_bracketed_paste
            .store(next_paste_active, Ordering::Relaxed);

        if safe_data.is_empty() {
            return;
        }
        if let Err(error) = self.input_tx.send(safe_data.into_owned()) {
            log::warn!(
                "PTY input queue is closed; discarded {} byte(s)",
                error.0.len()
            );
        }
    }

    pub fn resize(&self, cols: u16, rows: u16) {
        if let Ok(guard) = self.master.lock() {
            if let Some(fd) = guard.as_ref() {
                let ws = libc::winsize {
                    ws_row: rows,
                    ws_col: cols,
                    ws_xpixel: 0,
                    ws_ypixel: 0,
                };
                unsafe {
                    libc::ioctl(fd.as_raw_fd(), libc::TIOCSWINSZ, &ws);
                }
            }
        }
    }

    pub fn kill(&self) {
        self.close_master_fd();
        terminate_terminal_process(self.pid.as_raw());
    }

    /// Start an async reader. A bounded channel transfers 32 KiB chunks to the
    /// GLib main thread; when the UI falls behind, the child is naturally slowed
    /// through the channel and kernel PTY buffers instead of growing memory
    /// without limit.
    pub fn start_reader<F, E>(&self, mut callback: F, on_exit: E)
    where
        F: FnMut(Vec<u8>) + 'static,
        E: FnOnce(i32) + 'static,
    {
        let reader_fd = match self
            .master
            .lock()
            .ok()
            .and_then(|guard| guard.as_ref().and_then(|fd| fd.try_clone().ok()))
        {
            Some(fd) => fd,
            None => return,
        };

        let child_pid = self.pid;
        let (tx, rx) = mpsc::sync_channel::<PtyMsg>(PTY_QUEUE_CAPACITY);

        // Create an eventfd for signaling data availability to the main thread.
        let efd: RawFd = unsafe { libc::eventfd(0, libc::EFD_NONBLOCK | libc::EFD_CLOEXEC) };
        if efd < 0 {
            self.start_reader_polling(reader_fd, child_pid, tx, rx, callback, on_exit);
            return;
        }
        let eventfd = Arc::new(unsafe { OwnedFd::from_raw_fd(efd) });
        let wake_pending = Arc::new(AtomicBool::new(false));
        let eventfd_for_thread = Arc::clone(&eventfd);
        let wake_pending_for_thread = Arc::clone(&wake_pending);
        let shell_bracketed_paste = Arc::clone(&self.shell_bracketed_paste);
        spawn_reader_thread(
            reader_fd,
            child_pid,
            tx,
            "jterm4-pty-reader",
            shell_bracketed_paste,
            move || {
                notify_eventfd_once(&eventfd_for_thread, &wake_pending_for_thread);
            },
        );

        let on_exit = std::cell::Cell::new(Some(on_exit));

        unix_fd_add_local(eventfd.as_raw_fd(), move || {
            drain_eventfd(eventfd.as_raw_fd());

            // A producer may enqueue between the first empty read and clearing
            // `wake_pending`. Recheck after clearing so that transition cannot
            // lose its only eventfd notification.
            let message = match rx.try_recv() {
                Ok(message) => message,
                Err(mpsc::TryRecvError::Empty) => {
                    wake_pending.store(false, Ordering::Release);
                    match rx.try_recv() {
                        Ok(message) => {
                            wake_pending.store(true, Ordering::Release);
                            drain_eventfd(eventfd.as_raw_fd());
                            message
                        }
                        Err(mpsc::TryRecvError::Empty) => return true,
                        Err(mpsc::TryRecvError::Disconnected) => return false,
                    }
                }
                Err(mpsc::TryRecvError::Disconnected) => return false,
            };

            match message {
                PtyMsg::Data(data) => {
                    callback(data);
                    let eventfd = Arc::clone(&eventfd);
                    glib::timeout_add_local_once(PTY_DISPATCH_INTERVAL, move || {
                        signal_eventfd(eventfd.as_raw_fd());
                    });
                    true
                }
                PtyMsg::Exit(code) => {
                    if let Some(f) = on_exit.take() {
                        f(code);
                    }
                    false
                }
            }
        });
    }

    fn start_reader_polling<F, E>(
        &self,
        reader_fd: OwnedFd,
        child_pid: Pid,
        tx: mpsc::SyncSender<PtyMsg>,
        rx: mpsc::Receiver<PtyMsg>,
        mut callback: F,
        on_exit: E,
    ) where
        F: FnMut(Vec<u8>) + 'static,
        E: FnOnce(i32) + 'static,
    {
        spawn_reader_thread(
            reader_fd,
            child_pid,
            tx,
            "jterm4-pty-reader-poll",
            Arc::clone(&self.shell_bracketed_paste),
            || {},
        );

        let on_exit = std::cell::Cell::new(Some(on_exit));
        let rx = std::cell::RefCell::new(rx);

        glib::timeout_add_local(PTY_DISPATCH_INTERVAL, move || {
            match rx.borrow().try_recv() {
                Ok(PtyMsg::Data(data)) => {
                    callback(data);
                    glib::ControlFlow::Continue
                }
                Ok(PtyMsg::Exit(code)) => {
                    if let Some(f) = on_exit.take() {
                        f(code);
                    }
                    glib::ControlFlow::Break
                }
                Err(mpsc::TryRecvError::Empty) => glib::ControlFlow::Continue,
                Err(mpsc::TryRecvError::Disconnected) => glib::ControlFlow::Break,
            }
        });
    }
}

fn spawn_reader_thread(
    reader_fd: OwnedFd,
    child_pid: Pid,
    tx: mpsc::SyncSender<PtyMsg>,
    thread_name: &'static str,
    shell_bracketed_paste: Arc<AtomicBool>,
    notify: impl Fn() + Send + 'static,
) {
    std::thread::Builder::new()
        .name(thread_name.to_string())
        .spawn(move || {
            let mut file = std::fs::File::from(reader_fd);
            let fd = file.as_raw_fd();
            let mut buf = [0u8; PTY_READ_CHUNK_BYTES];
            let mut mode_tail = Vec::with_capacity(BRACKETED_PASTE_ENABLE.len().saturating_sub(1));
            loop {
                match file.read(&mut buf) {
                    Ok(0) => break,
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                    Ok(n) => {
                        let mut combined = Vec::with_capacity(PTY_READ_CHUNK_BYTES);
                        combined.extend_from_slice(&buf[..n]);
                        coalesce_pending(fd, &mut file, &mut buf, &mut combined);
                        let mode = observe_bracketed_paste_mode(
                            shell_bracketed_paste.load(Ordering::Relaxed),
                            &mut mode_tail,
                            &combined,
                        );
                        shell_bracketed_paste.store(mode, Ordering::Relaxed);
                        if tx.send(PtyMsg::Data(combined)).is_err() {
                            return;
                        }
                        notify();
                    }
                }
            }

            let code = wait_for_child_exit(child_pid);
            if tx.send(PtyMsg::Exit(code)).is_ok() {
                notify();
            }
        })
        .expect("failed to spawn PTY reader thread");
}

fn wait_for_child_exit(child_pid: Pid) -> i32 {
    use nix::sys::wait::{waitpid, WaitPidFlag, WaitStatus};

    for _ in 0..50 {
        match waitpid(child_pid, Some(WaitPidFlag::WNOHANG)) {
            Ok(WaitStatus::Exited(_, code)) => return code,
            Ok(WaitStatus::Signaled(_, signal, _)) => return 128 + signal as i32,
            Err(_) | Ok(_) => std::thread::sleep(std::time::Duration::from_millis(100)),
        }
    }

    match waitpid(child_pid, None) {
        Ok(WaitStatus::Exited(_, code)) => code,
        Ok(WaitStatus::Signaled(_, signal, _)) => 128 + signal as i32,
        _ => 1,
    }
}

/// Merge bytes already waiting on the PTY into one bounded delivery. This
/// reduces GTK crossings for programs that emit a repaint in several writes.
fn coalesce_pending(fd: RawFd, file: &mut std::fs::File, buf: &mut [u8], combined: &mut Vec<u8>) {
    const MAX_FOLLOWUP_READS: u32 = 8;
    let mut follow_ups = 0u32;
    while combined.len() < PTY_READ_CHUNK_BYTES && follow_ups < MAX_FOLLOWUP_READS {
        let mut poll_fd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let ready = unsafe { libc::poll(&mut poll_fd, 1, 1) };
        if ready <= 0 || (poll_fd.revents & libc::POLLIN) == 0 {
            break;
        }

        let remaining = PTY_READ_CHUNK_BYTES - combined.len();
        let read_len = remaining.min(buf.len());
        match file.read(&mut buf[..read_len]) {
            Ok(0) => break,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => break,
            Ok(read) => combined.extend_from_slice(&buf[..read]),
        }
        follow_ups += 1;
    }
}

fn notify_eventfd_once(eventfd: &OwnedFd, wake_pending: &AtomicBool) {
    if !wake_pending.swap(true, Ordering::AcqRel) {
        signal_eventfd(eventfd.as_raw_fd());
    }
}

fn drain_eventfd(eventfd: RawFd) {
    let mut value = 0u64;
    unsafe {
        libc::read(
            eventfd,
            (&mut value as *mut u64).cast::<libc::c_void>(),
            std::mem::size_of::<u64>(),
        );
    }
}

fn signal_eventfd(efd: RawFd) {
    let val: u64 = 1;
    unsafe {
        libc::write(efd, &val as *const u64 as *const libc::c_void, 8);
    }
}

impl Drop for OwnedPty {
    fn drop(&mut self) {
        self.close_master_fd();
        terminate_terminal_process(self.pid.as_raw());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::os::unix::net::UnixStream;

    #[test]
    fn complete_writer_delivers_the_entire_payload() {
        let payload_len = 128 * 1024;
        let (mut reader, writer) = UnixStream::pair().expect("create socket pair");

        let handle = std::thread::spawn(move || {
            let payload = vec![0x5a; payload_len];
            write_all_fd(writer.as_raw_fd(), &payload).expect("write payload");
        });

        let mut received = vec![0; payload_len];
        reader.read_exact(&mut received).expect("read payload");
        handle.join().expect("writer thread");
        assert!(received.iter().all(|byte| *byte == 0x5a));
    }

    #[test]
    fn eventfd_wakeup_is_coalesced_until_consumer_rearms() {
        let raw = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC) };
        assert!(raw >= 0);
        let eventfd = unsafe { OwnedFd::from_raw_fd(raw) };
        let wake_pending = AtomicBool::new(false);

        notify_eventfd_once(&eventfd, &wake_pending);
        notify_eventfd_once(&eventfd, &wake_pending);

        let mut value = 0u64;
        let read = unsafe {
            libc::read(
                eventfd.as_raw_fd(),
                (&mut value as *mut u64).cast::<libc::c_void>(),
                std::mem::size_of::<u64>(),
            )
        };
        assert_eq!(read as usize, std::mem::size_of::<u64>());
        assert_eq!(value, 1);

        wake_pending.store(false, Ordering::Release);
        notify_eventfd_once(&eventfd, &wake_pending);
        value = 0;
        let read = unsafe {
            libc::read(
                eventfd.as_raw_fd(),
                (&mut value as *mut u64).cast::<libc::c_void>(),
                std::mem::size_of::<u64>(),
            )
        };
        assert_eq!(read as usize, std::mem::size_of::<u64>());
        assert_eq!(value, 1);
    }

    #[test]
    fn unframed_multiline_insert_falls_back_without_shell_support() {
        let (safe, active) = sanitize_input_chunk(b"echo first\necho second", false, false);
        assert_eq!(safe.as_ref(), b"echo first");
        assert!(!active);

        let (safe, _) = sanitize_input_chunk(b"echo first\r\necho second", false, false);
        assert_eq!(safe.as_ref(), b"echo first");
    }

    #[test]
    fn shell_supported_multiline_insert_is_automatically_bracketed() {
        let input = b"echo first\necho second";
        let (safe, active) = sanitize_input_chunk(input, false, true);
        let mut expected = Vec::new();
        expected.extend_from_slice(BRACKETED_PASTE_START);
        expected.extend_from_slice(input);
        expected.extend_from_slice(BRACKETED_PASTE_END);
        assert_eq!(safe.as_ref(), expected.as_slice());
        assert!(!active);
    }

    #[test]
    fn explicit_submission_preserves_multiline_bytes() {
        let submitted = b"if true; then\necho ok\nfi\r";
        let (safe, active) = sanitize_input_chunk(submitted, false, false);
        assert_eq!(safe.as_ref(), submitted);
        assert!(!active);
    }

    #[test]
    fn bracketed_paste_preserves_multiline_body_across_writes() {
        let (start, active) = sanitize_input_chunk(BRACKETED_PASTE_START, false, false);
        assert_eq!(start.as_ref(), BRACKETED_PASTE_START);
        assert!(active);

        let body = b"echo first\necho second";
        let (safe_body, active) = sanitize_input_chunk(body, active, false);
        assert_eq!(safe_body.as_ref(), body);
        assert!(active);

        let (end, active) = sanitize_input_chunk(BRACKETED_PASTE_END, active, false);
        assert_eq!(end.as_ref(), BRACKETED_PASTE_END);
        assert!(!active);
    }

    #[test]
    fn ordinary_single_line_input_is_unchanged() {
        let input = b"git status";
        let (safe, active) = sanitize_input_chunk(input, false, false);
        assert_eq!(safe.as_ref(), input);
        assert!(!active);
    }

    #[test]
    fn observes_split_bracketed_paste_mode_sequences() {
        let mut tail = Vec::new();
        let enabled = observe_bracketed_paste_mode(false, &mut tail, b"prompt\x1b[?20");
        assert!(!enabled);
        let enabled = observe_bracketed_paste_mode(enabled, &mut tail, b"04h");
        assert!(enabled);

        let enabled = observe_bracketed_paste_mode(enabled, &mut tail, b"\x1b[?2004l");
        assert!(!enabled);
    }
}
