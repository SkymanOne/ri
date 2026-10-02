//! Lenient JSON parsing for model output: tool-call arguments and SSE payloads.

use serde_json::{Map, Number, Value};

const VALID_ESCAPES: &[char] = &['"', '\\', '/', 'b', 'f', 'n', 'r', 't', 'u'];

/// Escapes what models commonly emit raw inside JSON strings: control characters,
/// lone backslashes and invalid escapes. Valid JSON is returned unchanged.
pub fn repair_json(json: &str) -> String {
    let chars: Vec<char> = json.chars().collect();
    let mut repaired = String::with_capacity(json.len());
    let mut in_string = false;
    let mut index = 0;
    while index < chars.len() {
        let char = chars[index];
        if !in_string {
            repaired.push(char);
            in_string = char == '"';
        } else if char == '"' {
            repaired.push(char);
            in_string = false;
        } else if char == '\\' {
            match chars.get(index + 1) {
                None => repaired.push_str("\\\\"),
                Some('u')
                    if chars.len() >= index + 6
                        && chars[index + 2..index + 6]
                            .iter()
                            .all(char::is_ascii_hexdigit) =>
                {
                    repaired.extend(&chars[index..index + 6]);
                    index += 5;
                }
                Some(next) if VALID_ESCAPES.contains(next) => {
                    repaired.push('\\');
                    repaired.push(*next);
                    index += 1;
                }
                Some(_) => repaired.push_str("\\\\"),
            }
        } else if (char as u32) < 0x20 {
            match char {
                '\u{8}' => repaired.push_str("\\b"),
                '\u{c}' => repaired.push_str("\\f"),
                '\n' => repaired.push_str("\\n"),
                '\r' => repaired.push_str("\\r"),
                '\t' => repaired.push_str("\\t"),
                _ => repaired.push_str(&format!("\\u{:04x}", char as u32)),
            }
        } else {
            repaired.push(char);
        }
        index += 1;
    }
    repaired
}

/// Parses JSON, retrying once on the [`repair_json`] form.
pub fn parse_json_with_repair(json: &str) -> Result<Value, serde_json::Error> {
    serde_json::from_str(json).or_else(|err| {
        let repaired = repair_json(json);
        if repaired == json {
            Err(err)
        } else {
            serde_json::from_str(&repaired)
        }
    })
}

/// Parses possibly incomplete tool-call arguments into an object.
///
/// Complete JSON parses as-is (with repair); a prefix of an object yields the
/// fields seen so far, with a trailing string, number or literal kept in its partial
/// form. Anything that is not an object yields an empty object.
pub fn parse_streaming_json(partial: &str) -> Map<String, Value> {
    if partial.trim().is_empty() {
        return Map::new();
    }
    let value = parse_json_with_repair(partial)
        .ok()
        .or_else(|| parse_partial(partial))
        .or_else(|| parse_partial(&repair_json(partial)));
    match value {
        Some(Value::Object(map)) => map,
        _ => Map::new(),
    }
}

/// Parses a JSON prefix, closing whatever is still open.
fn parse_partial(text: &str) -> Option<Value> {
    let mut parser = Partial {
        chars: text.chars().collect(),
        index: 0,
    };
    parser.whitespace();
    parser.value().map(|(value, _)| value)
}

struct Partial {
    chars: Vec<char>,
    index: usize,
}

/// Whether a value ended before its closing token.
type Complete = bool;

impl Partial {
    fn peek(&self) -> Option<char> {
        self.chars.get(self.index).copied()
    }

    fn whitespace(&mut self) {
        while self
            .peek()
            .is_some_and(|c| matches!(c, ' ' | '\t' | '\n' | '\r'))
        {
            self.index += 1;
        }
    }

    /// A value, or `None` when nothing usable precedes the end or an error.
    fn value(&mut self) -> Option<(Value, Complete)> {
        match self.peek()? {
            '{' => self.object(),
            '[' => self.array(),
            '"' => self
                .string()
                .map(|(text, complete)| (Value::String(text), complete)),
            't' => self.literal("true", Value::Bool(true)),
            'f' => self.literal("false", Value::Bool(false)),
            'n' => self.literal("null", Value::Null),
            '-' | '0'..='9' => self.number(),
            _ => None,
        }
    }

    fn object(&mut self) -> Option<(Value, Complete)> {
        self.index += 1;
        let mut map = Map::new();
        loop {
            self.whitespace();
            match self.peek() {
                None => return Some((Value::Object(map), false)),
                Some('}') => {
                    self.index += 1;
                    return Some((Value::Object(map), true));
                }
                Some(',') => {
                    self.index += 1;
                    continue;
                }
                Some('"') => {}
                Some(_) => return None,
            }
            let (key, complete) = self.string()?;
            if !complete {
                return Some((Value::Object(map), false));
            }
            self.whitespace();
            match self.peek() {
                None => return Some((Value::Object(map), false)),
                Some(':') => self.index += 1,
                Some(_) => return None,
            }
            self.whitespace();
            if self.peek().is_none() {
                return Some((Value::Object(map), false));
            }
            match self.value() {
                Some((value, true)) => {
                    map.insert(key, value);
                }
                Some((value, false)) => {
                    map.insert(key, value);
                    return Some((Value::Object(map), false));
                }
                None if self.peek().is_none() => return Some((Value::Object(map), false)),
                None => return None,
            }
        }
    }

