use nix::libc;
use nix::pty::{openpty, OpenptyResult};
use nix::sys::signal::{self, Signal};
use nix::unistd::{self, ForkResult, Pid};
use std::ffi::CString;
use std::io::{self, Read as _, Write as _};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::io::RawFd;
use std::sync::mpsc;
use gtk4::glib;

enum PtyMsg {
    Data(Vec<u8>),
    Exit(i32),
}

pub struct OwnedPty {
    master: OwnedFd,
    pid: Pid,
}

impl OwnedPty {
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
                    master,
                    pid: child,
                })
            }
            Err(e) => Err(io::Error::new(io::ErrorKind::Other, e)),
        }
    }

    pub fn master_fd(&self) -> RawFd {
        self.master.as_raw_fd()
    }

    pub fn pid(&self) -> Pid {
        self.pid
    }

    pub fn pid_i32(&self) -> i32 {
        self.pid.as_raw()
    }

    pub fn write_bytes(&self, data: &[u8]) {
        let fd = self.master.as_raw_fd();
        let mut file = unsafe { std::fs::File::from_raw_fd(fd) };
        let _ = file.write_all(data);
        // Don't close the fd — leak it back out of File
        std::mem::forget(file);
    }

    pub fn resize(&self, cols: u16, rows: u16) {
        let ws = libc::winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        unsafe {
            libc::ioctl(self.master.as_raw_fd(), libc::TIOCSWINSZ, &ws);
        }
    }

    pub fn kill(&self) {
        let _ = signal::kill(Pid::from_raw(-self.pid.as_raw()), Signal::SIGHUP);
    }

    /// Start an async reader: spawns a background thread to read PTY output
    /// and delivers chunks to `callback` on the GLib main thread via idle_add_local.
    /// When the child exits, sends `PtyMsg::Exit(code)` so `on_exit` is called on the main thread.
    pub fn start_reader<F, E>(&self, mut callback: F, on_exit: E)
    where
        F: FnMut(Vec<u8>) + 'static,
        E: FnOnce(i32) + 'static,
    {
        let fd = self.master.as_raw_fd();
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
            let exit_code = match nix::sys::wait::waitpid(child_pid, None) {
                Ok(nix::sys::wait::WaitStatus::Exited(_, code)) => code,
                Ok(nix::sys::wait::WaitStatus::Signaled(_, sig, _)) => 128 + sig as i32,
                _ => 1,
            };
            let _ = tx.send(PtyMsg::Exit(exit_code));
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
        self.kill();
    }
}
