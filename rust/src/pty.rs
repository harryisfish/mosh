use std::ffi::OsString;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::process::ExitStatusExt;
use std::process::Stdio;

use anyhow::{Context, Result, ensure};
use tokio::io::unix::AsyncFd;
use tokio::process::{Child, Command};

use crate::model::Size;

pub struct Pty {
    pub io: AsyncFd<OwnedFd>,
    child: Option<Child>,
    exited: Option<i32>,
}

fn winsize(size: Size) -> libc::winsize {
    libc::winsize {
        ws_row: size.rows,
        ws_col: size.cols,
        ws_xpixel: size.pixel_width.min(u16::MAX as u32) as u16,
        ws_ypixel: size.pixel_height.min(u16::MAX as u32) as u16,
    }
}

impl Pty {
    pub fn spawn(argv: &[String], size: Size) -> Result<Self> {
        size.validate()?;
        ensure!(
            argv.len() <= 256 && argv.iter().map(String::len).sum::<usize>() <= 128 * 1024,
            "command exceeds limit"
        );
        let mut master = -1;
        let mut slave = -1;
        let mut window = winsize(size);
        let result = unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &raw mut window,
            )
        };
        if result == -1 {
            return Err(io::Error::last_os_error()).context("open PTY");
        }
        let master = unsafe { OwnedFd::from_raw_fd(master) };
        let slave = unsafe { OwnedFd::from_raw_fd(slave) };
        for fd in [master.as_raw_fd(), slave.as_raw_fd()] {
            if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } == -1 {
                return Err(io::Error::last_os_error()).context("set PTY close-on-exec");
            }
        }
        let flags = unsafe { libc::fcntl(master.as_raw_fd(), libc::F_GETFL) };
        if flags == -1
            || unsafe { libc::fcntl(master.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) }
                == -1
        {
            return Err(io::Error::last_os_error()).context("set PTY nonblocking");
        }
        let shell = std::env::var_os("SHELL").unwrap_or_else(|| OsString::from("/bin/sh"));
        let mut command = if let Some(program) = argv.first() {
            Command::new(program)
        } else {
            Command::new(&shell)
        };
        if argv.is_empty() {
            let name = std::path::Path::new(&shell)
                .file_name()
                .unwrap_or(shell.as_os_str());
            let mut arg0 = OsString::from("-");
            arg0.push(name);
            command.arg0(arg0);
        } else {
            command.args(&argv[1..]);
        }
        command
            .env("TERM", "xterm-256color")
            .env("COLORTERM", "truecolor")
            .env("TERM_PROGRAM", "mosh-rs");
        command
            .env_remove("KITTY_WINDOW_ID")
            .env_remove("WEZTERM_PANE")
            .env_remove("TMUX");
        command.stdin(Stdio::from(slave.try_clone()?));
        command.stdout(Stdio::from(slave.try_clone()?));
        command.stderr(Stdio::from(slave));
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 || libc::ioctl(0, libc::TIOCSCTTY as _, 0) == -1 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        command.kill_on_drop(true);
        let io = unsafe { AsyncFd::register(master) }?;
        let child = command.spawn().context("start remote command")?;
        Ok(Self {
            io,
            child: Some(child),
            exited: None,
        })
    }

    pub fn read(fd: &OwnedFd, buffer: &mut [u8]) -> io::Result<usize> {
        let count = unsafe { libc::read(fd.as_raw_fd(), buffer.as_mut_ptr().cast(), buffer.len()) };
        if count >= 0 {
            Ok(count as usize)
        } else {
            Err(io::Error::last_os_error())
        }
    }

    pub fn write(fd: &OwnedFd, buffer: &[u8]) -> io::Result<usize> {
        let count = unsafe { libc::write(fd.as_raw_fd(), buffer.as_ptr().cast(), buffer.len()) };
        if count >= 0 {
            Ok(count as usize)
        } else {
            Err(io::Error::last_os_error())
        }
    }

    pub fn resize(&self, size: Size) -> Result<()> {
        size.validate()?;
        if unsafe {
            libc::ioctl(
                self.io.get_ref().as_raw_fd(),
                libc::TIOCSWINSZ as _,
                &winsize(size),
            )
        } == -1
        {
            return Err(io::Error::last_os_error()).context("resize PTY");
        }
        Ok(())
    }

    pub fn echo_enabled(&self) -> bool {
        let mut attrs = std::mem::MaybeUninit::<libc::termios>::uninit();
        if unsafe { libc::tcgetattr(self.io.get_ref().as_raw_fd(), attrs.as_mut_ptr()) } == -1 {
            return false;
        }
        let attrs = unsafe { attrs.assume_init() };
        attrs.c_lflag & libc::ECHO != 0
    }

    pub fn poll_exit(&mut self) -> Result<Option<i32>> {
        if self.exited.is_none()
            && let Some(child) = self.child.as_mut()
            && let Some(status) = child.try_wait()?
        {
            self.exited = Some(
                status
                    .code()
                    .unwrap_or_else(|| 128 + status.signal().unwrap_or(1)),
            );
        }
        Ok(self.exited)
    }

    pub fn close(&mut self) {
        let Some(mut child) = self.child.take() else {
            return;
        };
        if self.exited.is_none() {
            if let Some(pid) = child.id() {
                unsafe {
                    libc::kill(-(pid as i32), libc::SIGHUP);
                }
            }
            let _ = child.start_kill();
        }
    }
}

impl Drop for Pty {
    fn drop(&mut self) {
        self.close();
    }
}
