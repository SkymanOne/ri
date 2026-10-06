//! Splitting raw terminal input into single key sequences and pastes.
//!
//! Port of `packages/tui/src/stdin-buffer.ts` in pi `v1.0.0`. Reads may end
//! inside an escape sequence; the buffer holds the partial sequence until the
//! rest arrives or the caller's timer, set from [`InputBuffer::timeout`],
//! elapses and [`InputBuffer::flush`] releases it as is.

use std::time::Duration;

const ESC: char = '\x1b';
const PASTE_START: &str = "\x1b[200~";
const PASTE_END: &str = "\x1b[201~";
const SEQUENCE_TIMEOUT: Duration = Duration::from_millis(50);
const ESCAPE_TIMEOUT: Duration = Duration::from_millis(10);
const SSH_ESCAPE_TIMEOUT: Duration = Duration::from_millis(100);

/// One unit of terminal input.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Input {
    /// A key sequence or a single character.
    Key(String),
    /// The text of a bracketed paste, without its markers.
    Paste(String),
}

#[derive(PartialEq, Eq)]
enum Status {
    Complete,
    Incomplete,
}

fn csi_status(data: &str) -> Status {
    let payload = &data[2..];
    let Some(last) = payload.chars().last() else {
        return Status::Incomplete;
    };
    if !('\x40'..='\x7e').contains(&last) {
        return Status::Incomplete;
    }
    if let Some(mouse) = payload.strip_prefix('<') {
        // SGR mouse: `<b;x;y` then `M` or `m`.
        let fields = &mouse[..mouse.len() - last.len_utf8()];
        let parts: Vec<&str> = fields.split(';').collect();
        let numeric =
            |part: &&str| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit());
        return if matches!(last, 'M' | 'm') && parts.len() == 3 && parts.iter().all(numeric) {
            Status::Complete
        } else {
            Status::Incomplete
        };
    }
    Status::Complete
}

fn terminated(data: &str, bell: bool) -> Status {
    if data.ends_with("\x1b\\") || (bell && data.ends_with('\x07')) {
        Status::Complete
    } else {
        Status::Incomplete
    }
}

/// Whether `data`, which starts with ESC, is a whole escape sequence.
fn status(data: &str) -> Status {
    let after = &data[1..];
    let count = after.chars().count();
    if count == 0 {
        return Status::Incomplete;
    }
    if after.starts_with("[M") {
        // X10 mouse: ESC [ M and three bytes.
        return if count >= 5 {
            Status::Complete
        } else {
            Status::Incomplete
        };
    }
    match after.chars().next() {
        Some('[') => csi_status(data),
        Some(']') => terminated(data, true),
        Some('P' | '_') => terminated(data, false),
        Some('O') if count < 2 => Status::Incomplete,
        _ => Status::Complete,
    }
}

/// Splits `buffer` into whole sequences and the incomplete remainder.
fn split(buffer: &str) -> (Vec<String>, String) {
    let mut sequences = Vec::new();
    let mut rest = buffer;
    while let Some(first) = rest.chars().next() {
        if first != ESC {
            sequences.push(first.to_string());
            rest = &rest[first.len_utf8()..];
            continue;
        }
        let mut found = None;
        for (index, c) in rest.char_indices().skip(1) {
            let end = index + c.len_utf8();
            if status(&rest[..end]) == Status::Complete {
                found = Some(end);
                break;
            }
        }
        let Some(end) = found else {
            return (sequences, rest.to_owned());
        };
        // WezTerm sends a Kitty Escape press as a raw ESC followed directly by the
        // release sequence; split them rather than read ESC ESC as Alt+Escape.
        if &rest[..end] == "\x1b\x1b" && rest[end..].starts_with(['[', ']', 'O', 'P', '_']) {
            sequences.push(ESC.to_string());
            rest = &rest[1..];
            continue;
        }
        sequences.push(rest[..end].to_owned());
        rest = &rest[end..];
    }
    (sequences, String::new())
}

