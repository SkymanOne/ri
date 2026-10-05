//! Key identification from raw terminal input: legacy sequences, the Kitty
//! keyboard protocol and xterm `modifyOtherKeys`.
//!
//! Port of `packages/tui/src/keys.ts` in pi `v1.0.0`. Key ids are pi's:
//! `"ctrl+c"`, `"shift+enter"`, `"alt+left"`, `"pageUp"`, and so on.

const SHIFT: u32 = 1;
const ALT: u32 = 2;
const CTRL: u32 = 4;
const SUPER: u32 = 8;
/// Caps Lock and Num Lock bits, ignored when matching.
const LOCK_MASK: u32 = 64 + 128;

const ESCAPE: i64 = 27;
const TAB: i64 = 9;
const ENTER: i64 = 13;
const SPACE: i64 = 32;
const BACKSPACE: i64 = 127;
const KP_ENTER: i64 = 57414;

const UP: i64 = -1;
const DOWN: i64 = -2;
const RIGHT: i64 = -3;
const LEFT: i64 = -4;
const DELETE: i64 = -10;
const INSERT: i64 = -11;
const PAGE_UP: i64 = -12;
const PAGE_DOWN: i64 = -13;
const HOME: i64 = -14;
const END: i64 = -15;

const SYMBOL_KEYS: &str = "`-=[]\\;',./!@#$%^&*()_+|~{}:<>?";

fn is_symbol(c: char) -> bool {
    SYMBOL_KEYS.contains(c)
}

fn is_symbol_code(code: i64) -> bool {
    u32::try_from(code)
        .ok()
        .and_then(char::from_u32)
        .is_some_and(is_symbol)
}

/// Kitty's keypad and functional key codes and their main-keyboard equivalents.
fn normalize_functional(code: i64) -> i64 {
    match code {
        57399..=57408 => code - 57399 + 48,
        57409 => 46,
        57410 => 47,
        57411 => 42,
        57412 => 45,
        57413 => 43,
        57415 => 61,
        57416 => 44,
        57417 => LEFT,
        57418 => RIGHT,
        57419 => UP,
        57420 => DOWN,
        57421 => PAGE_UP,
        57422 => PAGE_DOWN,
        57423 => HOME,
        57424 => END,
        57425 => INSERT,
        57426 => DELETE,
        other => other,
    }
}

fn normalize_shifted_letter(code: i64, modifier: u32) -> i64 {
    if (modifier & !LOCK_MASK) & SHIFT != 0 && (65..=90).contains(&code) {
        code + 32
    } else {
        code
    }
}

fn legacy(key: &str) -> &'static [&'static str] {
    match key {
        "up" => &["\x1b[A", "\x1bOA"],
        "down" => &["\x1b[B", "\x1bOB"],
        "right" => &["\x1b[C", "\x1bOC"],
        "left" => &["\x1b[D", "\x1bOD"],
        "home" => &["\x1b[H", "\x1bOH", "\x1b[1~", "\x1b[7~"],
        "end" => &["\x1b[F", "\x1bOF", "\x1b[4~", "\x1b[8~"],
        "insert" => &["\x1b[2~"],
        "delete" => &["\x1b[3~"],
        "pageup" => &["\x1b[5~", "\x1b[[5~"],
        "pagedown" => &["\x1b[6~", "\x1b[[6~"],
        "clear" => &["\x1b[E", "\x1bOE"],
        "f1" => &["\x1bOP", "\x1b[11~", "\x1b[[A"],
        "f2" => &["\x1bOQ", "\x1b[12~", "\x1b[[B"],
        "f3" => &["\x1bOR", "\x1b[13~", "\x1b[[C"],
        "f4" => &["\x1bOS", "\x1b[14~", "\x1b[[D"],
        "f5" => &["\x1b[15~", "\x1b[[E"],
        "f6" => &["\x1b[17~"],
        "f7" => &["\x1b[18~"],
        "f8" => &["\x1b[19~"],
        "f9" => &["\x1b[20~"],
        "f10" => &["\x1b[21~"],
        "f11" => &["\x1b[23~"],
        "f12" => &["\x1b[24~"],
        _ => &[],
    }
}

