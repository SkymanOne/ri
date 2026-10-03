//! Theme colors: parsing, OKHSL and OKLCH conversion, mixing and 256-color
//! quantization.
//!
//! Port of `packages/tui/src/colors.ts` and `packages/tui/src/oklab.ts` in pi
//! `v1.0.0`. The Oklab and OKHSL conversions follow Björn Ottosson's reference
//! implementation (<https://bottosson.github.io/posts/colorpicker/>),
//! Copyright (c) 2021 Björn Ottosson, MIT License.

use std::f64::consts::PI;

/// A concrete color.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Color {
    /// An entry of the 256-color palette.
    Indexed(u8),
    /// sRGB channels, 0 to 255.
    Rgb(f64, f64, f64),
    /// OKLCH lightness (0 to 1), chroma and hue in degrees.
    Oklch(f64, f64, f64),
}

/// How many colors the terminal shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColorMode {
    /// 24-bit color.
    TrueColor,
    /// The 256-color palette.
    Ansi256,
}

impl ColorMode {
    /// pi's name for the mode.
    pub fn as_str(self) -> &'static str {
        match self {
            ColorMode::TrueColor => "truecolor",
            ColorMode::Ansi256 => "256color",
        }
    }
}

/// sRGB channels, 0 to 255.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rgb {
    /// Red.
    pub r: u8,
    /// Green.
    pub g: u8,
    /// Blue.
    pub b: u8,
}

type Vector = [f64; 3];
type Matrix = [Vector; 3];

fn multiply(m: &Matrix, v: Vector) -> Vector {
    [0, 1, 2].map(|row| m[row][0] * v[0] + m[row][1] * v[1] + m[row][2] * v[2])
}

const LINEAR_SRGB_TO_LMS: Matrix = [
    [0.4122214694707629, 0.5363325372617349, 0.0514459932675022],
    [0.2119034958178251, 0.6806995506452344, 0.1073969535369405],
    [0.0883024591900564, 0.2817188391361215, 0.6299787016738222],
];
const LMS_TO_LAB: Matrix = [
    [0.210454268309314, 0.793617774702305, -0.0040720430116193],
    [1.9779985324311684, -2.42859224204858, 0.450593709617411],
    [0.0259040424655478, 0.7827717124575296, -0.8086757549230774],
];
const LAB_TO_LMS: Matrix = [
    [1.0, 0.3963377773761749, 0.2158037573099136],
    [1.0, -0.1055613458156586, -0.0638541728258133],
    [1.0, -0.0894841775298119, -1.2914855480194092],
];
#[allow(
    clippy::excessive_precision,
    reason = "digits as in the reference implementation; f64 rounds them identically"
)]
const LMS_TO_LINEAR_SRGB: Matrix = [
    [4.0767416360759583, -3.3077115392580629, 0.2309699031821043],
    [-1.2684379732850315, 2.6097573492876882, -0.341319376002657],
    [-0.0041960761386756, -0.7034186179359362, 1.7076146940746117],
];
type SaturationFit = ([f64; 2], [f64; 5]);
const SATURATION_FIT: [SaturationFit; 3] = [
    (
        [-1.8817031, -0.80936501],
        [1.19086277, 1.76576728, 0.59662641, 0.75515197, 0.56771245],
    ),
    (
        [1.8144408, -1.19445267],
        [0.73956515, -0.45954404, 0.08285427, 0.12541073, -0.14503204],
    ),
    (
        [0.13110758, 1.81333971],
        [1.35733652, -0.00915799, -1.1513021, -0.50559606, 0.00692167],
    ),
];
const K1: f64 = 0.206;
const K2: f64 = 0.03;
const K3: f64 = (1.0 + K1) / (1.0 + K2);

fn okhsl_to_oklab_lightness(x: f64) -> f64 {
    (x * x + K1 * x) / (K3 * (x + K2))
}

fn linear_to_srgb(value: f64) -> f64 {
    if value > 0.0031308 {
        1.055 * value.powf(1.0 / 2.4) - 0.055
    } else {
        12.92 * value
    }
}

fn srgb_to_linear(value: f64) -> f64 {
    if value <= 0.04045 {
        value / 12.92
    } else {
        ((value + 0.055) / 1.055).powf(2.4)
    }
}

fn oklab_to_linear_srgb(lab: Vector) -> Vector {
    multiply(
        &LMS_TO_LINEAR_SRGB,
        multiply(&LAB_TO_LMS, lab).map(|v| v.powi(3)),
    )
}