/// The code point of an unmodified Kitty CSI-u key at or above space.
fn unmodified_kitty_printable(sequence: &str) -> Option<u32> {
    let body = sequence.strip_prefix("\x1b[")?.strip_suffix('u')?;
    let digits = body
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(body.len());
    let code: u32 = body[..digits].parse().ok()?;
    // Optional `:shifted` (possibly empty), then optional `:base`.
    let mut rest = &body[digits..];
    if let Some(after) = rest.strip_prefix(':') {
        rest = after.trim_start_matches(|c: char| c.is_ascii_digit());
        if let Some(after) = rest.strip_prefix(':') {
            let trimmed = after.trim_start_matches(|c: char| c.is_ascii_digit());
            if trimmed.len() == after.len() {
                return None;
            }
            rest = trimmed;
        }
    }
    (rest.is_empty() && code >= 32).then_some(code)
}

/// Buffers terminal input and yields whole key sequences and pastes.
#[derive(Debug, Default)]
pub struct InputBuffer {
    buffer: String,
    paste: Option<String>,
    pending_kitty: Option<u32>,
    utf8: Vec<u8>,
}

impl InputBuffer {
    /// An empty buffer.
    pub fn new() -> InputBuffer {
        InputBuffer::default()
    }

    /// Feeds raw bytes, which may end inside a UTF-8 character.
    pub fn push(&mut self, bytes: &[u8]) -> Vec<Input> {
        let text = yapi_types::js::decode_utf8_stream(&mut self.utf8, bytes);
        let mut out = Vec::new();
        self.push_str(&text, &mut out);
        out
    }

    /// Feeds decoded text, appending what it completes to `out`.
    pub fn push_str(&mut self, text: &str, out: &mut Vec<Input>) {
        self.buffer.push_str(text);
        if let Some(paste) = &mut self.paste {
            paste.push_str(&self.buffer);
            self.buffer.clear();
            self.finish_paste(out);
            return;
        }
        if let Some(start) = self.buffer.find(PASTE_START) {
            let before = self.buffer[..start].to_owned();
            // A partial sequence just before the paste is dropped, as in pi.
            for sequence in split(&before).0 {
                self.emit(sequence, out);
            }
            self.pending_kitty = None;
            self.paste = Some(self.buffer[start + PASTE_START.len()..].to_owned());
            self.buffer.clear();
            self.finish_paste(out);
            return;
        }
        let (sequences, rest) = split(&self.buffer);
        self.buffer = rest;
        for sequence in sequences {
            self.emit(sequence, out);
        }
    }

    fn finish_paste(&mut self, out: &mut Vec<Input>) {
        let Some(paste) = &self.paste else { return };
        let Some(end) = paste.find(PASTE_END) else {
            return;
        };
        let content = paste[..end].to_owned();
        let rest = paste[end + PASTE_END.len()..].to_owned();
        self.paste = None;
        self.pending_kitty = None;
        out.push(Input::Paste(content));
        if !rest.is_empty() {
            self.push_str(&rest, out);
        }
    }

    fn emit(&mut self, sequence: String, out: &mut Vec<Input>) {
        // Some terminals follow a Kitty CSI-u key with the raw character too.
        let mut chars = sequence.chars();
        if let (Some(c), None) = (chars.next(), chars.next())
            && c.len_utf16() == 1
            && self.pending_kitty == Some(u32::from(c))
        {
            self.pending_kitty = None;
            return;
        }
        self.pending_kitty = unmodified_kitty_printable(&sequence);
        out.push(Input::Key(sequence));
    }

    /// How long to wait for the rest of a buffered partial sequence before
    /// calling [`InputBuffer::flush`]; `None` when nothing is buffered.
    pub fn timeout(&self, escape_timeout: Duration) -> Option<Duration> {
        match self.buffer.as_str() {
            "" => None,
            "\x1b" => Some(escape_timeout),
            _ => Some(SEQUENCE_TIMEOUT),
        }
    }