fn legacy_shift(key: &str) -> &'static [&'static str] {
    match key {
        "up" => &["\x1b[a"],
        "down" => &["\x1b[b"],
        "right" => &["\x1b[c"],
        "left" => &["\x1b[d"],
        "clear" => &["\x1b[e"],
        "insert" => &["\x1b[2$"],
        "delete" => &["\x1b[3$"],
        "pageup" => &["\x1b[5$"],
        "pagedown" => &["\x1b[6$"],
        "home" => &["\x1b[7$"],
        "end" => &["\x1b[8$"],
        _ => &[],
    }
}

fn legacy_ctrl(key: &str) -> &'static [&'static str] {
    match key {
        "up" => &["\x1bOa"],
        "down" => &["\x1bOb"],
        "right" => &["\x1bOc"],
        "left" => &["\x1bOd"],
        "clear" => &["\x1bOe"],
        "insert" => &["\x1b[2^"],
        "delete" => &["\x1b[3^"],
        "pageup" => &["\x1b[5^"],
        "pagedown" => &["\x1b[6^"],
        "home" => &["\x1b[7^"],
        "end" => &["\x1b[8^"],
        _ => &[],
    }
}

fn legacy_sequence_key(data: &str) -> Option<&'static str> {
    Some(match data {
        "\x1bOA" => "up",
        "\x1bOB" => "down",
        "\x1bOC" => "right",
        "\x1bOD" => "left",
        "\x1bOH" => "home",
        "\x1bOF" => "end",
        "\x1b[E" | "\x1bOE" => "clear",
        "\x1bOe" => "ctrl+clear",
        "\x1b[e" => "shift+clear",
        "\x1b[2~" => "insert",
        "\x1b[2$" => "shift+insert",
        "\x1b[2^" => "ctrl+insert",
        "\x1b[3$" => "shift+delete",
        "\x1b[3^" => "ctrl+delete",
        "\x1b[[5~" => "pageUp",
        "\x1b[[6~" => "pageDown",
        "\x1b[a" => "shift+up",
        "\x1b[b" => "shift+down",
        "\x1b[c" => "shift+right",
        "\x1b[d" => "shift+left",
        "\x1bOa" => "ctrl+up",
        "\x1bOb" => "ctrl+down",
        "\x1bOc" => "ctrl+right",
        "\x1bOd" => "ctrl+left",
        "\x1b[5$" => "shift+pageUp",
        "\x1b[6$" => "shift+pageDown",
        "\x1b[7$" => "shift+home",
        "\x1b[8$" => "shift+end",
        "\x1b[5^" => "ctrl+pageUp",
        "\x1b[6^" => "ctrl+pageDown",
        "\x1b[7^" => "ctrl+home",
        "\x1b[8^" => "ctrl+end",
        "\x1bOP" | "\x1b[11~" | "\x1b[[A" => "f1",
        "\x1bOQ" | "\x1b[12~" | "\x1b[[B" => "f2",
        "\x1bOR" | "\x1b[13~" | "\x1b[[C" => "f3",
        "\x1bOS" | "\x1b[14~" | "\x1b[[D" => "f4",
        "\x1b[[E" | "\x1b[15~" => "f5",
        "\x1b[17~" => "f6",
        "\x1b[18~" => "f7",
        "\x1b[19~" => "f8",
        "\x1b[20~" => "f9",
        "\x1b[21~" => "f10",
        "\x1b[23~" => "f11",
        "\x1b[24~" => "f12",
        "\x1bb" => "alt+left",
        "\x1bf" => "alt+right",
        "\x1bp" => "alt+up",
        "\x1bn" => "alt+down",
        _ => return None,
    })
}

fn legacy_modified(data: &str, key: &str, modifier: u32) -> bool {
    match modifier {
        SHIFT => legacy_shift(key).contains(&data),
        CTRL => legacy_ctrl(key).contains(&data),
        _ => false,
    }
}

/// A press, repeat or release (Kitty flag 2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyEventType {
    /// The key went down.
    Press,
    /// The key is held.
    Repeat,
    /// The key went up.
    Release,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Kitty {
    code: i64,
    shifted: Option<i64>,
    base_layout: Option<i64>,
    modifier: u32,
    event: KeyEventType,
}

