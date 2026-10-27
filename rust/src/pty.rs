#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::collections::BTreeSet;
use std::ffi::OsString;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::ffi::{CStr, CString};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::process::ExitStatusExt;
use std::process::Stdio;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::time::Duration;

use anyhow::{Context, Result, ensure};
#[cfg(any(target_os = "linux", target_os = "macos"))]
use tokio::io::AsyncReadExt;
use tokio::io::unix::AsyncFd;
use tokio::process::{Child, Command};

#[cfg(any(target_os = "linux", target_os = "macos"))]
use crate::model::valid_locale_name;
use crate::model::{Size, validate_locale_candidates};

#[cfg(any(target_os = "linux", target_os = "macos"))]
unsafe extern "C" {
    #[link_name = "nl_langinfo_l"]
    fn locale_codeset(item: libc::nl_item, locale: libc::locale_t) -> *mut libc::c_char;
}

struct Utf8Locale {
    name: String,
}

impl Utf8Locale {
    async fn select(candidates: &[String]) -> Result<Self> {
        validate_locale_candidates(candidates)?;
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            let server = ["LC_ALL", "LC_CTYPE", "LANG"]
                .into_iter()
                .filter_map(|key| std::env::var(key).ok());
            let fallback = ["C.UTF-8", "C.utf8", "UTF-8", "en_US.UTF-8", "en_US.utf8"]
                .into_iter()
                .map(str::to_owned);
            let mut seen = BTreeSet::new();
            for name in candidates.iter().cloned().chain(server).chain(fallback) {
                if valid_locale_name(&name) && seen.insert(name.clone()) && locale_is_utf8(&name) {
                    return Ok(Self { name });
                }
            }
            for name in installed_locale_names().await? {
                if valid_locale_name(&name) && seen.insert(name.clone()) && locale_is_utf8(&name) {
                    return Ok(Self { name });
                }
            }
            anyhow::bail!(
                "no usable UTF-8 locale found; generate or install a UTF-8 locale on the server and set LC_ALL, LC_CTYPE, or LANG to its name"
            );
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        anyhow::bail!("UTF-8 locale selection is unsupported on this platform");
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
async fn installed_locale_names() -> Result<Vec<String>> {
    const MAX_OUTPUT: usize = 64 * 1024;
    const MAX_NAMES: usize = 1024;
    let mut child = Command::new("locale")
        .arg("-a")
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .context("cannot discover server locales; install the locale utility or set LC_CTYPE to an installed UTF-8 locale")?;
    let result = tokio::time::timeout(Duration::from_secs(5), async {
        let mut stdout = child.stdout.take().context("locale stdout missing")?;
        let mut bytes = Vec::new();
        (&mut stdout)
            .take((MAX_OUTPUT + 1) as u64)
            .read_to_end(&mut bytes)
            .await?;
        ensure!(
            bytes.len() <= MAX_OUTPUT,
            "server locale list exceeds 64 KiB"
        );
        ensure!(
            child.wait().await?.success(),
            "server locale discovery failed; set LC_CTYPE to an installed UTF-8 locale"
        );
        let listing = std::str::from_utf8(&bytes).context("server locale list is not UTF-8")?;
        let mut names = Vec::new();
        for name in listing.lines() {
            ensure!(
                names.len() < MAX_NAMES,
                "server locale list exceeds 1024 names"
            );
            names.push(name.to_owned());
        }
        Ok::<_, anyhow::Error>(names)
    })
    .await;
    match result {
        Ok(Ok(names)) => Ok(names),
        result => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            match result {
                Ok(Err(error)) => Err(error),
                Err(_) => anyhow::bail!(
                    "server locale discovery timed out; set LC_CTYPE to an installed UTF-8 locale"
                ),
                Ok(Ok(_)) => unreachable!(),
            }
        }
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn locale_is_utf8(name: &str) -> bool {
    let Ok(name) = CString::new(name) else {
        return false;
    };
    let locale =
        unsafe { libc::newlocale(libc::LC_CTYPE_MASK, name.as_ptr(), std::ptr::null_mut()) };
    if locale.is_null() {
        return false;
    }
    let codeset = unsafe { locale_codeset(libc::CODESET, locale) };
    let is_utf8 = if codeset.is_null() {
        false
    } else {
        let codeset = unsafe { CStr::from_ptr(codeset) }.to_bytes();
        codeset.eq_ignore_ascii_case(b"UTF-8") || codeset.eq_ignore_ascii_case(b"UTF8")
    };
    unsafe { libc::freelocale(locale) };
    is_utf8
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn enable_utf8_input(slave: &OwnedFd) -> Result<()> {
    let mut attrs = std::mem::MaybeUninit::<libc::termios>::uninit();
    if unsafe { libc::tcgetattr(slave.as_raw_fd(), attrs.as_mut_ptr()) } == -1 {
        return Err(io::Error::last_os_error()).context("read PTY input modes");
    }
    let mut attrs = unsafe { attrs.assume_init() };
    attrs.c_iflag |= libc::IUTF8;
    if unsafe { libc::tcsetattr(slave.as_raw_fd(), libc::TCSANOW, &attrs) } == -1 {
        return Err(io::Error::last_os_error()).context("enable PTY UTF-8 input");
    }
    Ok(())
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn enable_utf8_input(_slave: &OwnedFd) -> Result<()> {
    Ok(())
}

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
    pub async fn spawn(argv: &[String], size: Size, locale_candidates: &[String]) -> Result<Self> {
        size.validate()?;
        ensure!(
            argv.len() <= 256 && argv.iter().map(String::len).sum::<usize>() <= 128 * 1024,
            "command exceeds limit"
        );
        let locale = Utf8Locale::select(locale_candidates).await?;
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
        enable_utf8_input(&slave)?;
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
            .env_remove("LC_ALL")
            .env("LC_CTYPE", &locale.name)
            .env("LANG", &locale.name)
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
