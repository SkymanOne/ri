//! A program in a pseudo-terminal, with its screen emulated.

use std::ffi::OsString;
use std::io::Write as _;
use std::path::Path;
use std::sync::mpsc::{Receiver, channel};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

/// How long the screen must stay unchanged to count as settled, unless
/// `RI_SETTLE_MS` sets it, as slow CI machines may need.
pub fn settle_time() -> Duration {
    std::env::var("RI_SETTLE_MS")
        .ok()
        .and_then(|value| value.parse().ok())
        .map_or(Duration::from_millis(600), Duration::from_millis)
}
/// The longest wait for the screen to settle.
pub const SETTLE_LIMIT: Duration = Duration::from_secs(15);

struct Screen {
    parser: vt100::Parser,
    /// Everything the program wrote.
    output: Vec<u8>,
    last_output: Instant,
    seen_output: bool,
    /// When the program first wrote anything.
    first_output: Option<Instant>,
}

/// A running program attached to a terminal of a fixed size.
pub struct Pty {
    child: Box<dyn portable_pty::Child + Send + Sync>,
    writer: Arc<Mutex<Box<dyn std::io::Write + Send>>>,
    screen: Arc<Mutex<Screen>>,
    updates: Receiver<()>,
    cols: u16,
    spawned: Instant,
    // Keeps the terminal open while the program runs.
    _master: Box<dyn portable_pty::MasterPty + Send>,
}

/// Replies to the queries in a chunk of output: primary device attributes,
/// and the default and palette colors of a dark theme.
fn answers(output: &[u8]) -> String {
    let text = String::from_utf8_lossy(output);
    let mut reply = String::new();
    let mut rest: &str = &text;
    while let Some(start) = rest.find('\x1b') {
        rest = &rest[start..];
        if let Some(after) = rest.strip_prefix("\x1b[c") {
            reply.push_str("\x1b[?62;22c");
            rest = after;
        } else if let Some(query) = rest
            .strip_prefix("\x1b]")
            .and_then(|body| body.split_once("?\x07"))
        {
            let target = query.0.trim_end_matches(';');
            let color = match target {
                "10" => "d0d0/d0d0/d0d0",
                "11" => "1c1c/1c1c/1c1c",
                _ => "8080/8080/8080",
            };
            reply.push_str(&format!("\x1b]{target};rgb:{color}\x07"));
            rest = query.1;
        } else {
            rest = &rest[1..];
        }
    }
    reply
}

fn io_error(error: impl std::fmt::Display) -> std::io::Error {
    std::io::Error::other(error.to_string())
}