impl Kitty {
    /// A key without shifted or base-layout codes.
    fn plain(code: i64, modifier: u32, event: KeyEventType) -> Kitty {
        Kitty {
            code,
            shifted: None,
            base_layout: None,
            modifier,
            event,
        }
    }
}

/// Reads a run of ASCII digits from the front of `text`.
fn digits(text: &str) -> (Option<i64>, &str) {
    let end = text
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(text.len());
    (text[..end].parse().ok(), &text[end..])
}

/// The `[; mod] [: event]` end of a CSI key: its modifier, 1 when absent, and
/// event; `None` when anything else follows.
fn mod_event(mut rest: &str) -> Option<(i64, Option<i64>)> {
    let mut modifier = 1;
    if let Some(after) = rest.strip_prefix(';') {
        let (value, after) = digits(after);
        modifier = value?;
        rest = after;
    }
    let mut event = None;
    if let Some(after) = rest.strip_prefix(':') {
        let (value, after) = digits(after);
        event = Some(value?);
        rest = after;
    }
    rest.is_empty().then_some((modifier, event))
}

fn event_type(value: Option<i64>) -> KeyEventType {
    match value {
        Some(2) => KeyEventType::Repeat,
        Some(3) => KeyEventType::Release,
        _ => KeyEventType::Press,
    }
}

/// `ESC [ code [: shifted [: base]] [; mod [: event]] u`, the shifted part possibly
/// empty.
fn parse_csi_u(data: &str) -> Option<Kitty> {
    let body = data.strip_prefix("\x1b[")?.strip_suffix('u')?;
    let (code, mut rest) = digits(body);
    let code = code?;
    let mut shifted = None;
    let mut base_layout = None;
    if let Some(after) = rest.strip_prefix(':') {
        let (value, after) = digits(after);
        shifted = value;
        rest = after;
        if let Some(after) = rest.strip_prefix(':') {
            let (value, after) = digits(after);
            base_layout = Some(value?);
            rest = after;
        }
    }
    let (modifier, event) = mod_event(rest)?;
    Some(Kitty {
        code,
        shifted,
        base_layout,
        modifier: u32::try_from(modifier - 1).ok()?,
        event: event_type(event),
    })
}

/// `ESC [ 1 ; mod [: event] <final>` for arrows and Home/End.
fn parse_modified_final(data: &str, finals: &str) -> Option<(char, u32, KeyEventType)> {
    let body = data.strip_prefix("\x1b[1;")?;
    let last = body.chars().last()?;
    if !finals.contains(last) {
        return None;
    }
    let body = &body[..body.len() - 1];
    let (modifier, rest) = digits(body);
    let modifier = modifier?;
    let event = match rest {
        "" => None,
        other => {
            let (value, rest) = digits(other.strip_prefix(':')?);
            if !rest.is_empty() {
                return None;
            }
            Some(value?)
        }
    };
    Some((last, u32::try_from(modifier - 1).ok()?, event_type(event)))
}

/// `ESC [ num [; mod] [: event] ~` for functional keys.
fn parse_functional(data: &str) -> Option<Kitty> {
    let body = data.strip_prefix("\x1b[")?.strip_suffix('~')?;
    let (number, rest) = digits(body);
    let number = number?;
    let (modifier, event) = mod_event(rest)?;
    let code = match number {
        2 => INSERT,
        3 => DELETE,
        5 => PAGE_UP,
        6 => PAGE_DOWN,
        7 => HOME,
        8 => END,
        _ => return None,
    };
    Some(Kitty::plain(
        code,
        u32::try_from(modifier - 1).ok()?,
        event_type(event),
    ))
}

fn parse_kitty(data: &str) -> Option<Kitty> {
    if let Some(kitty) = parse_csi_u(data) {
        return Some(kitty);
    }
    if let Some((last, modifier, event)) = parse_modified_final(data, "ABCD") {
        let code = match last {
            'A' => UP,
            'B' => DOWN,
            'C' => RIGHT,
            _ => LEFT,
        };
        return Some(Kitty::plain(code, modifier, event));
    }
    if let Some(kitty) = parse_functional(data) {
        return Some(kitty);
    }
    let (last, modifier, event) = parse_modified_final(data, "HF")?;
    Some(Kitty::plain(
        if last == 'H' { HOME } else { END },
        modifier,
        event,
    ))
}