fn linear_srgb_to_oklab(rgb: Vector) -> Vector {
    multiply(
        &LMS_TO_LAB,
        multiply(&LINEAR_SRGB_TO_LMS, rgb).map(f64::cbrt),
    )
}

fn rgb_to_oklab(rgb: [f64; 3]) -> Vector {
    linear_srgb_to_oklab(rgb.map(|channel| srgb_to_linear(channel / 255.0)))
}

/// JavaScript's `Math.round`: halves round up.
fn js_round(value: f64) -> f64 {
    (value + 0.5).floor()
}

fn linear_srgb_to_rgb(linear: Vector) -> [f64; 3] {
    linear.map(|value| js_round(linear_to_srgb(value).clamp(0.0, 1.0) * 255.0))
}

fn lms_slopes(a: f64, b: f64) -> Vector {
    [0, 1, 2].map(|row| LAB_TO_LMS[row][1] * a + LAB_TO_LMS[row][2] * b)
}

fn max_saturation(a: f64, b: f64) -> f64 {
    let channel = SATURATION_FIT
        .iter()
        .enumerate()
        .position(|(index, ([x, y], _))| index == 2 || x * a + y * b > 1.0)
        .unwrap_or(2);
    let [k0, k1, k2, k3, k4] = SATURATION_FIT[channel].1;
    let weights = LMS_TO_LINEAR_SRGB[channel];
    let saturation = k0 + k1 * a + k2 * b + k3 * a * a + k4 * a * b;
    let slopes = lms_slopes(a, b);
    let base = slopes.map(|k| 1.0 + saturation * k);
    let dot = |values: [f64; 3]| -> f64 { (0..3).map(|i| weights[i] * values[i]).sum() };
    let f = dot(base.map(|v| v.powi(3)));
    let f1 = dot([0, 1, 2].map(|i| 3.0 * slopes[i] * base[i].powi(2)));
    let f2 = dot([0, 1, 2].map(|i| 6.0 * slopes[i].powi(2) * base[i]));
    saturation - (f * f1) / (f1 * f1 - 0.5 * f * f2)
}

fn cusp(a: f64, b: f64) -> (f64, f64) {
    let saturation = max_saturation(a, b);
    let rgb = oklab_to_linear_srgb([1.0, saturation * a, saturation * b]);
    let lightness = (1.0 / rgb[0].max(rgb[1]).max(rgb[2])).cbrt();
    (lightness, lightness * saturation)
}

fn max_chroma(a: f64, b: f64, lightness: f64, (cusp_l, cusp_c): (f64, f64)) -> f64 {
    if lightness <= cusp_l {
        return cusp_c * lightness / cusp_l;
    }
    let t = cusp_c * (lightness - 1.0) / (cusp_l - 1.0);
    let slopes = lms_slopes(a, b);
    let lms = slopes.map(|k| lightness + t * k);
    let cubes = lms.map(|v| v.powi(3));
    let first = [0, 1, 2].map(|i| 3.0 * slopes[i] * lms[i].powi(2));
    let second = [0, 1, 2].map(|i| 6.0 * slopes[i].powi(2) * lms[i]);
    let dot = |row: &Vector, values: &[f64; 3]| {
        row[0] * values[0] + row[1] * values[1] + row[2] * values[2]
    };
    let step = LMS_TO_LINEAR_SRGB
        .iter()
        .map(|row| {
            let f = dot(row, &cubes) - 1.0;
            let f1 = dot(row, &first);
            let f2 = dot(row, &second);
            let u = f1 / (f1 * f1 - 0.5 * f * f2);
            if u >= 0.0 { -f * u } else { f64::MAX }
        })
        .fold(f64::INFINITY, f64::min);
    t + step
}

fn chroma_stops(l: f64, a: f64, b: f64) -> (f64, f64, f64) {
    let peak = cusp(a, b);
    let c_max = max_chroma(a, b, l, peak);
    let k = c_max / (l * (peak.1 / peak.0)).min((1.0 - l) * (peak.1 / (1.0 - peak.0)));
    let mid_s = 0.11516993
        + 1.0
            / (7.4477897
                + 4.1590124 * b
                + a * (-2.19557347
                    + 1.75198401 * b
                    + a * (-2.13704948 - 10.02301043 * b
                        + a * (-4.24894561 + 5.38770819 * b + 4.69891013 * a))));
    let mid_t = 0.11239642
        + 1.0
            / (1.6132032 - 0.68124379 * b
                + a * (0.40370612
                    + 0.90148123 * b
                    + a * (-0.27087943
                        + 0.6122399 * b
                        + a * (0.00299215 - 0.45399568 * b - 0.14661872 * a))));
    let c_mid = 0.9
        * k
        * (1.0 / (1.0 / (l * mid_s).powi(4) + 1.0 / ((1.0 - l) * mid_t).powi(4)))
            .sqrt()
            .sqrt();
    let c0 = (1.0 / (1.0 / (l * 0.4).powi(2) + 1.0 / ((1.0 - l) * 0.8).powi(2))).sqrt();
    (c0, c_mid, c_max)
}

