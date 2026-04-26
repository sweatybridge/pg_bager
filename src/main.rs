mod config;
mod encode;
mod layout;
mod scan;
mod term;

use std::{
    env,
    fs::{File, OpenOptions},
    io::{self, Read, Write},
    os::fd::AsRawFd,
    process::{Command, Stdio},
};

use config::Config;

fn main() {
    if let Err(error) = run() {
        eprintln!("pg_bager: {error}");
        std::process::exit(1);
    }
}

fn run() -> io::Result<()> {
    let config = Config::from_env();
    let protocol = term::Protocol::detect();

    let mut input = Vec::new();
    io::stdin().read_to_end(&mut input)?;
    let processed = layout::process(&input, protocol, &config)?;

    if let Some(command) = config.fallback.as_deref() {
        return write_to_pager(command, &processed.bytes);
    }

    if processed.rewritten {
        if let Some(interactive) = processed.interactive.as_ref() {
            return run_interactive(interactive, protocol).or_else(|_| {
                let mut stdout = io::stdout().lock();
                stdout.write_all(&processed.bytes)?;
                stdout.flush()
            });
        }

        let mut stdout = io::stdout().lock();
        stdout.write_all(&processed.bytes)?;
        stdout.flush()
    } else {
        let command = default_pager_command();
        write_to_pager(&command, &processed.bytes)
    }
}

fn run_interactive(output: &layout::InteractiveOutput, protocol: term::Protocol) -> io::Result<()> {
    if output.rows.is_empty() {
        return Ok(());
    }

    let mut tty = RawTty::open()?;
    let mut stdout = io::stdout().lock();
    let mut row_index = 0usize;

    loop {
        render_interactive_row(&mut stdout, output, row_index, protocol)?;

        match tty.read_key()? {
            Key::Down if row_index + 1 < output.rows.len() => row_index += 1,
            Key::Up if row_index > 0 => row_index -= 1,
            Key::Quit => {
                clear_screen(&mut stdout, protocol)?;
                return stdout.flush();
            }
            _ => {}
        }
    }
}

fn render_interactive_row(
    stdout: &mut impl Write,
    output: &layout::InteractiveOutput,
    row_index: usize,
    protocol: term::Protocol,
) -> io::Result<()> {
    clear_screen(stdout, protocol)?;
    stdout.write_all(&output.prefix)?;
    stdout.write_all(&output.rows[row_index])?;
    stdout.write_all(&output.suffix)?;
    if !output.suffix.ends_with(b"\n") {
        stdout.write_all(b"\n")?;
    }
    write!(
        stdout,
        "\x1b[7m row {}/{} - Up/Down: navigate - q: quit \x1b[0m",
        row_index + 1,
        output.rows.len()
    )?;
    stdout.flush()
}

