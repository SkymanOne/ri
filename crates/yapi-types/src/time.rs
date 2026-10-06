//! Timestamps as pi writes them: Unix milliseconds and ISO 8601 UTC with
//! milliseconds, and ids.

use std::time::{SystemTime, UNIX_EPOCH};

/// Unix time in milliseconds.
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as u64)
}

/// `2026-10-02T14:49:48.573Z`, as JavaScript's `Date.toISOString`.
pub fn iso(ms: u64) -> String {
    let days = (ms / 86_400_000) as i64;
    let rest = ms % 86_400_000;
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        rest / 3_600_000,
        rest / 60_000 % 60,
        rest / 1000 % 60,
        rest % 1000
    )
}

/// The current time as [`iso`].
pub fn now_iso() -> String {
    iso(now_ms())
}

/// Parses [`iso`] output, and offsets other than `Z`, to Unix milliseconds.
pub fn parse_iso(text: &str) -> Option<u64> {
    let bytes = text.as_bytes();
    if bytes.len() < 19 || bytes[4] != b'-' || bytes[7] != b'-' || bytes[13] != b':' {
        return None;
    }
    let number = |range: std::ops::Range<usize>| text.get(range)?.parse::<i64>().ok();
    let (year, month, day) = (number(0..4)?, number(5..7)?, number(8..10)?);
    let (hour, minute, second) = (number(11..13)?, number(14..16)?, number(17..19)?);
    let mut rest = &text[19..];
    let mut millis = 0;
    if let Some(fraction) = rest.strip_prefix('.') {
        let digits: String = fraction.chars().take_while(char::is_ascii_digit).collect();
        rest = &fraction[digits.len()..];
        millis = format!("{digits:0<3}")[..3].parse::<i64>().ok()?;
    }
    let offset_minutes = match rest {
        "Z" | "" => 0,
        offset => {
            let sign = if offset.starts_with('-') { -1 } else { 1 };
            let hours = offset.get(1..3)?.parse::<i64>().ok()?;
            let minutes = offset
                .get(4..6)
                .and_then(|m| m.parse::<i64>().ok())
                .unwrap_or(0);
            sign * (hours * 60 + minutes)
        }
    };
    let days = days_from_civil(year, month, day);
    let ms = ((days * 24 + hour) * 60 + minute - offset_minutes) * 60_000 + second * 1000 + millis;
    u64::try_from(ms).ok()
}

/// Howard Hinnant's algorithm: days since 1970-01-01 to (year, month, day).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let yoe = year.rem_euclid(400);
    let mp = if month > 2 { month - 3 } else { month + 9 };
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// `N` bytes from the OS generator; zeros on a platform without one.
pub fn random_bytes<const N: usize>() -> [u8; N] {
    let mut bytes = [0u8; N];
    let _ = getrandom::fill(&mut bytes);
    bytes
}

/// `bytes` as lowercase hexadecimal.
pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// `count` random bytes as lowercase hexadecimal.
pub fn random_hex(count: usize) -> String {
    let mut bytes = vec![0u8; count];
    let _ = getrandom::fill(&mut bytes);
    hex(&bytes)
}

/// A random UUID v4, as `crypto.randomUUID`.
pub fn uuid_v4() -> String {
    let mut bytes = random_bytes::<16>();
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    format_uuid(&bytes)
}

/// A time-ordered UUID v7, used for session ids.
pub fn uuid_v7() -> String {
    let mut bytes = random_bytes::<16>();
    bytes[..6].copy_from_slice(&now_ms().to_be_bytes()[2..]);
    bytes[6] = (bytes[6] & 0x0f) | 0x70;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    format_uuid(&bytes)
}

fn format_uuid(bytes: &[u8; 16]) -> String {
    format!(
        "{}-{}-{}-{}-{}",
        hex(&bytes[..4]),
        hex(&bytes[4..6]),
        hex(&bytes[6..8]),
        hex(&bytes[8..10]),
        hex(&bytes[10..])
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_iso() {
        let ms = 1_790_952_595_641;
        assert_eq!(iso(ms), "2026-10-02T14:49:55.641Z");
        assert_eq!(parse_iso("2026-10-02T14:49:55.641Z"), Some(ms));
        assert_eq!(parse_iso("2026-10-02T16:49:55.641+02:00"), Some(ms));
        assert_eq!(iso(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(parse_iso("2024-02-29T00:00:00Z"), Some(1_709_164_800_000));
    }

    #[test]
    fn makes_uuids() {
        let v7 = uuid_v7();
        assert_eq!(v7.len(), 36);
        assert_eq!(&v7[14..15], "7");
        assert_eq!(&uuid_v4()[14..15], "4");
    }
}