impl Pty {
    /// Starts `executable` with `args` in `cwd`, with exactly `env` and
    /// `TERM=xterm-256color`, on a `cols` by `rows` terminal. With `answer`,
    /// the terminal replies to device attribute and color queries as a dark
    /// xterm does; otherwise it stays silent.
    pub fn spawn(
        executable: &Path,
        args: &[String],
        cwd: &Path,
        env: &[(&str, OsString)],
        (cols, rows): (u16, u16),
        answer: bool,
    ) -> std::io::Result<Pty> {
        let pair = portable_pty::native_pty_system()
            .openpty(portable_pty::PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(io_error)?;
        let mut command = portable_pty::CommandBuilder::new(executable);
        command.args(args);
        command.cwd(cwd);
        command.env_clear();
        for (key, value) in env {
            command.env(key, value);
        }
        command.env("TERM", "xterm-256color");
        let spawned = Instant::now();
        let child = pair.slave.spawn_command(command).map_err(io_error)?;
        drop(pair.slave);
        let mut reader = pair.master.try_clone_reader().map_err(io_error)?;
        let writer = Arc::new(Mutex::new(pair.master.take_writer().map_err(io_error)?));
        let screen = Arc::new(Mutex::new(Screen {
            parser: vt100::Parser::new(rows, cols, 0),
            output: Vec::new(),
            last_output: Instant::now(),
            seen_output: false,
            first_output: None,
        }));
        let (notify, updates) = channel();
        let shared = Arc::clone(&screen);
        let replies = Arc::clone(&writer);
        std::thread::spawn(move || {
            let mut buffer = [0u8; 8192];
            loop {
                match std::io::Read::read(&mut reader, &mut buffer) {
                    Ok(0) | Err(_) => break,
                    Ok(count) => {
                        if answer {
                            let reply = answers(&buffer[..count]);
                            if !reply.is_empty() {
                                let mut writer =
                                    replies.lock().unwrap_or_else(PoisonError::into_inner);
                                let _ = writer.write_all(reply.as_bytes());
                                let _ = writer.flush();
                            }
                        }
                        {
                            let mut screen = shared.lock().unwrap_or_else(PoisonError::into_inner);
                            screen.parser.process(&buffer[..count]);
                            screen.output.extend_from_slice(&buffer[..count]);
                            let now = Instant::now();
                            screen.last_output = now;
                            screen.seen_output = true;
                            screen.first_output.get_or_insert(now);
                        }
                        if notify.send(()).is_err() {
                            break;
                        }
                    }
                }
            }
        });
        Ok(Pty {
            child,
            writer,
            screen,
            updates,
            cols,
            spawned,
            _master: pair.master,
        })
    }

    fn lock(&self) -> MutexGuard<'_, Screen> {
        self.screen.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// How long after its start the program first wrote output, if it has.
    pub fn first_output(&self) -> Option<Duration> {
        self.lock()
            .first_output
            .map(|at| at.duration_since(self.spawned))
    }

    /// The program's process id.
    pub fn pid(&self) -> Option<u32> {
        self.child.process_id()
    }

    /// Sends input as one write.
    pub fn write(&mut self, input: &str) -> std::io::Result<()> {
        let mut writer = self.writer.lock().unwrap_or_else(PoisonError::into_inner);
        writer.write_all(input.as_bytes())?;
        writer.flush()
    }

    /// The screen, one string per row.
    pub fn rows(&self) -> Vec<String> {
        self.lock().parser.screen().rows(0, self.cols).collect()
    }

    /// The OSC 9;4 progress sequences the program wrote so far, in order,
    /// such as `"9;4;3"`.
    pub fn progress(&self) -> Vec<String> {
        let screen = self.lock();
        let text = String::from_utf8_lossy(&screen.output);
        text.split("\x1b]")
            .skip(1)
            .filter_map(|rest| rest.split_once('\x07').map(|(body, _)| body))
            .filter(|body| body.starts_with("9;4;"))
            .map(str::to_owned)
            .collect()
    }

    /// Waits until the screen has been quiet for [`settle_time`], counting from
    /// now or the last output, whichever is later; at most [`SETTLE_LIMIT`].
    pub fn settle(&self) {
        let quiet = settle_time();
        let started = Instant::now();
        loop {
            std::thread::sleep(Duration::from_millis(50));
            let (last, seen) = {
                let screen = self.lock();
                (screen.last_output.max(started), screen.seen_output)
            };
            if (seen && last.elapsed() >= quiet) || started.elapsed() >= SETTLE_LIMIT {
                break;
            }
        }
    }

    /// Waits until `done` holds for the screen rows; the time it took, or
    /// `None` after `limit`.
    pub fn wait_for(&self, limit: Duration, done: impl Fn(&[String]) -> bool) -> Option<Duration> {
        let started = Instant::now();
        loop {
            if done(&self.rows()) {
                return Some(started.elapsed());
            }
            let left = limit.checked_sub(started.elapsed())?;
            let _ = self
                .updates
                .recv_timeout(left.min(Duration::from_millis(5)));
        }
    }

    /// The exit code, killing the program first if it still runs (-1).
    pub fn finish(mut self) -> std::io::Result<i32> {
        Ok(match self.child.try_wait()? {
            Some(status) => status.exit_code() as i32,
            None => {
                let _ = self.child.kill();
                let _ = self.child.wait();
                -1
            }
        })
    }
}
