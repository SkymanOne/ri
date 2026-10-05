//! The process terminal: raw mode, size, keyboard protocol negotiation and the
//! color query.
//!
//! Port of `packages/tui/src/terminal.ts` and the color query of
//! `packages/tui/src/tui.ts` in pi `v1.0.0`.

/// Requests Kitty keyboard flags 7 (disambiguate, event types, alternate
/// keys), queries them, and asks for device attributes as a sentinel.
pub const KEYBOARD_QUERY: &str = "\x1b[>7u\x1b[?u\x1b[c";
/// Pops the Kitty keyboard flags.
pub const KITTY_DISABLE: &str = "\x1b[<u";
/// Enables xterm `modifyOtherKeys` mode 2.
pub const MODIFY_OTHER_KEYS_ENABLE: &str = "\x1b[>4;2m";
/// Disables `modifyOtherKeys`.
pub const MODIFY_OTHER_KEYS_DISABLE: &str = "\x1b[>4;0m";
/// Enables bracketed paste.
pub const BRACKETED_PASTE_ENABLE: &str = "\x1b[?2004h";
/// Disables bracketed paste.
pub const BRACKETED_PASTE_DISABLE: &str = "\x1b[?2004l";

const PALETTE_SIZE: usize = 16;

/// Default colors, palette colors 0 to 15 and a device attributes request
/// that marks the end of the replies.
pub fn color_query() -> String {
    let mut query = String::from("\x1b]10;?\x07\x1b]11;?\x07");
    for index in 0..PALETTE_SIZE {
        query.push_str(&format!("\x1b]4;{index};?\x07"));
    }
    query.push_str("\x1b[c");
    query
}

fn is_device_attributes(sequence: &str) -> bool {
    sequence
        .strip_prefix("\x1b[?")
        .and_then(|rest| rest.strip_suffix('c'))
        .is_some_and(|body| body.bytes().all(|b| b.is_ascii_digit() || b == b';'))
}

fn kitty_flags(sequence: &str) -> Option<u32> {
    let body = sequence.strip_prefix("\x1b[?")?.strip_suffix('u')?;
    (!body.is_empty() && body.bytes().all(|b| b.is_ascii_digit()))
        .then(|| body.parse().ok())
        .flatten()
}

fn is_negotiation_prefix(sequence: &str) -> bool {
    sequence == "\x1b["
        || sequence
            .strip_prefix("\x1b[?")
            .is_some_and(|body| body.bytes().all(|b| b.is_ascii_digit() || b == b';'))
}

/// What became of an input sequence.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Filtered {
    /// Keys for the application, in order.
    Forward(Vec<String>),
    /// Held until the rest of a split reply arrives or [`KeyboardProtocol::flush`].
    Pending,
}

/// The Kitty keyboard protocol negotiation, with `modifyOtherKeys` as the
/// fallback when the terminal answers device attributes first.
#[derive(Debug, Default)]
pub struct KeyboardProtocol {
    pending_device_attributes: u32,
    buffer: String,
    pushed: bool,
    /// The Kitty protocol is active.
    pub kitty: bool,
    /// `modifyOtherKeys` is active.
    pub modify_other_keys: bool,
}