/// OKHSL (hue in degrees, saturation and lightness 0 to 1) to sRGB.
pub fn okhsl_to_rgb(hue: f64, saturation: f64, lightness: f64) -> [f64; 3] {
    let l = okhsl_to_oklab_lightness(lightness);
    let mut lab = [l, 0.0, 0.0];
    if l > 0.0 && l < 1.0 && saturation > 0.0 {
        let angle = 2.0 * PI * (((hue % 360.0) + 360.0) % 360.0) / 360.0;
        let (a, b) = (angle.cos(), angle.sin());
        let (c0, c_mid, c_max) = chroma_stops(l, a, b);
        let chroma = if saturation < 0.8 {
            let t = 1.25 * saturation;
            let k1 = 0.8 * c0;
            t * k1 / (1.0 - (1.0 - k1 / c_mid) * t)
        } else {
            let t = 5.0 * (saturation - 0.8);
            let k1 = 0.2 * c_mid.powi(2) * 1.25f64.powi(2) / c0;
            c_mid + t * k1 / (1.0 - (1.0 - k1 / (c_max - c_mid)) * t)
        };
        lab = [l, chroma * a, chroma * b];
    }
    linear_srgb_to_rgb(oklab_to_linear_srgb(lab))
}

/// Oklab lightness to OKHSL lightness.
pub fn oklab_to_okhsl_lightness(x: f64) -> f64 {
    0.5 * (K3 * x - K1 + ((K3 * x - K1).powi(2) + 4.0 * K2 * K3 * x).sqrt())
}

/// sRGB to OKHSL: hue in degrees (0 for grays), saturation and lightness 0 to 1.
pub fn rgb_to_okhsl(rgb: [f64; 3]) -> (f64, f64, f64) {
    let [l, a, b] = rgb_to_oklab(rgb);
    let chroma = a.hypot(b);
    let lightness = oklab_to_okhsl_lightness(l);
    if chroma < 1e-9 || lightness <= 0.0 || lightness >= 1.0 {
        return (0.0, 0.0, lightness);
    }
    let hue = (b.atan2(a) * 180.0 / PI + 360.0) % 360.0;
    let (c0, c_mid, c_max) = chroma_stops(l, a / chroma, b / chroma);
    let saturation = if chroma < c_mid {
        let k1 = 0.8 * c0;
        0.8 * (chroma / (k1 + (1.0 - k1 / c_mid) * chroma))
    } else {
        let k1 = 0.2 * c_mid.powi(2) * 1.25f64.powi(2) / c0;
        let offset = chroma - c_mid;
        0.8 + 0.2 * (offset / (k1 + (1.0 - k1 / (c_max - c_mid)) * offset))
    };
    (hue, saturation.clamp(0.0, 1.0), lightness)
}

fn in_gamut(linear: Vector) -> bool {
    let epsilon = 1e-7;
    linear
        .iter()
        .all(|channel| *channel >= -epsilon && *channel <= 1.0 + epsilon)
}

fn oklch_to_rgb(l: f64, c: f64, h: f64) -> [f64; 3] {
    let radians = h * PI / 180.0;
    let (cos, sin) = (radians.cos(), radians.sin());
    let at = |chroma: f64| oklab_to_linear_srgb([l, chroma * cos, chroma * sin]);
    let direct = at(c);
    if in_gamut(direct) {
        return linear_srgb_to_rgb(direct);
    }
    let mut linear = at(0.0);
    let (mut low, mut high) = (0.0, c);
    for _ in 0..20 {
        let chroma = (low + high) / 2.0;
        let candidate = at(chroma);
        if in_gamut(candidate) {
            low = chroma;
            linear = candidate;
        } else {
            high = chroma;
        }
    }
    linear_srgb_to_rgb(linear)
}

