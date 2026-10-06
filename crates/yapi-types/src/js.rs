//! JavaScript string and number semantics that pi's texts depend on: string
//! lengths and slices count UTF-16 code units, and `toFixed` and `Math.round`
//! round halves up.

/// `text.length`.
pub fn len(text: &str) -> usize {
    text.encode_utf16().count()
}

/// `text.slice(start, end)` with indices in UTF-16 code units, clamped to the
/// text. A surrogate pair cut in half becomes U+FFFD.
pub fn slice(text: &str, start: usize, end: usize) -> String {
    let units: Vec<u16> = text.encode_utf16().collect();
    let end = end.min(units.len());
    let start = start.min(end);
    String::from_utf16_lossy(&units[start..end])
}

/// `TextDecoder.decode(bytes, {stream: true})`: appends `bytes` to `pending`
/// and decodes what is complete. A character cut off at the end stays
/// pending; invalid bytes become U+FFFD.
pub fn decode_utf8_stream(pending: &mut Vec<u8>, bytes: &[u8]) -> String {
    pending.extend_from_slice(bytes);
    let valid = match std::str::from_utf8(pending) {
        Ok(text) => text.len(),
        Err(error) if error.error_len().is_none() => error.valid_up_to(),
        Err(_) => pending.len(),
    };
    let text = String::from_utf8_lossy(&pending[..valid]).into_owned();
    pending.drain(..valid);
    text
}

/// `Math.round(value)`.
pub fn round(value: f64) -> f64 {
    (value + 0.5).floor()
}

/// `value.toFixed(digits)`: the exact value rounded half up.
pub fn to_fixed(value: f64, digits: usize) -> String {
    if !value.is_finite() {
        return value.to_string();
    }
    if value < 0.0 {
        let positive = to_fixed(-value, digits);
        return if positive.bytes().all(|byte| matches!(byte, b'0' | b'.')) {
            positive
        } else {
            format!("-{positive}")
        };
    }
    let wide = format!("{value:.40}");
    let (whole, fraction) = wide.split_once('.').unwrap_or((&wide, ""));
    let mut number: Vec<u8> = whole
        .bytes()
        .chain(fraction.bytes().chain(std::iter::repeat(b'0')).take(digits))
        .collect();
    if fraction
        .as_bytes()
        .get(digits)
        .is_some_and(|digit| *digit >= b'5')
    {
        let mut index = number.len();
        loop {
            if index == 0 {
                number.insert(0, b'1');
                break;
            }
            index -= 1;
            if number[index] == b'9' {
                number[index] = b'0';
            } else {
                number[index] += 1;
                break;
            }
        }
    }
    let text = String::from_utf8_lossy(&number).into_owned();
    if digits == 0 {
        return text;
    }
    let split = text.len() - digits;
    format!("{}.{}", &text[..split], &text[split..])
}

/// Node's code and description for an I/O error of `kind`, such as
/// `("ENOENT", "no such file or directory")`.
pub fn errno(kind: std::io::ErrorKind) -> Option<(&'static str, &'static str)> {
    use std::io::ErrorKind;
    Some(match kind {
        ErrorKind::NotFound => ("ENOENT", "no such file or directory"),
        ErrorKind::PermissionDenied => ("EACCES", "permission denied"),
        ErrorKind::AlreadyExists => ("EEXIST", "file already exists"),
        ErrorKind::IsADirectory => ("EISDIR", "illegal operation on a directory"),
        ErrorKind::NotADirectory => ("ENOTDIR", "not a directory"),
        _ => return None,
    })
}

/// The message Node gives a failed file system call, such as
/// `ENOENT: no such file or directory, access '/a/b'`.
pub fn node_error(err: &std::io::Error, syscall: &str, path: &std::path::Path) -> String {
    let path = path.display();
    match errno(err.kind()) {
        // Node names no path for a read, as `readFile` reports it.
        Some(("EISDIR", text)) if syscall == "read" => format!("EISDIR: {text}, read"),
        Some((code, text)) => format!("{code}: {text}, {syscall} '{path}'"),
        None => format!("{err}, {syscall} '{path}'"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn words_io_errors_as_node() {
        let error = |kind| std::io::Error::from(kind);
        let path = std::path::Path::new("/a/b");
        assert_eq!(
            node_error(&error(std::io::ErrorKind::NotFound), "open", path),
            "ENOENT: no such file or directory, open '/a/b'"
        );
        assert_eq!(
            node_error(&error(std::io::ErrorKind::IsADirectory), "read", path),
            "EISDIR: illegal operation on a directory, read"
        );
        assert_eq!(
            node_error(&error(std::io::ErrorKind::AlreadyExists), "mkdir", path),
            "EEXIST: file already exists, mkdir '/a/b'"
        );
        assert_eq!(errno(std::io::ErrorKind::Other), None);
    }

    #[test]
    fn counts_and_slices_utf16_units() {
        assert_eq!(len("a😀"), 3);
        assert_eq!(slice("a😀b", 0, 3), "a😀");
        assert_eq!(slice("a😀b", 0, 2), "a\u{fffd}");
        assert_eq!(slice("abc", 2, 10), "c");
    }

    #[test]
    fn decodes_utf8_streams() {
        let mut pending = Vec::new();
        let euro = "€".as_bytes();
        assert_eq!(decode_utf8_stream(&mut pending, &euro[..2]), "");
        assert_eq!(decode_utf8_stream(&mut pending, &euro[2..]), "€");
        assert_eq!(decode_utf8_stream(&mut pending, b"a\xffb"), "a\u{fffd}b");
        assert!(pending.is_empty());
    }

    #[test]
    fn rounds_like_javascript() {
        assert_eq!(to_fixed(0.05, 1), "0.1");
        assert_eq!(to_fixed(1.25, 1), "1.3");
        assert_eq!(to_fixed(1.005, 2), "1.00");
        assert_eq!(to_fixed(9.96, 1), "10.0");
        assert_eq!(to_fixed(0.0, 1), "0.0");
        assert_eq!(to_fixed(2.5, 0), "3");
        assert_eq!(to_fixed(-1.25, 1), "-1.3");
        assert_eq!(round(2.5), 3.0);
        assert_eq!(round(-2.5), -2.0);
    }
}