/// `ESC [ 27 ; mod ; code ~`, xterm's modifyOtherKeys.
fn parse_modify_other_keys(data: &str) -> Option<(i64, u32)> {
    let body = data.strip_prefix("\x1b[27;")?.strip_suffix('~')?;
    let (modifier, rest) = digits(body);
    let (code, rest) = digits(rest.strip_prefix(';')?);
    if !rest.is_empty() {
        return None;
    }
    Some((code?, u32::try_from(modifier? - 1).ok()?))
}

/// Whether input carries the Kitty event type `event` (flag 2). Pasted text
/// never does.
fn has_event(data: &str, event: char) -> bool {
    !data.contains("\x1b[200~")
        && "u~ABCDHF"
            .chars()
            .any(|last| data.contains(&format!(":{event}{last}")))
}

/// Whether input is a Kitty key release (flag 2). Pasted text never is.
pub fn is_key_release(data: &str) -> bool {
    has_event(data, '3')
}

/// Whether input is a Kitty key repeat (flag 2). Pasted text never is.
pub fn is_key_repeat(data: &str) -> bool {
    has_event(data, '2')
}

fn raw_ctrl_char(key: &str) -> Option<String> {
    let c = key.chars().next()?.to_ascii_lowercase();
    let code = c as u32;
    if c.is_ascii_lowercase() || matches!(c, '[' | '\\' | ']' | '_') {
        return char::from_u32(code & 0x1f).map(String::from);
    }
    (c == '-').then(|| "\x1f".to_owned())
}

fn format_with_modifiers(name: &str, modifier: u32) -> Option<String> {
    let effective = modifier & !LOCK_MASK;
    if effective & !(SHIFT | CTRL | ALT | SUPER) != 0 {
        return None;
    }
    let mut parts: Vec<&str> = Vec::new();
    if effective & SHIFT != 0 {
        parts.push("shift");
    }
    if effective & CTRL != 0 {
        parts.push("ctrl");
    }
    if effective & ALT != 0 {
        parts.push("alt");
    }
    if effective & SUPER != 0 {
        parts.push("super");
    }
    parts.push(name);
    Some(parts.join("+"))
}

/// Matches raw terminal input against pi key ids.
///
/// `kitty` says the Kitty keyboard protocol is active, which changes how some
/// ambiguous legacy sequences read. `windows_terminal` makes a raw `0x08` mean
/// Ctrl+Backspace, as Windows Terminal sends it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Keys {
    /// The Kitty keyboard protocol is active.
    pub kitty: bool,
    /// Running in Windows Terminal outside SSH.
    pub windows_terminal: bool,
}

impl Keys {
    /// Detects Windows Terminal from the environment, as pi does.
    pub fn detect(kitty: bool) -> Keys {
        let set = |name: &str| std::env::var_os(name).is_some_and(|value| !value.is_empty());
        Keys {
            kitty,
            windows_terminal: set("WT_SESSION")
                && !set("SSH_CONNECTION")
                && !set("SSH_CLIENT")
                && !set("SSH_TTY"),
        }
    }

    fn kitty_matches(data: &str, expected: i64, expected_modifier: u32) -> bool {
        let Some(parsed) = parse_kitty(data) else {
            return false;
        };
        if parsed.modifier & !LOCK_MASK != expected_modifier & !LOCK_MASK {
            return false;
        }
        let code = normalize_shifted_letter(normalize_functional(parsed.code), parsed.modifier);
        let expected_code =
            normalize_shifted_letter(normalize_functional(expected), expected_modifier);
        if code == expected_code {
            return true;
        }
        // Non-Latin layouts report the PC-101 key as the base layout key; trust it
        // only when the reported key is not itself a Latin letter or symbol.
        parsed.base_layout == Some(expected) && !(97..=122).contains(&code) && !is_symbol_code(code)
    }

    fn modify_other_keys_matches(data: &str, expected: i64, expected_modifier: u32) -> bool {
        parse_modify_other_keys(data)
            .is_some_and(|(code, modifier)| code == expected && modifier == expected_modifier)
    }