const BASIC_COLORS: [[f64; 3]; 16] = [
    [0.0, 0.0, 0.0],
    [128.0, 0.0, 0.0],
    [0.0, 128.0, 0.0],
    [128.0, 128.0, 0.0],
    [0.0, 0.0, 128.0],
    [128.0, 0.0, 128.0],
    [0.0, 128.0, 128.0],
    [192.0, 192.0, 192.0],
    [128.0, 128.0, 128.0],
    [255.0, 0.0, 0.0],
    [0.0, 255.0, 0.0],
    [255.0, 255.0, 0.0],
    [0.0, 0.0, 255.0],
    [255.0, 0.0, 255.0],
    [0.0, 255.0, 255.0],
    [255.0, 255.0, 255.0],
];
const CUBE_VALUES: [f64; 6] = [0.0, 95.0, 135.0, 175.0, 215.0, 255.0];

fn indexed_to_rgb(index: u8) -> [f64; 3] {
    match index {
        0..=15 => BASIC_COLORS[usize::from(index)],
        16..=231 => {
            let cube = usize::from(index - 16);
            [
                CUBE_VALUES[cube / 36],
                CUBE_VALUES[(cube % 36) / 6],
                CUBE_VALUES[cube % 6],
            ]
        }
        _ => {
            let gray = 8.0 + f64::from(index - 232) * 10.0;
            [gray, gray, gray]
        }
    }
}

/// Error for a color string pi would reject.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("Invalid color value: {0}")]
pub struct InvalidColor(pub String);

fn number(text: &str) -> Option<f64> {
    let value: f64 = text.parse().ok()?;
    value.is_finite().then_some(value)
}

/// Splits `name(args)` case-insensitively into its whitespace-separated args.
fn function_args<'a>(value: &'a str, name: &str) -> Option<Vec<&'a str>> {
    let head = value.get(..name.len() + 1)?;
    if !head.eq_ignore_ascii_case(&format!("{name}(")) {
        return None;
    }
    let inner = value[name.len() + 1..].strip_suffix(')')?;
    Some(inner.split_whitespace().collect())
}

/// A number with an optional `%` (divided by 100) or, when `deg` is set, an
/// optional `deg` suffix.
fn component(text: &str, deg: bool) -> Option<(f64, bool)> {
    if let Some(stripped) = text.strip_suffix('%') {
        return Some((number(stripped)?, true));
    }
    let lower = text.to_ascii_lowercase();
    let text = if deg {
        lower.strip_suffix("deg").unwrap_or(&lower)
    } else {
        &lower
    };
    Some((number(text)?, false))
}