impl KeyboardProtocol {
    /// The query to write at startup.
    pub fn query(&mut self) -> &'static str {
        self.pushed = true;
        self.pending_device_attributes += 1;
        self.buffer.clear();
        KEYBOARD_QUERY
    }

    fn enable_modify_other_keys(&mut self, write: &mut String) {
        if !self.kitty && !self.modify_other_keys {
            write.push_str(MODIFY_OTHER_KEYS_ENABLE);
            self.modify_other_keys = true;
        }
    }

    /// Takes one input sequence. Negotiation replies are consumed; anything to
    /// send to the terminal is appended to `write`.
    pub fn filter(&mut self, sequence: &str, write: &mut String) -> Filtered {
        let mut forward = Vec::new();
        let mut candidate = sequence.to_owned();
        if !self.buffer.is_empty() {
            let buffered = format!("{}{sequence}", self.buffer);
            if kitty_flags(&buffered).is_some() || is_device_attributes(&buffered) {
                self.buffer.clear();
                candidate = buffered;
            } else if is_negotiation_prefix(&buffered) {
                self.buffer = buffered;
                return Filtered::Pending;
            } else {
                forward.push(std::mem::take(&mut self.buffer));
            }
        }
        if let Some(flags) = kitty_flags(&candidate) {
            if flags != 0 {
                if self.modify_other_keys {
                    write.push_str(MODIFY_OTHER_KEYS_DISABLE);
                    self.modify_other_keys = false;
                }
                self.kitty = true;
            } else {
                self.enable_modify_other_keys(write);
            }
            return Filtered::Forward(forward);
        }
        if is_device_attributes(&candidate) {
            if self.pending_device_attributes == 0 {
                forward.push(candidate);
                return Filtered::Forward(forward);
            }
            self.pending_device_attributes -= 1;
            self.enable_modify_other_keys(write);
            return Filtered::Forward(forward);
        }
        if is_negotiation_prefix(&candidate) && forward.is_empty() {
            self.buffer = candidate;
            return Filtered::Pending;
        }
        forward.push(candidate);
        Filtered::Forward(forward)
    }

    /// Releases a held partial reply as input, after its fragment timeout.
    pub fn flush(&mut self) -> Option<String> {
        (!self.buffer.is_empty()).then(|| std::mem::take(&mut self.buffer))
    }

    /// The bytes that undo the negotiated modes.
    pub fn disable(&mut self) -> String {
        let mut out = String::new();
        if self.pushed || self.kitty {
            out.push_str(KITTY_DISABLE);
        }
        if self.modify_other_keys {
            out.push_str(MODIFY_OTHER_KEYS_DISABLE);
        }
        self.pushed = false;
        self.kitty = false;
        self.modify_other_keys = false;
        out
    }
}

/// Colors the terminal reported.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TerminalColors {
    /// Default foreground.
    pub foreground: Option<[f64; 3]>,
    /// Default background.
    pub background: Option<[f64; 3]>,
    /// ANSI colors 0 to 15, when all were reported.
    pub palette: Option<Vec<[f64; 3]>>,
}

fn hex_channel(channel: &str) -> Option<f64> {
    if channel.is_empty() || channel.len() > 8 || !channel.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let max = 16f64.powi(channel.len() as i32) - 1.0;
    let value = u64::from_str_radix(channel, 16).ok()? as f64;
    Some(yapi_types::js::round((value / max) * 255.0))
}

fn osc_color(value: &str) -> Option<[f64; 3]> {
    let value = value.trim();
    if let Some(hex) = value.strip_prefix('#') {
        let channels: Vec<&str> = match hex.len() {
            6 => vec![&hex[0..2], &hex[2..4], &hex[4..6]],
            12 => vec![&hex[0..4], &hex[4..8], &hex[8..12]],
            _ => return None,
        };
        let parsed: Vec<f64> = channels
            .into_iter()
            .map(hex_channel)
            .collect::<Option<_>>()?;
        return Some([parsed[0], parsed[1], parsed[2]]);
    }
    let lower = value.to_ascii_lowercase();
    let rest = lower
        .strip_prefix("rgba:")
        .or_else(|| lower.strip_prefix("rgb:"))
        .unwrap_or(&lower);
    let mut parts = rest.split('/');
    let (r, g, b) = (parts.next()?, parts.next()?, parts.next()?);
    Some([hex_channel(r)?, hex_channel(g)?, hex_channel(b)?])
}

/// Which color an OSC reply reports.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColorTarget {
    /// OSC 10.
    Foreground,
    /// OSC 11.
    Background,
    /// OSC 4 with this index.
    Palette(usize),
}

