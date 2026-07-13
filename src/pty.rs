use gtk4::glib;
use nix::libc;
use nix::pty::{openpty, OpenptyResult};
use nix::unistd::{self, ForkResult, Pid};
use std::ffi::CString;
use std::io::{self, Read as _};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::mpsc;

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

                if let Some(dir) = cwd {
                    let _ = std::env::set_current_dir(dir);
                }
                for (key, val) in env_extra {
                    unsafe { std::env::set_var(key, val) };
                }
                unsafe { std::env::set_var("TERM", "xterm-256color") };

                let c_argv: Vec<CString> = argv.iter().map(|a| CString::new(*a).unwrap()).collect();
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
                })
            }
            Err(error) => Err(io::Error::other(error)),
        }
    }

    pub fn pid_i32(&self) -> i32 {
        self.pid.as_raw()
    }

    pub fn write_bytes(&self, data: &[u8]) {
        if data.is_empty() {
            return;
        }
        if let Err(error) = self.input_tx.send(data.to_vec()) {
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
        let fd = match self
            .master
            .lock()
            .ok()
            .and_then(|guard| guard.as_ref().map(|fd| fd.as_raw_fd()))
        {
            Some(fd) => fd,
            None => return,
        };

        let child_pid = self.pid;
        let (tx, rx) = mpsc::sync_channel::<PtyMsg>(PTY_QUEUE_CAPACITY);

        // Create an eventfd for signaling data availability to the main thread.
        let efd: RawFd = unsafe { libc::eventfd(0, libc::EFD_NONBLOCK | libc::EFD_CLOEXEC) };
        if efd < 0 {
            // Fallback to 1ms polling if eventfd creation fails.
            self.start_reader_polling(fd, child_pid, tx, rx, callback, on_exit);
            return;
        }

        let efd_for_thread = efd;

        std::thread::Builder::new()
            .name("jterm4-pty-reader".to_string())
            .spawn(move || {
                let mut file = unsafe { std::fs::File::from_raw_fd(fd) };
                let mut buf = [0u8; PTY_READ_CHUNK_BYTES];
                loop {
                    match file.read(&mut buf) {
                        Ok(0) | Err(_) => {
                            std::mem::forget(file);
                            break;
                        }
                        Ok(n) => {
                            if tx.send(PtyMsg::Data(buf[..n].to_vec())).is_err() {
                                std::mem::forget(file);
                                return;
                            }
                            signal_eventfd(efd_for_thread);
                        }
                    }
                }

                let max_wait_secs = 5;
                for _ in 0..(max_wait_secs * 10) {
                    match nix::sys::wait::waitpid(
                        child_pid,
                        Some(nix::sys::wait::WaitPidFlag::WNOHANG),
                    ) {
                        Ok(nix::sys::wait::WaitStatus::Exited(_, code)) => {
                            let _ = tx.send(PtyMsg::Exit(code));
                            signal_eventfd(efd_for_thread);
                            return;
                        }
                        Ok(nix::sys::wait::WaitStatus::Signaled(_, sig, _)) => {
                            let _ = tx.send(PtyMsg::Exit(128 + sig as i32));
                            signal_eventfd(efd_for_thread);
                            return;
                        }
                        Err(_) | Ok(_) => {
                            std::thread::sleep(std::time::Duration::from_millis(100));
                        }
                    }
                }

                match nix::sys::wait::waitpid(child_pid, None) {
                    Ok(nix::sys::wait::WaitStatus::Exited(_, code)) => {
                        let _ = tx.send(PtyMsg::Exit(code));
                    }
                    Ok(nix::sys::wait::WaitStatus::Signaled(_, sig, _)) => {
                        let _ = tx.send(PtyMsg::Exit(128 + sig as i32));
                    }
                    _ => {
                        let _ = tx.send(PtyMsg::Exit(1));
                    }
                }
                signal_eventfd(efd_for_thread);
            })
            .expect("failed to spawn PTY reader thread");

        let on_exit = std::cell::Cell::new(Some(on_exit));

        unix_fd_add_local(efd, move || {
            // Drain the eventfd counter.
            let mut val: u64 = 0;
            unsafe {
                libc::read(efd, &mut val as *mut u64 as *mut libc::c_void, 8);
            }

            match rx.try_recv() {
                Ok(PtyMsg::Data(data)) => {
                    callback(data);
                    // The read above consumed an aggregate wakeup counter. Re-arm
                    // the fd so any remaining queued chunk gets a later dispatch.
                    signal_eventfd(efd);
                    true
                }
                Ok(PtyMsg::Exit(code)) => {
                    if let Some(f) = on_exit.take() {
                        f(code);
                    }
                    unsafe {
                        libc::close(efd);
                    }
                    false
                }
                Err(mpsc::TryRecvError::Empty) => true,
                Err(mpsc::TryRecvError::Disconnected) => {
                    unsafe {
                        libc::close(efd);
                    }
                    false
                }
            }
        });
    }

    fn start_reader_polling<F, E>(
        &self,
        fd: RawFd,
        child_pid: Pid,
        tx: mpsc::SyncSender<PtyMsg>,
        rx: mpsc::Receiver<PtyMsg>,
        mut callback: F,
        on_exit: E,
    ) where
        F: FnMut(Vec<u8>) + 'static,
        E: FnOnce(i32) + 'static,
    {
        std::thread::Builder::new()
            .name("jterm4-pty-reader-poll".to_string())
            .spawn(move || {
                let mut file = unsafe { std::fs::File::from_raw_fd(fd) };
                let mut buf = [0u8; PTY_READ_CHUNK_BYTES];
                loop {
                    match file.read(&mut buf) {
                        Ok(0) | Err(_) => {
                            std::mem::forget(file);
                            break;
                        }
                        Ok(n) => {
                            if tx.send(PtyMsg::Data(buf[..n].to_vec())).is_err() {
                                std::mem::forget(file);
                                return;
                            }
                        }
                    }
                }

                let max_wait_secs = 5;
                for _ in 0..(max_wait_secs * 10) {
                    match nix::sys::wait::waitpid(
                        child_pid,
                        Some(nix::sys::wait::WaitPidFlag::WNOHANG),
                    ) {
                        Ok(nix::sys::wait::WaitStatus::Exited(_, code)) => {
                            let _ = tx.send(PtyMsg::Exit(code));
                            return;
                        }
                        Ok(nix::sys::wait::WaitStatus::Signaled(_, sig, _)) => {
                            let _ = tx.send(PtyMsg::Exit(128 + sig as i32));
                            return;
                        }
                        Err(_) | Ok(_) => {
                            std::thread::sleep(std::time::Duration::from_millis(100));
                        }
                    }
                }

                match nix::sys::wait::waitpid(child_pid, None) {
                    Ok(nix::sys::wait::WaitStatus::Exited(_, code)) => {
                        let _ = tx.send(PtyMsg::Exit(code));
                    }
                    Ok(nix::sys::wait::WaitStatus::Signaled(_, sig, _)) => {
                        let _ = tx.send(PtyMsg::Exit(128 + sig as i32));
                    }
                    _ => {
                        let _ = tx.send(PtyMsg::Exit(1));
                    }
                }
            })
            .expect("failed to spawn polling PTY reader thread");

        let on_exit = std::cell::Cell::new(Some(on_exit));
        let rx = std::cell::RefCell::new(rx);

        glib::timeout_add_local(std::time::Duration::from_millis(1), move || {
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
}
