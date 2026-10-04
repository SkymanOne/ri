//! JSON output that matches pi's `JSON.stringify`.
//!
//! serde_json and JavaScript escape strings the same way but print numbers
//! differently: JavaScript writes `0.000003` and `5` where serde_json writes
//! `3e-6` and `5.0`. These functions print numbers the JavaScript way, so a
//! document parsed from a pi file serializes back to the same bytes.

use std::io;

use serde::Serialize;
use serde_json::ser::{CompactFormatter, Formatter, PrettyFormatter, Serializer};

/// Serializes `value` like `JSON.stringify(value)`.
pub fn to_string<T: Serialize + ?Sized>(value: &T) -> serde_json::Result<String> {
    write(value, JsFormatter(CompactFormatter))
}

/// Serializes `value` like `JSON.stringify(value, null, indent)`.
pub fn to_string_pretty<T: Serialize + ?Sized>(
    value: &T,
    indent: &str,
) -> serde_json::Result<String> {
    write(
        value,
        JsFormatter(PrettyFormatter::with_indent(indent.as_bytes())),
    )
}

fn write<T: Serialize + ?Sized>(
    value: &T,
    formatter: impl Formatter,
) -> serde_json::Result<String> {
    let mut out = Vec::new();
    value.serialize(&mut Serializer::with_formatter(&mut out, formatter))?;
    // serde_json only writes UTF-8: string contents come from `str` and escapes are ASCII.
    Ok(String::from_utf8(out).expect("serde_json writes UTF-8"))
}

/// Wraps a serde_json formatter, keeping its layout and printing numbers with
/// ECMAScript `Number.prototype.toString`.
struct JsFormatter<F>(F);

/// The largest integer a JavaScript number holds exactly.
const MAX_SAFE_INTEGER: u64 = (1 << 53) - 1;

impl<F: Formatter> Formatter for JsFormatter<F> {
    fn write_f64<W: ?Sized + io::Write>(&mut self, writer: &mut W, value: f64) -> io::Result<()> {
        // serde_json writes `null` for non-finite values before reaching this point.
        writer.write_all(ryu_js::Buffer::new().format(value).as_bytes())
    }

    // Integers beyond what a double holds exactly are rounded, as
    // `JSON.parse` then `JSON.stringify` round them.
    fn write_i64<W: ?Sized + io::Write>(&mut self, writer: &mut W, value: i64) -> io::Result<()> {
        if value.unsigned_abs() > MAX_SAFE_INTEGER {
            return self.write_f64(writer, value as f64);
        }
        self.0.write_i64(writer, value)
    }

    fn write_u64<W: ?Sized + io::Write>(&mut self, writer: &mut W, value: u64) -> io::Result<()> {
        if value > MAX_SAFE_INTEGER {
            return self.write_f64(writer, value as f64);
        }
        self.0.write_u64(writer, value)
    }

    fn begin_array<W: ?Sized + io::Write>(&mut self, writer: &mut W) -> io::Result<()> {
        self.0.begin_array(writer)
    }

    fn end_array<W: ?Sized + io::Write>(&mut self, writer: &mut W) -> io::Result<()> {
        self.0.end_array(writer)
    }

    fn begin_array_value<W: ?Sized + io::Write>(
        &mut self,
        writer: &mut W,
        first: bool,
    ) -> io::Result<()> {
        self.0.begin_array_value(writer, first)
    }

    fn end_array_value<W: ?Sized + io::Write>(&mut self, writer: &mut W) -> io::Result<()> {
        self.0.end_array_value(writer)
    }

    fn begin_object<W: ?Sized + io::Write>(&mut self, writer: &mut W) -> io::Result<()> {
        self.0.begin_object(writer)
    }

    fn end_object<W: ?Sized + io::Write>(&mut self, writer: &mut W) -> io::Result<()> {
        self.0.end_object(writer)
    }

    fn begin_object_key<W: ?Sized + io::Write>(
        &mut self,
        writer: &mut W,
        first: bool,
    ) -> io::Result<()> {
        self.0.begin_object_key(writer, first)
    }

    fn end_object_key<W: ?Sized + io::Write>(&mut self, writer: &mut W) -> io::Result<()> {
        self.0.end_object_key(writer)
    }

    fn begin_object_value<W: ?Sized + io::Write>(&mut self, writer: &mut W) -> io::Result<()> {
        self.0.begin_object_value(writer)
    }

    fn end_object_value<W: ?Sized + io::Write>(&mut self, writer: &mut W) -> io::Result<()> {
        self.0.end_object_value(writer)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;

    #[test]
    fn numbers_match_javascript() {
        // Expected strings are `String(n)` in JavaScript.
        let cases: [(f64, &str); 12] = [
            (0.0, "0"),
            (-0.0, "0"),
            (5.0, "5"),
            (-42.0, "-42"),
            (0.000003, "0.000003"),
            (1e-7, "1e-7"),
            (1e21, "1e+21"),
            (0.1 + 0.2, "0.30000000000000004"),
            (9007199254740992.0, "9007199254740992"),
            (123.456, "123.456"),
            (5e-324, "5e-324"),
            (f64::MAX, "1.7976931348623157e+308"),
        ];
        for (value, expected) in cases {
            assert_eq!(to_string(&value).unwrap(), expected, "{value:e}");
        }
    }

    #[test]
    fn large_integers_round_like_javascript() {
        // `JSON.stringify(JSON.parse(text))` in JavaScript.
        let value: Value = serde_json::from_str(
            "[9007199254740991,9007199254740993,12345678901234567890,-12345678901234567]",
        )
        .unwrap();
        assert_eq!(
            to_string(&value).unwrap(),
            "[9007199254740991,9007199254740992,12345678901234567000,-12345678901234568]"
        );
    }

    #[test]
    fn parsed_numbers_round_trip() {
        let text = r#"[0,0.000003,0.0037020000000000004,1e+21,1893456000000,-1.5e-7]"#;
        let value: Value = serde_json::from_str(text).unwrap();
        assert_eq!(to_string(&value).unwrap(), text);
    }

    #[test]
    fn pretty_matches_javascript_layout() {
        let value = json!({ "a": [], "b": {}, "c": [1, { "d": null }] });
        let expected = "{\n  \"a\": [],\n  \"b\": {},\n  \"c\": [\n    1,\n    {\n      \"d\": null\n    }\n  ]\n}";
        assert_eq!(to_string_pretty(&value, "  ").unwrap(), expected);
    }

    #[test]
    fn non_finite_numbers_are_null() {
        assert_eq!(
            to_string(&[f64::NAN, f64::INFINITY]).unwrap(),
            "[null,null]"
        );
    }
}