fn clear_screen(stdout: &mut impl Write, protocol: term::Protocol) -> io::Result<()> {
    if protocol == term::Protocol::Kitty {
        stdout.write_all(b"\x1b_Ga=d,d=A\x1b\\")?;
    }
    stdout.write_all(b"\x1b[2J\x1b[H")
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Key {
    Up,
    Down,
    Quit,
    Other,
}

struct RawTty {
    tty: File,
    original: libc::termios,
}

impl RawTty {
    fn open() -> io::Result<Self> {
        let tty = OpenOptions::new().read(true).write(true).open("/dev/tty")?;
        let fd = tty.as_raw_fd();
        let original = tcgetattr(fd)?;
        let mut raw = original;

        raw.c_iflag &= !(libc::BRKINT | libc::ICRNL | libc::INPCK | libc::ISTRIP | libc::IXON);
        raw.c_cflag |= libc::CS8;
        raw.c_lflag &= !(libc::ECHO | libc::ICANON | libc::IEXTEN);
        raw.c_cc[libc::VMIN] = 1;
        raw.c_cc[libc::VTIME] = 0;

        tcsetattr(fd, &raw)?;
        Ok(Self { tty, original })
    }

    fn read_key(&mut self) -> io::Result<Key> {
        let byte = self.read_byte()?;
        match byte {
            b'q' | b'Q' => Ok(Key::Quit),
            b'\x1b' => self.read_escape_key(),
            _ => Ok(Key::Other),
        }
    }

    fn read_escape_key(&mut self) -> io::Result<Key> {
        if self.poll_byte(100)? != Some(b'[') {
            return Ok(Key::Other);
        }

        match self.poll_byte(100)? {
            Some(b'A') => Ok(Key::Up),
            Some(b'B') => Ok(Key::Down),
            _ => Ok(Key::Other),
        }
    }

    fn read_byte(&mut self) -> io::Result<u8> {
        let mut byte = [0u8; 1];
        self.tty.read_exact(&mut byte)?;
        Ok(byte[0])
    }

    fn poll_byte(&mut self, timeout_ms: i32) -> io::Result<Option<u8>> {
        let mut poll_fd = libc::pollfd {
            fd: self.tty.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let result = unsafe { libc::poll(&mut poll_fd, 1, timeout_ms) };
        if result < 0 {
            return Err(io::Error::last_os_error());
        }
        if result == 0 {
            return Ok(None);
        }
        self.read_byte().map(Some)
    }
}

impl Drop for RawTty {
    fn drop(&mut self) {
        let _ = unsafe {
            libc::tcsetattr(
                self.tty.as_raw_fd(),
                libc::TCSANOW,
                &self.original as *const libc::termios,
            )
        };
    }
}

fn tcgetattr(fd: i32) -> io::Result<libc::termios> {
    let mut termios = std::mem::MaybeUninit::<libc::termios>::uninit();
    let result = unsafe { libc::tcgetattr(fd, termios.as_mut_ptr()) };
    if result == 0 {
        Ok(unsafe { termios.assume_init() })
    } else {
        Err(io::Error::last_os_error())
    }
}

fn tcsetattr(fd: i32, termios: &libc::termios) -> io::Result<()> {
    let result = unsafe { libc::tcsetattr(fd, libc::TCSANOW, termios as *const libc::termios) };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn default_pager_command() -> String {
    env::var("PAGER")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| {
            if command_exists("less") {
                "less -R".to_string()
            } else {
                "cat".to_string()
            }
        })
}

fn command_exists(name: &str) -> bool {
    env::var_os("PATH").is_some_and(|paths| {
        env::split_paths(&paths).any(|path| {
            let candidate = path.join(name);
            candidate.is_file()
        })
    })
}

fn write_to_pager(command: &str, bytes: &[u8]) -> io::Result<()> {
    let mut child = Command::new("sh")
        .arg("-c")
        .arg(command)
        .stdin(Stdio::piped())
        .spawn()?;

    {
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "pager stdin unavailable"))?;
        stdin.write_all(bytes)?;
    }

    let status = child.wait()?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "pager command exited with {status}"
        )))
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::{command_exists, write_to_pager};
    use crate::{config::Config, layout, scan::PNG_MAGIC, term::Protocol};

    #[test]
    fn finds_shell() {
        assert!(command_exists("sh"));
    }

    #[test]
    fn fallback_command_receives_rewritten_stream() {
        let dir = tempfile::tempdir().unwrap();
        let output_path = dir.path().join("pager.out");
        let command = format!("cat > {}", output_path.display());

        let mut input = String::from("thumbnail\n\\x");
        for byte in PNG_MAGIC {
            input.push_str(&format!("{byte:02x}"));
        }
        input.push('\n');

        let config = Config {
            max_row_bytes: 4096,
            max_pixels_w: None,
            max_pixels_h: None,
            disable: false,
            fallback: Some(command.clone()),
        };
        let processed = layout::process(input.as_bytes(), Protocol::Kitty, &config).unwrap();

        write_to_pager(&command, &processed.bytes).unwrap();

        let written = fs::read(output_path).unwrap();
        assert_eq!(written, processed.bytes);
        assert!(String::from_utf8(written)
            .unwrap()
            .contains("\x1b_Gf=100,a=T;"));
    }
}