    fn printable_modify_other_keys(data: &str, expected: i64, expected_modifier: u32) -> bool {
        if expected_modifier == 0 {
            return false;
        }
        parse_modify_other_keys(data).is_some_and(|(code, modifier)| {
            modifier == expected_modifier
                && normalize_shifted_letter(code, modifier)
                    == normalize_shifted_letter(expected, expected_modifier)
        })
    }

    fn raw_backspace(&self, data: &str, expected_modifier: u32) -> bool {
        match data {
            "\x7f" => expected_modifier == 0,
            "\x08" if self.windows_terminal => expected_modifier == CTRL,
            "\x08" => expected_modifier == 0,
            _ => false,
        }
    }

    fn either(data: &str, code: i64, modifier: u32) -> bool {
        Self::kitty_matches(data, code, modifier)
            || Self::modify_other_keys_matches(data, code, modifier)
    }

    fn navigation(&self, data: &str, key: &str, code: i64, modifier: u32) -> bool {
        if modifier == 0 {
            return legacy(key).contains(&data) || Self::kitty_matches(data, code, 0);
        }
        legacy_modified(data, key, modifier) || Self::kitty_matches(data, code, modifier)
    }

    /// Whether `data` is the key `key_id`, such as `"ctrl+c"` or `"shift+enter"`.
    pub fn matches(&self, data: &str, key_id: &str) -> bool {
        let lower = key_id.to_lowercase();
        let parts: Vec<&str> = lower.split('+').collect();
        let Some(&key) = parts.last().filter(|key| !key.is_empty()) else {
            return false;
        };
        let mut modifier = 0;
        for (name, bit) in [
            ("shift", SHIFT),
            ("alt", ALT),
            ("ctrl", CTRL),
            ("super", SUPER),
        ] {
            if parts.contains(&name) {
                modifier |= bit;
            }
        }
        match key {
            "escape" | "esc" => {
                modifier == 0
                    && (data == "\x1b"
                        || Self::kitty_matches(data, ESCAPE, 0)
                        || Self::modify_other_keys_matches(data, ESCAPE, 0))
            }
            "space" => {
                if !self.kitty
                    && ((modifier == CTRL && data == "\x00")
                        || (modifier == ALT && data == "\x1b "))
                {
                    return true;
                }
                if modifier == 0 {
                    return data == " " || Self::either(data, SPACE, 0);
                }
                Self::either(data, SPACE, modifier)
            }
            "tab" => match modifier {
                SHIFT => data == "\x1b[Z" || Self::either(data, TAB, SHIFT),
                0 => data == "\t" || Self::kitty_matches(data, TAB, 0),
                _ => Self::either(data, TAB, modifier),
            },
            "enter" | "return" => match modifier {
                SHIFT => {
                    Self::kitty_matches(data, ENTER, SHIFT)
                        || Self::kitty_matches(data, KP_ENTER, SHIFT)
                        || Self::modify_other_keys_matches(data, ENTER, SHIFT)
                        || (self.kitty && (data == "\x1b\r" || data == "\n"))
                }
                ALT => {
                    Self::kitty_matches(data, ENTER, ALT)
                        || Self::kitty_matches(data, KP_ENTER, ALT)
                        || Self::modify_other_keys_matches(data, ENTER, ALT)
                        || (!self.kitty && data == "\x1b\r")
                }
                0 => {
                    data == "\r"
                        || (!self.kitty && data == "\n")
                        || data == "\x1bOM"
                        || Self::kitty_matches(data, ENTER, 0)
                        || Self::kitty_matches(data, KP_ENTER, 0)
                }
                _ => {
                    Self::kitty_matches(data, ENTER, modifier)
                        || Self::kitty_matches(data, KP_ENTER, modifier)
                        || Self::modify_other_keys_matches(data, ENTER, modifier)
                }
            },
            "backspace" => match modifier {
                ALT => {
                    data == "\x1b\x7f" || data == "\x1b\x08" || Self::either(data, BACKSPACE, ALT)
                }
                CTRL => self.raw_backspace(data, CTRL) || Self::either(data, BACKSPACE, CTRL),
                0 => self.raw_backspace(data, 0) || Self::either(data, BACKSPACE, 0),
                _ => Self::either(data, BACKSPACE, modifier),
            },
            "insert" => self.navigation(data, key, INSERT, modifier),
            "delete" => self.navigation(data, key, DELETE, modifier),
            "clear" => {
                if modifier == 0 {
                    legacy("clear").contains(&data)
                } else {
                    legacy_modified(data, "clear", modifier)
                }
            }
            "home" => self.navigation(data, key, HOME, modifier),
            "end" => self.navigation(data, key, END, modifier),
            "pageup" => self.navigation(data, key, PAGE_UP, modifier),
            "pagedown" => self.navigation(data, key, PAGE_DOWN, modifier),
            "up" | "down" => {
                let (code, alt_legacy) = if key == "up" {
                    (UP, "\x1bp")
                } else {
                    (DOWN, "\x1bn")
                };
                if modifier == ALT {
                    return data == alt_legacy || Self::kitty_matches(data, code, ALT);
                }
                self.navigation(data, key, code, modifier)
            }
            "left" | "right" => {
                let left = key == "left";
                let code = if left { LEFT } else { RIGHT };
                if modifier == ALT {
                    let (csi, upper, lower) = if left {
                        ("\x1b[1;3D", "\x1bB", "\x1bb")
                    } else {
                        ("\x1b[1;3C", "\x1bF", "\x1bf")
                    };
                    return data == csi
                        || (!self.kitty && data == upper)
                        || data == lower
                        || Self::kitty_matches(data, code, ALT);
                }
                if modifier == CTRL {
                    let csi = if left { "\x1b[1;5D" } else { "\x1b[1;5C" };
                    return data == csi
                        || legacy_modified(data, key, CTRL)
                        || Self::kitty_matches(data, code, CTRL);
                }
                self.navigation(data, key, code, modifier)
            }
            "f1" | "f2" | "f3" | "f4" | "f5" | "f6" | "f7" | "f8" | "f9" | "f10" | "f11"
            | "f12" => modifier == 0 && legacy(key).contains(&data),
            _ => self.matches_character(data, key, modifier),
        }
    }

