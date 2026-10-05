//! Server-sent events decoding, as the provider SDKs pi uses decode them.

/// One server-sent event.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Event {
    /// The `event:` field, if any.
    pub event: Option<String>,
    /// `data:` lines joined by `\n`.
    pub data: String,
}

/// Incremental decoder: feed bytes as they arrive, collect complete events.
///
/// Lines end at `\n`, `\r\n` or `\r`; a blank line dispatches the event. Comments
/// (`:`) and unknown fields are ignored. Bytes of a UTF-8 character split across
/// chunks are kept until the rest arrives.
#[derive(Debug, Default)]
pub struct Decoder {
    pending: Vec<u8>,
    buffer: String,
    event: Option<String>,
    data: Vec<String>,
}

impl Decoder {
    /// Decodes a chunk and returns the events it completes.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<Event> {
        let text = yapi_types::js::decode_utf8_stream(&mut self.pending, chunk);
        self.buffer.push_str(&text);
        self.lines()
    }

    /// Ends the stream: decodes what is left, including an unterminated last line and
    /// an undispatched event.
    pub fn finish(&mut self) -> Vec<Event> {
        if !self.pending.is_empty() {
            let text = String::from_utf8_lossy(&self.pending).into_owned();
            self.pending.clear();
            self.buffer.push_str(&text);
        }
        let mut events = self.lines();
        if !self.buffer.is_empty() {
            let line = std::mem::take(&mut self.buffer);
            events.extend(self.line(&line));
        }
        events.extend(self.flush());
        events
    }

    fn lines(&mut self) -> Vec<Event> {
        let mut events = Vec::new();
        while let Some(index) = self.buffer.find(['\r', '\n']) {
            let mut next = index + 1;
            // A `\r` at the end of the buffer may be the first half of `\r\n`.
            if self.buffer.as_bytes()[index] == b'\r' {
                match self.buffer.as_bytes().get(next) {
                    Some(b'\n') => next += 1,
                    None => break,
                    Some(_) => {}
                }
            }
            let line = self.buffer[..index].to_owned();
            self.buffer.drain(..next);
            events.extend(self.line(&line));
        }
        events
    }

    fn line(&mut self, line: &str) -> Option<Event> {
        if line.is_empty() {
            return self.flush();
        }
        if line.starts_with(':') {
            return None;
        }
        let (field, value) = match line.split_once(':') {
            Some((field, value)) => (field, value.strip_prefix(' ').unwrap_or(value)),
            None => (line, ""),
        };
        match field {
            "event" => self.event = Some(value.to_owned()),
            "data" => self.data.push(value.to_owned()),
            _ => {}
        }
        None
    }

    fn flush(&mut self) -> Option<Event> {
        if self.event.is_none() && self.data.is_empty() {
            return None;
        }
        Some(Event {
            event: self.event.take(),
            data: std::mem::take(&mut self.data).join("\n"),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode(chunks: &[&[u8]]) -> Vec<Event> {
        let mut decoder = Decoder::default();
        let mut events = Vec::new();
        for chunk in chunks {
            events.extend(decoder.push(chunk));
        }
        events.extend(decoder.finish());
        events
    }

    fn event(name: Option<&str>, data: &str) -> Event {
        Event {
            event: name.map(str::to_owned),
            data: data.to_owned(),
        }
    }

    #[test]
    fn splits_events_across_chunks() {
        let events = decode(&[
            b"event: a\nda",
            b"ta: {\"x\":1}\n\nevent: b\r\ndata: 2\r",
            b"\n\r\n",
        ]);
        assert_eq!(
            events,
            [event(Some("a"), "{\"x\":1}"), event(Some("b"), "2")]
        );
    }

    #[test]
    fn joins_data_lines_and_skips_comments() {
        let events = decode(&[b": ping\ndata: one\ndata:two\nid: 3\n\n"]);
        assert_eq!(events, [event(None, "one\ntwo")]);
    }

    #[test]
    fn keeps_split_utf8_and_flushes_at_end() {
        let crab = "data: 🦀".as_bytes();
        let events = decode(&[&crab[..8], &crab[8..]]);
        assert_eq!(events, [event(None, "🦀")]);
    }
}