/// Parses an OSC 10, 11 or 4 reply; the color is `None` when unparseable.
pub fn parse_osc_color(sequence: &str) -> Option<(ColorTarget, Option<[f64; 3]>)> {
    let body = sequence.strip_prefix("\x1b]")?;
    let body = body
        .strip_suffix('\x07')
        .or_else(|| body.strip_suffix("\x1b\\"))?;
    if body.contains(['\x07', '\x1b']) {
        return None;
    }
    let (target, value) = if let Some(value) = body.strip_prefix("10;") {
        (ColorTarget::Foreground, value)
    } else if let Some(value) = body.strip_prefix("11;") {
        (ColorTarget::Background, value)
    } else {
        let rest = body.strip_prefix("4;")?;
        let (index, value) = rest.split_once(';')?;
        if index.is_empty() || index.len() > 3 || !index.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        (ColorTarget::Palette(index.parse().ok()?), value)
    };
    Some((target, osc_color(value)))
}

/// Collects the replies to [`color_query`].
#[derive(Debug, Default)]
pub struct ColorQuery {
    colors: TerminalColors,
    palette: Vec<Option<[f64; 3]>>,
    replied: Vec<ColorTarget>,
    done: bool,
}

impl ColorQuery {
    /// A query awaiting replies.
    pub fn new() -> ColorQuery {
        ColorQuery {
            palette: vec![None; PALETTE_SIZE],
            ..ColorQuery::default()
        }
    }

    /// Whether every reply, or the closing device attributes, arrived.
    pub fn is_done(&self) -> bool {
        self.done
    }

    /// Takes a sequence; returns whether it was a reply to this query.
    pub fn consume(&mut self, sequence: &str) -> bool {
        if self.done {
            return false;
        }
        if is_device_attributes(sequence) {
            self.done = true;
            return true;
        }
        let Some((target, rgb)) = parse_osc_color(sequence) else {
            return false;
        };
        if self.replied.contains(&target) {
            return true;
        }
        self.replied.push(target);
        match target {
            ColorTarget::Foreground => self.colors.foreground = rgb,
            ColorTarget::Background => self.colors.background = rgb,
            ColorTarget::Palette(index) if index < PALETTE_SIZE => self.palette[index] = rgb,
            ColorTarget::Palette(_) => {}
        }
        if self.replied.len() == PALETTE_SIZE + 2 {
            self.done = true;
        }
        true
    }

    /// The colors reported so far.
    pub fn colors(&self) -> TerminalColors {
        let mut colors = self.colors.clone();
        if self.palette.iter().all(Option::is_some) {
            colors.palette = Some(self.palette.iter().flatten().copied().collect());
        }
        colors
    }
}

/// Raw mode on the controlling terminal, restored on drop.
#[cfg(unix)]
pub struct RawMode {
    original: rustix::termios::Termios,
}

#[cfg(unix)]
impl RawMode {
    /// Puts stdin into raw mode as libuv does: no echo, no line editing, no
    /// signal keys, output processing kept.
    pub fn enable() -> std::io::Result<RawMode> {
        let original = rustix::termios::tcgetattr(rustix::stdio::stdin())?;
        let mode = RawMode { original };
        mode.reenable()?;
        Ok(mode)
    }

    /// Puts stdin back into raw mode after [`RawMode::restore`].
    pub fn reenable(&self) -> std::io::Result<()> {
        use rustix::termios::{
            ControlModes, InputModes, LocalModes, OptionalActions, SpecialCodeIndex, tcsetattr,
        };
        let mut raw = self.original.clone();
        raw.input_modes -= InputModes::BRKINT
            | InputModes::ICRNL
            | InputModes::INPCK
            | InputModes::ISTRIP
            | InputModes::IXON;
        raw.control_modes |= ControlModes::CS8;
        raw.local_modes -=
            LocalModes::ECHO | LocalModes::ICANON | LocalModes::IEXTEN | LocalModes::ISIG;
        raw.special_codes[SpecialCodeIndex::VMIN] = 1;
        raw.special_codes[SpecialCodeIndex::VTIME] = 0;
        tcsetattr(rustix::stdio::stdin(), OptionalActions::Now, &raw)?;
        Ok(())
    }

    /// Restores the original mode.
    pub fn restore(&self) {
        let _ = rustix::termios::tcsetattr(
            rustix::stdio::stdin(),
            rustix::termios::OptionalActions::Now,
            &self.original,
        );
    }
}