impl Color {
    /// Parses a theme color: `#rgb`, `#rrggbb`, `oklch(L C H)`, `okhsl(H S L)`.
    pub fn parse(value: &str) -> Result<Color, InvalidColor> {
        let invalid = || InvalidColor(value.to_owned());
        if let Some(hex) = value.strip_prefix('#')
            && (hex.len() == 3 || hex.len() == 6)
            && hex.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            let digits: String = if hex.len() == 3 {
                hex.chars().flat_map(|c| [c, c]).collect()
            } else {
                hex.to_owned()
            };
            let channel =
                |i: usize| f64::from(u8::from_str_radix(&digits[i..i + 2], 16).unwrap_or(0));
            return Ok(Color::Rgb(channel(0), channel(2), channel(4)));
        }
        if let Some(args) = function_args(value, "oklch") {
            let [l, c, h] = args.as_slice() else {
                return Err(invalid());
            };
            let (l, percent) = component(l, false).ok_or_else(invalid)?;
            let l = if percent { l / 100.0 } else { l };
            let (c, c_percent) = component(c, false).ok_or_else(invalid)?;
            let (h, h_percent) = component(h, true).ok_or_else(invalid)?;
            if c_percent || h_percent || !(0.0..=1.0).contains(&l) || c < 0.0 {
                return Err(invalid());
            }
            return Ok(Color::Oklch(l, c, ((h % 360.0) + 360.0) % 360.0));
        }
        if let Some(args) = function_args(value, "okhsl") {
            let [h, s, l] = args.as_slice() else {
                return Err(invalid());
            };
            let (h, h_percent) = component(h, true).ok_or_else(invalid)?;
            let (s, s_percent) = component(s, false).ok_or_else(invalid)?;
            let (l, l_percent) = component(l, false).ok_or_else(invalid)?;
            let s = if s_percent { s / 100.0 } else { s };
            let l = if l_percent { l / 100.0 } else { l };
            if h_percent || !(0.0..=1.0).contains(&s) || !(0.0..=1.0).contains(&l) {
                return Err(invalid());
            }
            let [r, g, b] = okhsl_to_rgb(h, s, l);
            return Ok(Color::Rgb(r, g, b));
        }
        Err(invalid())
    }

    /// sRGB channels.
    pub fn to_rgb(self) -> [f64; 3] {
        match self {
            Color::Indexed(index) => indexed_to_rgb(index),
            Color::Rgb(r, g, b) => [r, g, b],
            Color::Oklch(l, c, h) => oklch_to_rgb(l, c, h),
        }
    }

    /// OKLCH channels.
    pub fn to_oklch(self) -> (f64, f64, f64) {
        if let Color::Oklch(l, c, h) = self {
            return (l, c, h);
        }
        let [l, a, b] = rgb_to_oklab(self.to_rgb());
        (l, a.hypot(b), (b.atan2(a) * 180.0 / PI + 360.0) % 360.0)
    }

    /// `amount` (0 to 1) of the way from `self` to `other`, in OKLCH.
    pub fn mix(self, other: Color, amount: f64) -> Color {
        let (al, ac, ah) = self.to_oklch();
        let (bl, bc, bh) = other.to_oklch();
        let first_hue = if ac < 1e-7 { bh } else { ah };
        let second_hue = if bc < 1e-7 { first_hue } else { bh };
        let delta = ((second_hue - first_hue + 540.0) % 360.0) - 180.0;
        let hue = first_hue + delta * amount;
        Color::Oklch(
            al + (bl - al) * amount,
            ac + (bc - ac) * amount,
            ((hue % 360.0) + 360.0) % 360.0,
        )
    }

    /// The color for a terminal with `mode`.
    pub fn to_terminal(self, mode: ColorMode) -> ratatui_core::style::Color {
        if let Color::Indexed(index) = self {
            return ratatui_core::style::Color::Indexed(index);
        }
        let rgb = self.to_rgb();
        match mode {
            ColorMode::TrueColor => {
                let [r, g, b] = rgb.map(|channel| js_round(channel).clamp(0.0, 255.0) as u8);
                ratatui_core::style::Color::Rgb(r, g, b)
            }
            ColorMode::Ansi256 => ratatui_core::style::Color::Indexed(rgb_to_ansi256(rgb)),
        }
    }
}

fn closest(values: &[f64], target: f64) -> usize {
    let mut best = 0;
    let mut distance = f64::INFINITY;
    for (index, value) in values.iter().enumerate() {
        let d = (target - value).abs();
        if d < distance {
            best = index;
            distance = d;
        }
    }
    best
}

fn color_distance(a: [f64; 3], b: [f64; 3]) -> f64 {
    let (dr, dg, db) = (a[0] - b[0], a[1] - b[1], a[2] - b[2]);
    dr * dr * 0.299 + dg * dg * 0.587 + db * db * 0.114
}

/// The nearest 256-color palette entry.
pub fn rgb_to_ansi256(rgb: [f64; 3]) -> u8 {
    let [r, g, b] = rgb.map(|channel| closest(&CUBE_VALUES, channel));
    let cube = [CUBE_VALUES[r], CUBE_VALUES[g], CUBE_VALUES[b]];
    let cube_index = 16 + 36 * r + 6 * g + b;
    let grays: Vec<f64> = (0..24).map(|i| 8.0 + f64::from(i) * 10.0).collect();
    let gray = js_round(0.299 * rgb[0] + 0.587 * rgb[1] + 0.114 * rgb[2]);
    let offset = closest(&grays, gray);
    let value = grays[offset];
    let spread = rgb[0].max(rgb[1]).max(rgb[2]) - rgb[0].min(rgb[1]).min(rgb[2]);
    if spread < 10.0 && color_distance(rgb, [value; 3]) < color_distance(rgb, cube) {
        return (232 + offset) as u8;
    }
    cube_index as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_theme_colors() {
        assert_eq!(Color::parse("#fa0").unwrap().to_rgb(), [255.0, 170.0, 0.0]);
        assert_eq!(
            Color::parse("#102030").unwrap().to_rgb(),
            [16.0, 32.0, 48.0]
        );
        assert!(Color::parse("okhsl(234 3% 89%)").is_ok());
        assert!(Color::parse("OKLCH(70% 0.1 120deg)").is_ok());
        assert!(Color::parse("okhsl(1 2 3)").is_err());
        assert!(Color::parse("red").is_err());
        assert_eq!(rgb_to_ansi256([0.0, 0.0, 0.0]), 16);
        assert_eq!(rgb_to_ansi256([128.0, 128.0, 128.0]), 244);
    }
}
