//! The `application/vnd.amazon.eventstream` framing of AWS streaming
//! responses: length-prefixed messages with typed headers and CRC32
//! checksums, as `@smithy/eventstream-codec` reads them.

use indexmap::IndexMap;

/// One decoded message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Message {
    /// String-valued headers, such as `:event-type`; other header types are
    /// kept as their text form.
    pub headers: IndexMap<String, String>,
    /// The payload.
    pub payload: Vec<u8>,
}

impl Message {
    /// A header's value.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).map(String::as_str)
    }
}

/// Splits a byte stream into messages.
#[derive(Debug, Default)]
pub struct Decoder {
    buffer: Vec<u8>,
}

fn u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_be_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

impl Decoder {
    /// Adds received bytes.
    pub fn push(&mut self, bytes: &[u8]) {
        self.buffer.extend_from_slice(bytes);
    }

    /// Whether bytes of an unfinished message remain.
    pub fn has_partial(&self) -> bool {
        !self.buffer.is_empty()
    }

    /// The next complete message, if one has arrived.
    pub fn next_message(&mut self) -> Result<Option<Message>, String> {
        if self.buffer.len() < 12 {
            return Ok(None);
        }
        let total = u32_at(&self.buffer, 0) as usize;
        let headers_len = u32_at(&self.buffer, 4) as usize;
        if total < 16 || headers_len > total - 16 {
            return Err(format!("Invalid event stream message length: {total}"));
        }
        if self.buffer.len() < total {
            return Ok(None);
        }
        let message: Vec<u8> = self.buffer.drain(..total).collect();
        let prelude_crc = u32_at(&message, 8);
        if crc32fast::hash(&message[..8]) != prelude_crc {
            return Err(format!(
                "The prelude checksum specified in the message ({prelude_crc}) does not match the calculated CRC32 checksum ({})",
                crc32fast::hash(&message[..8])
            ));
        }
        let message_crc = u32_at(&message, total - 4);
        if crc32fast::hash(&message[..total - 4]) != message_crc {
            return Err(format!(
                "The message checksum ({}) did not match the expected value of {message_crc}",
                crc32fast::hash(&message[..total - 4])
            ));
        }
        let headers = parse_headers(&message[12..12 + headers_len])?;
        Ok(Some(Message {
            headers,
            payload: message[12 + headers_len..total - 4].to_vec(),
        }))
    }
}

fn parse_headers(mut bytes: &[u8]) -> Result<IndexMap<String, String>, String> {
    let invalid = || "Invalid event stream header".to_owned();
    let mut headers = IndexMap::new();
    while !bytes.is_empty() {
        let name_len = usize::from(bytes[0]);
        let name = bytes.get(1..1 + name_len).ok_or_else(invalid)?;
        let name = String::from_utf8_lossy(name).into_owned();
        bytes = &bytes[1 + name_len..];
        let kind = *bytes.first().ok_or_else(invalid)?;
        bytes = &bytes[1..];
        let fixed = |len: usize, bytes: &mut &[u8]| -> Result<Vec<u8>, String> {
            let value = bytes.get(..len).ok_or_else(invalid)?.to_vec();
            *bytes = &bytes[len..];
            Ok(value)
        };
        let value = match kind {
            0 => "true".to_owned(),
            1 => "false".to_owned(),
            2 => (fixed(1, &mut bytes)?[0] as i8).to_string(),
            3 => i16::from_be_bytes(fixed(2, &mut bytes)?.try_into().map_err(|_| invalid())?)
                .to_string(),
            4 => i32::from_be_bytes(fixed(4, &mut bytes)?.try_into().map_err(|_| invalid())?)
                .to_string(),
            5 | 8 => i64::from_be_bytes(fixed(8, &mut bytes)?.try_into().map_err(|_| invalid())?)
                .to_string(),
            6 | 7 => {
                let len = bytes.get(..2).ok_or_else(invalid)?;
                let len = usize::from(u16::from_be_bytes([len[0], len[1]]));
                bytes = &bytes[2..];
                String::from_utf8_lossy(&fixed(len, &mut bytes)?).into_owned()
            }
            9 => super::sigv4::hex(&fixed(16, &mut bytes)?),
            _ => return Err(format!("Unrecognized event stream header type: {kind}")),
        };
        headers.insert(name, value);
    }
    Ok(headers)
}

/// Encodes a message with string headers, for tests and mock servers.
pub fn encode(headers: &[(&str, &str)], payload: &[u8]) -> Vec<u8> {
    let mut header_bytes = Vec::new();
    for (name, value) in headers {
        header_bytes.push(name.len() as u8);
        header_bytes.extend_from_slice(name.as_bytes());
        header_bytes.push(7);
        header_bytes.extend_from_slice(&(value.len() as u16).to_be_bytes());
        header_bytes.extend_from_slice(value.as_bytes());
    }
    let total = (16 + header_bytes.len() + payload.len()) as u32;
    let mut message = Vec::new();
    message.extend_from_slice(&total.to_be_bytes());
    message.extend_from_slice(&(header_bytes.len() as u32).to_be_bytes());
    let prelude_crc = crc32fast::hash(&message);
    message.extend_from_slice(&prelude_crc.to_be_bytes());
    message.extend_from_slice(&header_bytes);
    message.extend_from_slice(payload);
    let crc = crc32fast::hash(&message);
    message.extend_from_slice(&crc.to_be_bytes());
    message
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_split_messages_and_rejects_corruption() {
        let first = encode(
            &[(":event-type", "messageStart")],
            br#"{"role":"assistant"}"#,
        );
        let second = encode(&[(":message-type", "exception")], b"{}");
        let mut decoder = Decoder::default();
        let mut stream = first.clone();
        stream.extend_from_slice(&second);
        decoder.push(&stream[..7]);
        assert_eq!(decoder.next_message(), Ok(None));
        decoder.push(&stream[7..]);
        let message = decoder.next_message().unwrap().unwrap();
        assert_eq!(message.header(":event-type"), Some("messageStart"));
        assert_eq!(message.payload, br#"{"role":"assistant"}"#);
        assert_eq!(
            decoder.next_message().unwrap().unwrap().header(":message-type"),
            Some("exception")
        );
        assert!(!decoder.has_partial());
        let mut corrupt = first;
        let last = corrupt.len() - 5;
        corrupt[last] ^= 1;
        decoder.push(&corrupt);
        assert!(decoder.next_message().unwrap_err().contains("message checksum"));
    }
}
