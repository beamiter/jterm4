use nix::libc;
use nix::pty::{openpty, OpenptyResult};
use nix::unistd::{self, ForkResult, Pid};
use std::ffi::CString;
use std::io::{self, Read as _};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::mpsc;
use gtk4::glib;

use crate::state::terminate_terminal_process;

enum PtyMsg {
    Data(Vec<u8>),
    Exit(i32),
}

pub struct OwnedPty {
    master: std::sync::Arc<std::sync::Mutex<Option<OwnedFd>>>,
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
const G_PRIORITY_DEFAULT: i32 = 0;

struct FdWatchData<F: FnMut() -> bool> {
    callback: F,
}

extern "C" fn fd_watch_callback<F: FnMut() -> bool>(
    _fd: i32,
    _condition: u32,
    user_data: *mut std::ffi::c_void,
) -> i32 {
    let data = unsafe { &mut *(user_data as *mut FdWatchData<F>) };
    if (data.callback)() { 1 } else { 0 }
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
            G_PRIORITY_DEFAULT,
            fd,
            G_IO_IN,
            fd_watch_callback::<F>,
            ptr,
            fd_watch_destroy::<F>,
        );
    }
}

impl OwnedPty {
    fn close_master_fd(&self) {
        if let Ok(mut guard) = self.master.lock() {
            guard.take();
        }
    }

    pub fn spawn(
        argv: &[&str],
        cwd: Option<&str>,
        env_extra: &[(&str, &str)],
    ) -> io::Result<Self> {
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

                let c_argv: Vec<CString> = argv
                    .iter()
                    .map(|a| CString::new(*a).unwrap())
                    .collect();
                let _ = unistd::execvp(&c_argv[0], &c_argv);
                std::process::exit(127);
            }
            Ok(ForkResult::Parent { child }) => {
                drop(slave);
                Ok(OwnedPty {
                    master: std::sync::Arc::new(std::sync::Mutex::new(Some(master))),
                    pid: child,
                })
            }
            Err(e) => Err(io::Error::other(e)),
        }
    }

    pub fn pid_i32(&self) -> i32 {
        self.pid.as_raw()
    }

    pub fn write_bytes(&self, data: &[u8]) {
        if let Ok(guard) = self.master.lock() {
            if let Some(fd) = guard.as_ref() {
                let raw = fd.as_raw_fd();
                unsafe {
                    libc::write(raw, data.as_ptr() as *const libc::c_void, data.len());
                }
            }
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

    /// Start an async reader: spawns a background thread reading with 64KB buffer,
    /// delivers data via eventfd-signaled mpsc channel on the GLib main thread.
    pub fn start_reader<F, E>(&self, mut callback: F, on_exit: E)
    where
        F: FnMut(Vec<u8>) + 'static,
        E: FnOnce(i32) + 'static,
    {
        let fd = match self.master.lock().ok().and_then(|guard| {
            guard.as_ref().map(|fd| fd.as_raw_fd())
        }) {
            Some(fd) => fd,
            None => return,
        };

        let child_pid = self.pid;
        let (tx, rx) = mpsc::channel::<PtyMsg>();

        // Create an eventfd for signaling data availability to the main thread
        let efd: RawFd = unsafe { libc::eventfd(0, libc::EFD_NONBLOCK | libc::EFD_CLOEXEC) };
        if efd < 0 {
            // Fallback to 1ms polling if eventfd creation fails
            self.start_reader_polling(fd, child_pid, tx, rx, callback, on_exit);
            return;
        }

        let efd_for_thread = efd;

        std::thread::spawn(move || {
            let mut file = unsafe { std::fs::File::from_raw_fd(fd) };
            let mut buf = [0u8; 65536];
            loop {
                match file.read(&mut buf) {
                    Ok(0) | Err(_) => {
                        std::mem::forget(file);
                        break;
                    }
                    Ok(n) => {
                        if tx.send(PtyMsg::Data(buf[..n].to_vec())).is_err() {
                            std::mem::forget(file);
                            break;
                        }
                        signal_eventfd(efd_for_thread);
                    }
                }
            }

            let max_wait_secs = 5;
            for _ in 0..(max_wait_secs * 10) {
                match nix::sys::wait::waitpid(child_pid, Some(nix::sys::wait::WaitPidFlag::WNOHANG)) {
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
        });

        let on_exit = std::cell::Cell::new(Some(on_exit));

        unix_fd_add_local(efd, move || {
            // Drain the eventfd counter
            let mut val: u64 = 0;
            unsafe {
                libc::read(efd, &mut val as *mut u64 as *mut libc::c_void, 8);
            }

            loop {
                match rx.try_recv() {
                    Ok(PtyMsg::Data(data)) => {
                        callback(data);
                    }
                    Ok(PtyMsg::Exit(code)) => {
                        if let Some(f) = on_exit.take() {
                            f(code);
                        }
                        unsafe { libc::close(efd); }
                        return false; // Remove source
                    }
                    Err(mpsc::TryRecvError::Empty) => {
                        return true; // Keep watching
                    }
                    Err(mpsc::TryRecvError::Disconnected) => {
                        unsafe { libc::close(efd); }
                        return false;
                    }
                }
            }
        });
    }

    fn start_reader_polling<F, E>(
        &self,
        fd: RawFd,
        child_pid: Pid,
        tx: mpsc::Sender<PtyMsg>,
        rx: mpsc::Receiver<PtyMsg>,
        mut callback: F,
        on_exit: E,
    ) where
        F: FnMut(Vec<u8>) + 'static,
        E: FnOnce(i32) + 'static,
    {
        std::thread::spawn(move || {
            let mut file = unsafe { std::fs::File::from_raw_fd(fd) };
            let mut buf = [0u8; 65536];
            loop {
                match file.read(&mut buf) {
                    Ok(0) | Err(_) => {
                        std::mem::forget(file);
                        break;
                    }
                    Ok(n) => {
                        if tx.send(PtyMsg::Data(buf[..n].to_vec())).is_err() {
                            std::mem::forget(file);
                            break;
                        }
                    }
                }
            }

            let max_wait_secs = 5;
            for _ in 0..(max_wait_secs * 10) {
                match nix::sys::wait::waitpid(child_pid, Some(nix::sys::wait::WaitPidFlag::WNOHANG)) {
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
        });

        let on_exit = std::cell::Cell::new(Some(on_exit));
        let rx = std::cell::RefCell::new(rx);

        glib::timeout_add_local(std::time::Duration::from_millis(1), move || {
            loop {
                match rx.borrow().try_recv() {
                    Ok(PtyMsg::Data(data)) => {
                        callback(data);
                    }
                    Ok(PtyMsg::Exit(code)) => {
                        if let Some(f) = on_exit.take() {
                            f(code);
                        }
                        return glib::ControlFlow::Break;
                    }
                    Err(mpsc::TryRecvError::Empty) => {
                        return glib::ControlFlow::Continue;
                    }
                    Err(mpsc::TryRecvError::Disconnected) => {
                        return glib::ControlFlow::Break;
                    }
                }
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