    fn array(&mut self) -> Option<(Value, Complete)> {
        self.index += 1;
        let mut items = Vec::new();
        loop {
            self.whitespace();
            match self.peek() {
                None => return Some((Value::Array(items), false)),
                Some(']') => {
                    self.index += 1;
                    return Some((Value::Array(items), true));
                }
                Some(',') => {
                    self.index += 1;
                    continue;
                }
                Some(_) => {}
            }
            match self.value() {
                Some((value, true)) => items.push(value),
                Some((value, false)) => {
                    items.push(value);
                    return Some((Value::Array(items), false));
                }
                None if self.peek().is_none() => return Some((Value::Array(items), false)),
                None => return None,
            }
        }
    }

    fn string(&mut self) -> Option<(String, Complete)> {
        self.index += 1;
        let mut text = String::new();
        while let Some(char) = self.peek() {
            self.index += 1;
            match char {
                '"' => return Some((text, true)),
                '\\' => {
                    let Some(escape) = self.peek() else {
                        return Some((text, false));
                    };
                    self.index += 1;
                    match escape {
                        '"' | '\\' | '/' => text.push(escape),
                        'b' => text.push('\u{8}'),
                        'f' => text.push('\u{c}'),
                        'n' => text.push('\n'),
                        'r' => text.push('\r'),
                        't' => text.push('\t'),
                        'u' => {
                            let Some(unit) = self.hex4() else {
                                return Some((text, false));
                            };
                            let code = if (0xd800..0xdc00).contains(&unit)
                                && self.chars.get(self.index) == Some(&'\\')
                                && self.chars.get(self.index + 1) == Some(&'u')
                            {
                                self.index += 2;
                                match self.hex4() {
                                    Some(low) if (0xdc00..0xe000).contains(&low) => {
                                        0x10000 + ((unit - 0xd800) << 10) + (low - 0xdc00)
                                    }
                                    _ => 0xfffd,
                                }
                            } else {
                                unit
                            };
                            text.push(char::from_u32(code).unwrap_or('\u{fffd}'));
                        }
                        _ => return None,
                    }
                }
                _ => text.push(char),
            }
        }
        Some((text, false))
    }

    fn hex4(&mut self) -> Option<u32> {
        let digits: String = self.chars.get(self.index..self.index + 4)?.iter().collect();
        let value = u32::from_str_radix(&digits, 16).ok()?;
        self.index += 4;
        Some(value)
    }

    fn literal(&mut self, word: &str, value: Value) -> Option<(Value, Complete)> {
        for expected in word.chars() {
            match self.peek() {
                None => return Some((value, false)),
                Some(char) if char == expected => self.index += 1,
                Some(_) => return None,
            }
        }
        Some((value, true))
    }

    fn number(&mut self) -> Option<(Value, Complete)> {
        let start = self.index;
        while self
            .peek()
            .is_some_and(|c| c.is_ascii_digit() || matches!(c, '-' | '+' | '.' | 'e' | 'E'))
        {
            self.index += 1;
        }
        let complete = self.peek().is_some();
        let mut text: String = self.chars[start..self.index].iter().collect();
        loop {
            if let Ok(number) = serde_json::from_str::<Number>(&text) {
                return Some((Value::Number(number), complete));
            }
            if complete || text.pop().is_none() || text.is_empty() {
                return None;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn repairs_raw_control_characters_and_bad_escapes() {
        assert_eq!(repair_json("{\"a\":\"x\ny\"}"), "{\"a\":\"x\\ny\"}");
        assert_eq!(repair_json(r#"{"a":"C:\dir"}"#), r#"{"a":"C:\\dir"}"#);
        assert_eq!(repair_json(r#"{"a":"\u00e9\n"}"#), r#"{"a":"\u00e9\n"}"#);
        assert_eq!(
            parse_json_with_repair("{\"a\":\"tab\there\"}").unwrap(),
            json!({"a": "tab\there"})
        );
    }

    #[test]
    fn parses_prefixes() {
        let cases = [
            ("", json!({})),
            ("{", json!({})),
            (r#"{"pa"#, json!({})),
            (r#"{"path""#, json!({})),
            (r#"{"path":"#, json!({})),
            (r#"{"path":"/tm"#, json!({"path": "/tm"})),
            (r#"{"path":"/tmp","n":12"#, json!({"path": "/tmp", "n": 12})),
            (r#"{"n":1.5e"#, json!({"n": 1.5})),
            (r#"{"a":[1,{"b":tr"#, json!({"a": [1, {"b": true}]})),
            (r#"{"a":"\u00"#, json!({"a": ""})),
            (r#"{"a":"x\"#, json!({"a": "x"})),
            (r#"{"a":1}"#, json!({"a": 1})),
            ("[1,2]", json!({})),
            ("nonsense", json!({})),
        ];
        for (input, expected) in cases {
            assert_eq!(
                Value::Object(parse_streaming_json(input)),
                expected,
                "{input}"
            );
        }
    }
}