    /// Releases a buffered partial sequence as one key.
    pub fn flush(&mut self) -> Vec<Input> {
        if self.buffer.is_empty() {
            return Vec::new();
        }
        let sequence = std::mem::take(&mut self.buffer);
        self.pending_kitty = None;
        let mut out = Vec::new();
        self.emit(sequence, &mut out);
        out
    }

    /// Drops all buffered input.
    pub fn clear(&mut self) {
        *self = InputBuffer::default();
    }
}

/// How long a lone ESC waits for a following byte before it reads as Escape:
/// `PI_TUI_ESC_TIMEOUT` milliseconds if set, longer over SSH.
pub fn escape_timeout(env: impl Fn(&str) -> Option<String>) -> Duration {
    if let Some(ms) = env("PI_TUI_ESC_TIMEOUT")
        .and_then(|value| value.trim().parse::<f64>().ok())
        .filter(|ms| ms.is_finite() && *ms > 0.0)
    {
        return Duration::from_secs_f64(ms / 1000.0);
    }
    let set = |name: &str| env(name).is_some_and(|value| !value.is_empty());
    if set("SSH_CONNECTION") || set("SSH_TTY") {
        SSH_ESCAPE_TIMEOUT
    } else {
        ESCAPE_TIMEOUT
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(items: &[&str]) -> Vec<Input> {
        items
            .iter()
            .map(|item| Input::Key((*item).into()))
            .collect()
    }

    #[test]
    fn splits_batched_input() {
        let mut buffer = InputBuffer::new();
        assert_eq!(
            buffer.push(b"ab\x1b[A\x1bOP\x1bx\x1b[<35;20;5m"),
            keys(&["a", "b", "\x1b[A", "\x1bOP", "\x1bx", "\x1b[<35;20;5m"])
        );
    }

    #[test]
    fn holds_partial_sequences() {
        let mut buffer = InputBuffer::new();
        assert!(buffer.push(b"\x1b[<35").is_empty());
        assert_eq!(buffer.timeout(ESCAPE_TIMEOUT), Some(SEQUENCE_TIMEOUT));
        assert_eq!(buffer.push(b";20;5m"), keys(&["\x1b[<35;20;5m"]));
        assert!(buffer.push(b"\x1b").is_empty());
        assert_eq!(buffer.timeout(ESCAPE_TIMEOUT), Some(ESCAPE_TIMEOUT));
        assert_eq!(buffer.flush(), keys(&["\x1b"]));
        assert_eq!(buffer.push("é".as_bytes()[..1].as_ref()), vec![]);
        assert_eq!(buffer.push(&"é".as_bytes()[1..]), keys(&["é"]));
    }

    #[test]
    fn extracts_pastes() {
        let mut buffer = InputBuffer::new();
        assert_eq!(buffer.push(b"x\x1b[200~line\n\x1b[A"), keys(&["x"]));
        assert_eq!(
            buffer.push(b"more\x1b[201~y"),
            vec![
                Input::Paste("line\n\x1b[Amore".into()),
                Input::Key("y".into())
            ]
        );
    }

    #[test]
    fn drops_raw_echo_of_kitty_key() {
        let mut buffer = InputBuffer::new();
        assert_eq!(buffer.push(b"\x1b[97ua"), keys(&["\x1b[97u"]));
        assert_eq!(
            buffer.push(b"\x1b\x1b[27;1:3u"),
            keys(&["\x1b", "\x1b[27;1:3u"])
        );
    }

    #[test]
    fn resolves_escape_timeout() {
        let env = |vars: &'static [(&'static str, &'static str)]| {
            move |name: &str| {
                vars.iter()
                    .find(|(key, _)| *key == name)
                    .map(|(_, value)| (*value).to_owned())
            }
        };
        assert_eq!(escape_timeout(env(&[])), ESCAPE_TIMEOUT);
        assert_eq!(
            escape_timeout(env(&[("SSH_TTY", "/dev/pts/1")])),
            SSH_ESCAPE_TIMEOUT
        );
        assert_eq!(
            escape_timeout(env(&[("PI_TUI_ESC_TIMEOUT", "25")])),
            Duration::from_millis(25)
        );
    }
}
