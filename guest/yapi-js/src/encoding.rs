//! Byte encodings for the runtime's `TextEncoder`, `TextDecoder`, `Buffer`,
//! `atob` and `btoa`: UTF-8, Latin-1 and base64, as Node decodes them.

use base64::Engine as _;
use base64::engine::DecodePaddingMode;
use base64::engine::general_purpose::{
    GeneralPurpose, GeneralPurposeConfig, STANDARD, URL_SAFE_NO_PAD,
};
use rquickjs::convert::List;
use rquickjs::{Ctx, TypedArray};

/// Node's base64 decoder: padding optional, leftover bits dropped.
const LENIENT: GeneralPurpose = GeneralPurpose::new(
    &base64::alphabet::STANDARD,
    GeneralPurposeConfig::new()
        .with_decode_padding_mode(DecodePaddingMode::RequireNone)
        .with_decode_allow_trailing_bits(true),
);

/// Runs `f` on the bytes `array` views; a detached array has none.
fn with_bytes<T>(array: &TypedArray<'_, u8>, f: impl FnOnce(&[u8]) -> T) -> T {
    // SAFETY: `f` runs no JS, so nothing can write to, detach or resize the
    // buffer while the slice is borrowed.
    f(unsafe { array.as_bytes() }.unwrap_or_default())
}

/// `text` as UTF-8. The runtime passes well-formed text, with lone
/// surrogates already replaced.
pub fn utf8_encode<'js>(ctx: Ctx<'js>, text: String) -> rquickjs::Result<TypedArray<'js, u8>> {
    TypedArray::new(ctx, text.into_bytes())
}

/// Decodes UTF-8 `bytes`, replacing each invalid sequence with U+FFFD as the
/// WHATWG decoder does. With `stream`, a sequence cut off by the end of
/// `bytes` is left for the next call. Returns the text and the number of
/// bytes left.
pub fn utf8_decode(bytes: TypedArray<'_, u8>, stream: bool) -> List<(String, usize)> {
    with_bytes(&bytes, |bytes| {
        let end = if stream { complete(bytes) } else { bytes.len() };
        List((
            String::from_utf8_lossy(&bytes[..end]).into_owned(),
            bytes.len() - end,
        ))
    })
}

/// The length of `bytes` without a UTF-8 sequence that its end cuts off.
fn complete(bytes: &[u8]) -> usize {
    // A cut-off sequence is a lead byte and at most two continuation bytes.
    let start = bytes.len().saturating_sub(3);
    match bytes[start..].iter().rposition(|byte| byte & 0xc0 != 0x80) {
        Some(lead) => {
            let lead = start + lead;
            match std::str::from_utf8(&bytes[lead..]) {
                Err(error) if error.error_len().is_none() => lead,
                _ => bytes.len(),
            }
        }
        None => bytes.len(),
    }
}

/// `bytes` as Latin-1: each byte is the code point of the same value.
pub fn latin1_decode(bytes: TypedArray<'_, u8>) -> String {
    with_bytes(&bytes, |bytes| {
        bytes.iter().map(|&byte| char::from(byte)).collect()
    })
}

/// `bytes` in base64, or in unpadded base64url with `url`.
pub fn base64_encode(bytes: TypedArray<'_, u8>, url: bool) -> String {
    with_bytes(&bytes, |bytes| {
        if url {
            URL_SAFE_NO_PAD.encode(bytes)
        } else {
            STANDARD.encode(bytes)
        }
    })
}

/// Decodes base64 or base64url as Node does: characters outside both
/// alphabets are skipped and decoding stops at the first `=`.
pub fn base64_decode<'js>(ctx: Ctx<'js>, text: String) -> rquickjs::Result<TypedArray<'js, u8>> {
    let mut digits: Vec<u8> = text
        .bytes()
        .take_while(|&byte| byte != b'=')
        .filter_map(|byte| match byte {
            b'-' => Some(b'+'),
            b'_' => Some(b'/'),
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'+' | b'/' => Some(byte),
            _ => None,
        })
        .collect();
    // A lone final digit holds too few bits for a byte.
    if digits.len() % 4 == 1 {
        digits.pop();
    }
    TypedArray::new(ctx, LENIENT.decode(digits).unwrap_or_default())
}