    fn matches_character(&self, data: &str, key: &str, modifier: u32) -> bool {
        let mut chars = key.chars();
        let (Some(c), None) = (chars.next(), chars.next()) else {
            return false;
        };
        let letter = c.is_ascii_lowercase();
        let digit = c.is_ascii_digit();
        if !letter && !digit && !is_symbol(c) {
            return false;
        }
        let code = i64::from(u32::from(c));
        let raw_ctrl = raw_ctrl_char(key);
        if modifier == CTRL + ALT
            && !self.kitty
            && let Some(raw) = &raw_ctrl
            && data == format!("\x1b{raw}")
        {
            return true;
        }
        if modifier == ALT && !self.kitty && data == format!("\x1b{key}") {
            return true;
        }
        match modifier {
            CTRL => {
                raw_ctrl.as_deref() == Some(data)
                    || Self::kitty_matches(data, code, CTRL)
                    || Self::printable_modify_other_keys(data, code, CTRL)
            }
            SHIFT => {
                (letter && data == key.to_uppercase())
                    || Self::kitty_matches(data, code, SHIFT)
                    || Self::printable_modify_other_keys(data, code, SHIFT)
            }
            0 => data == key || Self::kitty_matches(data, code, 0),
            _ => {
                Self::kitty_matches(data, code, modifier)
                    || Self::printable_modify_other_keys(data, code, modifier)
            }
        }
    }

