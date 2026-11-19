use std::io::{self, Write};

pub fn prompt(label: &str, secret: bool) -> Result<String, String> {
    eprint!("{label}");
    io::stderr().flush().map_err(|e| e.to_string())?;
    #[cfg(unix)]
    let guard = if secret { EchoGuard::disable()? } else { None };
    #[cfg(not(unix))]
    if secret {
        return Err("此平台暂不支持隐藏密钥输入".into());
    }
    let mut value = String::new();
    let read = io::stdin()
        .read_line(&mut value)
        .map_err(|e| e.to_string())?;
    #[cfg(unix)]
    drop(guard);
    if secret {
        eprintln!();
    }
    if read == 0 {
        return Err("输入已结束".into());
    }
    Ok(value.trim().to_owned())
}

#[cfg(unix)]
struct EchoGuard(libc::termios);
#[cfg(unix)]
impl EchoGuard {
    fn disable() -> Result<Option<Self>, String> {
        if unsafe { libc::isatty(libc::STDIN_FILENO) } != 1 {
            return Ok(None);
        }
        let mut state = std::mem::MaybeUninit::<libc::termios>::uninit();
        if unsafe { libc::tcgetattr(libc::STDIN_FILENO, state.as_mut_ptr()) } != 0 {
            return Err("无法关闭终端回显".into());
        }
        let old = unsafe { state.assume_init() };
        let mut new = old;
        new.c_lflag &= !libc::ECHO;
        if unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &new) } != 0 {
            return Err("无法关闭终端回显".into());
        }
        Ok(Some(Self(old)))
    }
}
#[cfg(unix)]
impl Drop for EchoGuard {
    fn drop(&mut self) {
        unsafe {
            libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &self.0);
        }
    }
}
