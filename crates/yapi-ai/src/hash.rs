//! pi's `shortHash`, used to derive stable ids from longer ones.

/// pi's `shortHash`: two 32-bit mixes over UTF-16 units, printed in base 36.
pub fn short_hash(text: &str) -> String {
    let (mut h1, mut h2): (u32, u32) = (0xdead_beef, 0x41c6_ce57);
    for unit in text.encode_utf16() {
        let unit = u32::from(unit);
        h1 = (h1 ^ unit).wrapping_mul(2_654_435_761);
        h2 = (h2 ^ unit).wrapping_mul(1_597_334_677);
    }
    h1 = (h1 ^ (h1 >> 16)).wrapping_mul(2_246_822_507)
        ^ (h2 ^ (h2 >> 13)).wrapping_mul(3_266_489_909);
    h2 = (h2 ^ (h2 >> 16)).wrapping_mul(2_246_822_507)
        ^ (h1 ^ (h1 >> 13)).wrapping_mul(3_266_489_909);
    format!("{}{}", base36(h2), base36(h1))
}

fn base36(mut value: u32) -> String {
    if value == 0 {
        return "0".into();
    }
    let mut digits = Vec::new();
    while value > 0 {
        digits.push(std::char::from_digit(value % 36, 36).unwrap_or('0'));
        value /= 36;
    }
    digits.iter().rev().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashes_like_pi() {
        // Values computed with pi's shortHash in Node.
        assert_eq!(short_hash(""), "k4n83c7h0j2b");
        assert_eq!(
            short_hash("call_abc|fc_0123456789abcdefghijklmnopqrstuvwxyz"),
            "zdt65wxpmvl8"
        );
    }
}