    /// The key id of `data`, if it is a recognized key.
    pub fn parse(&self, data: &str) -> Option<String> {
        if let Some(kitty) = parse_kitty(data) {
            return format_parsed(kitty.code, kitty.modifier, kitty.base_layout);
        }
        if let Some((code, modifier)) = parse_modify_other_keys(data) {
            return format_parsed(code, modifier, None);
        }
        if self.kitty && (data == "\x1b\r" || data == "\n") {
            return Some("shift+enter".into());
        }
        if let Some(key) = legacy_sequence_key(data) {
            return Some(key.into());
        }
        let fixed = match data {
            "\x1b" => Some("escape"),
            "\x1c" => Some("ctrl+\\"),
            "\x1d" => Some("ctrl+]"),
            "\x1f" => Some("ctrl+-"),
            "\x1b\x1b" => Some("ctrl+alt+["),
            "\x1b\x1c" => Some("ctrl+alt+\\"),
            "\x1b\x1d" => Some("ctrl+alt+]"),
            "\x1b\x1f" => Some("ctrl+alt+-"),
            "\t" => Some("tab"),
            "\r" | "\x1bOM" => Some("enter"),
            "\n" if !self.kitty => Some("enter"),
            "\x00" => Some("ctrl+space"),
            " " => Some("space"),
            "\x7f" => Some("backspace"),
            "\x08" if self.windows_terminal => Some("ctrl+backspace"),
            "\x08" => Some("backspace"),
            "\x1b[Z" => Some("shift+tab"),
            "\x1b\r" if !self.kitty => Some("alt+enter"),
            "\x1b " if !self.kitty => Some("alt+space"),
            "\x1b\x7f" | "\x1b\x08" => Some("alt+backspace"),
            "\x1bB" if !self.kitty => Some("alt+left"),
            "\x1bF" if !self.kitty => Some("alt+right"),
            _ => None,
        };
        if let Some(key) = fixed {
            return Some(key.into());
        }
        let chars: Vec<char> = data.chars().collect();
        if !self.kitty && chars.len() == 2 && chars[0] == '\x1b' {
            let code = chars[1] as u32;
            if (1..=26).contains(&code) {
                return char::from_u32(code + 96).map(|c| format!("ctrl+alt+{c}"));
            }
            let c = chars[1];
            if c.is_ascii_lowercase() || c.is_ascii_digit() || is_symbol(c) {
                return Some(format!("alt+{c}"));
            }
        }
        let fixed = match data {
            "\x1b[A" => Some("up"),
            "\x1b[B" => Some("down"),
            "\x1b[C" => Some("right"),
            "\x1b[D" => Some("left"),
            "\x1b[H" | "\x1bOH" => Some("home"),
            "\x1b[F" | "\x1bOF" => Some("end"),
            "\x1b[3~" => Some("delete"),
            "\x1b[5~" => Some("pageUp"),
            "\x1b[6~" => Some("pageDown"),
            _ => None,
        };
        if let Some(key) = fixed {
            return Some(key.into());
        }
        if chars.len() == 1 {
            let code = chars[0] as u32;
            if (1..=26).contains(&code) {
                return char::from_u32(code + 96).map(|c| format!("ctrl+{c}"));
            }
            if (32..=126).contains(&code) {
                return Some(data.to_owned());
            }
        }
        None
    }
}

fn format_parsed(code: i64, modifier: u32, base_layout: Option<i64>) -> Option<String> {
    let identity = normalize_shifted_letter(normalize_functional(code), modifier);
    let known =
        (97..=122).contains(&identity) || (48..=57).contains(&identity) || is_symbol_code(identity);
    let effective = if known {
        identity
    } else {
        base_layout.unwrap_or(identity)
    };
    let name = match effective {
        ESCAPE => "escape".to_owned(),
        TAB => "tab".to_owned(),
        ENTER | KP_ENTER => "enter".to_owned(),
        SPACE => "space".to_owned(),
        BACKSPACE => "backspace".to_owned(),
        DELETE => "delete".to_owned(),
        INSERT => "insert".to_owned(),
        HOME => "home".to_owned(),
        END => "end".to_owned(),
        PAGE_UP => "pageUp".to_owned(),
        PAGE_DOWN => "pageDown".to_owned(),
        UP => "up".to_owned(),
        DOWN => "down".to_owned(),
        LEFT => "left".to_owned(),
        RIGHT => "right".to_owned(),
        code if (48..=57).contains(&code) || (97..=122).contains(&code) || is_symbol_code(code) => {
            char::from_u32(u32::try_from(code).ok()?)?.to_string()
        }
        _ => return None,
    };
    format_with_modifiers(&name, modifier)
}

