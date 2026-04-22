use nix::libc;
use nix::pty::{openpty, OpenptyResult};
use nix::unistd::{self, ForkResult, Pid};
use std::ffi::CString;
use std::io::{self, Read as _, Write as _};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::io::RawFd;
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
        let OpenptyResult { master, slave } =
            openpty(None, None).map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;

        match unsafe { unistd::fork() } {
            Ok(ForkResult::Child) => {
                drop(master);
                let slave_fd = slave.as_raw_fd();

                unsafe {
                    libc::setsid();
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
                // TERM must be set for applications that check it
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
            Err(e) => Err(io::Error::new(io::ErrorKind::Other, e)),
        }
    }

    pub fn master_fd(&self) -> Option<RawFd> {
        self.master.lock().ok().and_then(|guard| {
            guard.as_ref().map(|fd| fd.as_raw_fd())
        })
    }

    pub fn pid(&self) -> Pid {
        self.pid
    }

    pub fn pid_i32(&self) -> i32 {
        self.pid.as_raw()
    }

    pub fn write_bytes(&self, data: &[u8]) {
        if let Ok(guard) = self.master.lock() {
            if let Some(fd) = guard.as_ref() {
                let fd = fd.as_raw_fd();
                let mut file = unsafe { std::fs::File::from_raw_fd(fd) };
                let _ = file.write_all(data);
                std::mem::forget(file);
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

    /// Start an async reader: spawns a background thread to read PTY output
    /// and delivers chunks to `callback` on the GLib main thread via idle_add_local.
    /// When the child exits, sends `PtyMsg::Exit(code)` so `on_exit` is called on the main thread.
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
        let rx = std::cell::RefCell::new(rx);

        std::thread::spawn(move || {
            let mut file = unsafe { std::fs::File::from_raw_fd(fd) };
            let mut buf = [0u8; 8192];
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

            // Wait for the child with timeout using non-blocking checks
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
                        // WNOHANG returns Ok(current status) or Err if not found
                        std::thread::sleep(std::time::Duration::from_millis(100));
                    }
                }
            }

            // Final blocking wait (should be quick if process is responsive to SIGHUP)
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
        glib::idle_add_local(move || {
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
                    Err(mpsc::TryRecvError::Empty) => return glib::ControlFlow::Continue,
                    Err(mpsc::TryRecvError::Disconnected) => return glib::ControlFlow::Break,
                }
            }
        });
    }
}

impl Drop for OwnedPty {
    fn drop(&mut self) {
        self.close_master_fd();
        terminate_terminal_process(self.pid.as_raw());
    }
}