#[cfg(unix)]
impl Drop for RawMode {
    fn drop(&mut self) {
        self.restore();
    }
}

/// Stops the process group, as the terminal's suspend key would; returns once
/// the group is continued.
#[cfg(unix)]
pub fn suspend() {
    let _ = rustix::process::kill_current_process_group(rustix::process::Signal::TSTP);
}

/// Whether stdin has input within `timeout`.
#[cfg(unix)]
pub fn stdin_ready(timeout: std::time::Duration) -> bool {
    use rustix::event::{PollFd, PollFlags, Timespec, poll};
    let stdin = rustix::stdio::stdin();
    let mut fds = [PollFd::new(&stdin, PollFlags::IN)];
    let timeout = Timespec {
        tv_sec: timeout.as_secs() as _,
        tv_nsec: timeout.subsec_nanos() as _,
    };
    poll(&mut fds, Some(&timeout)).is_ok_and(|ready| ready > 0)
}

/// The terminal size as (columns, rows): the window size of stdout, else
/// `COLUMNS` and `LINES`, else 80 by 24.
pub fn size() -> (usize, usize) {
    #[cfg(unix)]
    if let Ok(size) = rustix::termios::tcgetwinsize(rustix::stdio::stdout())
        && size.ws_col > 0
        && size.ws_row > 0
    {
        return (usize::from(size.ws_col), usize::from(size.ws_row));
    }
    let env = |name: &str, default: usize| {
        std::env::var(name)
            .ok()
            .and_then(|value| value.parse().ok())
            .filter(|value| *value > 0)
            .unwrap_or(default)
    };
    (env("COLUMNS", 80), env("LINES", 24))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn negotiates_kitty_or_falls_back() {
        let mut protocol = KeyboardProtocol::default();
        assert_eq!(protocol.query(), KEYBOARD_QUERY);
        let mut write = String::new();
        assert_eq!(
            protocol.filter("\x1b[?7u", &mut write),
            Filtered::Forward(vec![])
        );
        assert!(protocol.kitty);
        assert_eq!(
            protocol.filter("\x1b[?62;22c", &mut write),
            Filtered::Forward(vec![])
        );
        assert!(write.is_empty());
        assert_eq!(
            protocol.filter("\x1b[?62;22c", &mut write),
            Filtered::Forward(vec!["\x1b[?62;22c".into()])
        );

        let mut legacy = KeyboardProtocol::default();
        legacy.query();
        assert_eq!(legacy.filter("\x1b[?", &mut write), Filtered::Pending);
        assert_eq!(legacy.filter("62c", &mut write), Filtered::Forward(vec![]));
        assert!(legacy.modify_other_keys);
        assert_eq!(write, MODIFY_OTHER_KEYS_ENABLE);
        assert_eq!(
            legacy.filter("a", &mut write),
            Filtered::Forward(vec!["a".into()])
        );
        assert_eq!(
            legacy.disable(),
            format!("{KITTY_DISABLE}{MODIFY_OTHER_KEYS_DISABLE}")
        );
    }

    #[test]
    fn collects_color_replies() {
        let mut query = ColorQuery::new();
        assert!(query.consume("\x1b]11;rgb:1e1e/1e1e/2e2e\x1b\\"));
        assert!(query.consume("\x1b]10;#cdd6f4\x07"));
        assert!(!query.consume("x"));
        for index in 0..16 {
            assert!(query.consume(&format!("\x1b]4;{index};rgb:ff/00/00\x07")));
        }
        assert!(query.is_done());
        let colors = query.colors();
        assert_eq!(colors.background, Some([30.0, 30.0, 46.0]));
        assert_eq!(colors.foreground, Some([205.0, 214.0, 244.0]));
        assert_eq!(colors.palette.map(|p| p.len()), Some(16));
        let mut silent = ColorQuery::new();
        assert!(silent.consume("\x1b[?62;22c"));
        assert!(silent.is_done());
        assert_eq!(silent.colors(), TerminalColors::default());
    }
}