/// The printable text of a Kitty CSI-u or modifyOtherKeys sequence for a plain
/// or shifted key; `None` for other input.
pub fn decode_printable(data: &str) -> Option<String> {
    if let Some(kitty) = parse_csi_u(data) {
        let allowed = SHIFT | LOCK_MASK;
        if kitty.modifier & !allowed != 0 || kitty.modifier & (ALT | CTRL) != 0 {
            return None;
        }
        let mut code = kitty.code;
        if kitty.modifier & SHIFT != 0
            && let Some(shifted) = kitty.shifted
        {
            code = shifted;
        }
        let code = normalize_functional(code);
        if code < 32 {
            return None;
        }
        return char::from_u32(u32::try_from(code).ok()?).map(String::from);
    }
    let (code, modifier) = parse_modify_other_keys(data)?;
    if (modifier & !LOCK_MASK) & !SHIFT != 0 || code < 32 {
        return None;
    }
    char::from_u32(u32::try_from(code).ok()?).map(String::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    const LEGACY: Keys = Keys {
        kitty: false,
        windows_terminal: false,
    };
    const KITTY: Keys = Keys {
        kitty: true,
        windows_terminal: false,
    };

    #[test]
    fn matches_legacy_keys() {
        assert!(LEGACY.matches("\x03", "ctrl+c"));
        assert!(LEGACY.matches("\r", "enter"));
        assert!(LEGACY.matches("\n", "enter"));
        assert!(!KITTY.matches("\n", "enter"));
        assert!(KITTY.matches("\n", "shift+enter"));
        assert!(LEGACY.matches("\x1b\r", "alt+enter"));
        assert!(LEGACY.matches("\x1bb", "alt+left"));
        assert!(LEGACY.matches("\x1b[1;5D", "ctrl+left"));
        assert!(LEGACY.matches("\x7f", "backspace"));
        assert!(LEGACY.matches("\x1f", "ctrl+-"));
        assert!(LEGACY.matches("A", "shift+a"));
        assert!(LEGACY.matches("\x1bx", "alt+x"));
        assert!(LEGACY.matches("\x1b[5~", "pageUp"));
    }

    #[test]
    fn matches_kitty_sequences() {
        assert!(KITTY.matches("\x1b[13;2u", "shift+enter"));
        assert!(KITTY.matches("\x1b[99;5u", "ctrl+c"));
        assert!(KITTY.matches("\x1b[99;133u", "ctrl+c"), "caps lock ignored");
        assert!(KITTY.matches("\x1b[1;5A", "ctrl+up"));
        assert!(KITTY.matches("\x1b[3;3~", "alt+delete"));
        assert!(KITTY.matches("\x1b[57414u", "enter"), "keypad enter");
        // Cyrillic с reported with base layout c.
        assert!(KITTY.matches("\x1b[1089::99;5u", "ctrl+c"));
        assert!(LEGACY.matches("\x1b[27;2;13~", "shift+enter"));
    }

    #[test]
    fn parses_key_ids() {
        assert_eq!(LEGACY.parse("\x03").as_deref(), Some("ctrl+c"));
        assert_eq!(LEGACY.parse("\x1b[13;2u").as_deref(), Some("shift+enter"));
        assert_eq!(LEGACY.parse("\x1b[1;5A").as_deref(), Some("ctrl+up"));
        assert_eq!(LEGACY.parse("\x1b[5~").as_deref(), Some("pageUp"));
        assert_eq!(LEGACY.parse("\x1bx").as_deref(), Some("alt+x"));
        assert_eq!(LEGACY.parse("\x1b\x01").as_deref(), Some("ctrl+alt+a"));
        assert_eq!(LEGACY.parse("q").as_deref(), Some("q"));
        assert_eq!(LEGACY.parse("\x1b[65;2u").as_deref(), Some("shift+a"));
        assert_eq!(LEGACY.parse("é"), None);
    }

    #[test]
    fn decodes_printable_and_events() {
        assert_eq!(decode_printable("\x1b[97u").as_deref(), Some("a"));
        assert_eq!(decode_printable("\x1b[97:65;2u").as_deref(), Some("A"));
        assert_eq!(decode_printable("\x1b[97;5u"), None);
        assert!(is_key_release("\x1b[97;1:3u"));
        assert!(is_key_repeat("\x1b[97;1:2u"));
        assert!(!is_key_release("\x1b[200~90:62:3F"));
    }
}
