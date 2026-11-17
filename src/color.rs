//! Color-space helpers for the color picker and eyedropper: HSV ⇄ RGB, hex
//! strings and the text formats colors are copied in.

use crate::document::Rgba;

/// Hue in degrees [0, 360), saturation and value in [0, 1].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Hsv {
    pub h: f32,
    pub s: f32,
    pub v: f32,
}

impl Hsv {
    pub fn to_rgb(self) -> [u8; 3] {
        let h = self.h.rem_euclid(360.0) / 60.0;
        let (s, v) = (self.s.clamp(0.0, 1.0), self.v.clamp(0.0, 1.0));
        let c = v * s;
        let x = c * (1.0 - (h % 2.0 - 1.0).abs());
        let (r, g, b) = match h as u32 {
            0 => (c, x, 0.0),
            1 => (x, c, 0.0),
            2 => (0.0, c, x),
            3 => (0.0, x, c),
            4 => (x, 0.0, c),
            _ => (c, 0.0, x),
        };
        let m = v - c;
        [r, g, b].map(|ch| ((ch + m) * 255.0).round() as u8)
    }

    /// HSV of an RGB color. Hue and saturation are undefined for greys and
    /// black; `keep` supplies them so dragging value to 0 and back does not
    /// lose the hue the user picked.
    pub fn from_rgb(rgb: [u8; 3], keep: Hsv) -> Hsv {
        let [r, g, b] = rgb.map(|c| c as f32 / 255.0);
        let max = r.max(g).max(b);
        let min = r.min(g).min(b);
        let d = max - min;
        let v = max;
        if max <= 0.0 {
            return Hsv { v: 0.0, ..keep };
        }
        let s = d / max;
        if d <= 0.0 {
            return Hsv {
                h: keep.h,
                s: 0.0,
                v,
            };
        }
        let h = if max == r {
            60.0 * ((g - b) / d).rem_euclid(6.0)
        } else if max == g {
            60.0 * ((b - r) / d + 2.0)
        } else {
            60.0 * ((r - g) / d + 4.0)
        };
        Hsv { h, s, v }
    }
}

/// `#rrggbb`, or `#rrggbbaa` when not opaque.
pub fn to_hex(c: Rgba) -> String {
    if c.a == 255 {
        format!("#{:02x}{:02x}{:02x}", c.r, c.g, c.b)
    } else {
        format!("#{:02x}{:02x}{:02x}{:02x}", c.r, c.g, c.b, c.a)
    }
}

/// Parses `#rgb`, `#rrggbb` or `#rrggbbaa` (the `#` is optional).
pub fn parse_hex(s: &str) -> Option<Rgba> {
    let hex = s.trim().trim_start_matches('#');
    if !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let byte = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).ok();
    match hex.len() {
        3 => {
            let n = |i: usize| u8::from_str_radix(&hex[i..=i], 16).ok().map(|v| v * 17);
            Some(Rgba {
                r: n(0)?,
                g: n(1)?,
                b: n(2)?,
                a: 255,
            })
        }
        6 => Some(Rgba {
            r: byte(0)?,
            g: byte(2)?,
            b: byte(4)?,
            a: 255,
        }),
        8 => Some(Rgba {
            r: byte(0)?,
            g: byte(2)?,
            b: byte(4)?,
            a: byte(6)?,
        }),
        _ => None,
    }
}

/// A text format the eyedropper copies colors in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Hex,
    Rgb,
    Cmyk,
    Hsv,
    Hsl,
}

impl Format {
    pub const ALL: [Format; 5] = [
        Format::Hex,
        Format::Rgb,
        Format::Cmyk,
        Format::Hsv,
        Format::Hsl,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Format::Hex => "HEX",
            Format::Rgb => "RGB",
            Format::Cmyk => "CMYK",
            Format::Hsv => "HSV",
            Format::Hsl => "HSL",
        }
    }

    /// `c` as text, rounded to whole units: `#0067ed`, `0, 103, 237`,
    /// `100%, 57%, 0%, 7%`, `214°, 100%, 93%`, `214°, 100%, 46%`. Only hex
    /// carries alpha; the other formats describe the color channels alone.
    pub fn format(self, c: Rgba) -> String {
        let pct = |x: f32| (x * 100.0).round() as u32;
        // 359.6° rounds to 360°, which is 0°.
        let deg = |h: f32| h.round() as u32 % 360;
        let [r, g, b] = [c.r, c.g, c.b].map(|v| v as f32 / 255.0);
        let (max, min) = (r.max(g).max(b), r.min(g).min(b));
        let hue = || {
            Hsv::from_rgb(
                [c.r, c.g, c.b],
                Hsv {
                    h: 0.0,
                    s: 0.0,
                    v: 0.0,
                },
            )
            .h
        };
        match self {
            Format::Hex => to_hex(c),
            Format::Rgb => format!("{}, {}, {}", c.r, c.g, c.b),
            Format::Cmyk => {
                let k = 1.0 - max;
                let ink = |ch: f32| if k >= 1.0 { 0.0 } else { (max - ch) / max };
                format!(
                    "{}%, {}%, {}%, {}%",
                    pct(ink(r)),
                    pct(ink(g)),
                    pct(ink(b)),
                    pct(k)
                )
            }
            Format::Hsv => {
                let s = if max > 0.0 { (max - min) / max } else { 0.0 };
                format!("{}°, {}%, {}%", deg(hue()), pct(s), pct(max))
            }
            Format::Hsl => {
                let l = (max + min) / 2.0;
                let d = max - min;
                let s = if d > 0.0 {
                    d / (1.0 - (2.0 * l - 1.0).abs())
                } else {
                    0.0
                };
                format!("{}°, {}%, {}%", deg(hue()), pct(s), pct(l))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn primaries_and_greys() {
        let hsv = |h, s, v| Hsv { h, s, v };
        assert_eq!(hsv(0.0, 1.0, 1.0).to_rgb(), [255, 0, 0]);
        assert_eq!(hsv(120.0, 1.0, 1.0).to_rgb(), [0, 255, 0]);
        assert_eq!(hsv(240.0, 1.0, 1.0).to_rgb(), [0, 0, 255]);
        assert_eq!(hsv(360.0, 1.0, 1.0).to_rgb(), [255, 0, 0], "hue wraps");
        assert_eq!(hsv(77.0, 0.0, 0.5).to_rgb(), [128, 128, 128]);
    }

    #[test]
    fn rgb_round_trips_through_hsv() {
        let keep = Hsv {
            h: 0.0,
            s: 0.0,
            v: 0.0,
        };
        for rgb in [
            [224, 49, 49],
            [12, 200, 77],
            [1, 2, 3],
            [255, 255, 0],
            [90, 10, 200],
        ] {
            assert_eq!(Hsv::from_rgb(rgb, keep).to_rgb(), rgb, "{rgb:?}");
        }
    }

    #[test]
    fn greys_keep_the_previous_hue() {
        let keep = Hsv {
            h: 210.0,
            s: 0.8,
            v: 1.0,
        };
        assert_eq!(Hsv::from_rgb([0, 0, 0], keep).h, 210.0);
        assert_eq!(Hsv::from_rgb([0, 0, 0], keep).s, 0.8);
        let grey = Hsv::from_rgb([100, 100, 100], keep);
        assert_eq!((grey.h, grey.s), (210.0, 0.0));
    }

    #[test]
    fn hex_parsing_and_formatting() {
        let c = Rgba {
            r: 0xe0,
            g: 0x31,
            b: 0x31,
            a: 255,
        };
        assert_eq!(to_hex(c), "#e03131");
        assert_eq!(parse_hex("#e03131"), Some(c));
        assert_eq!(parse_hex("E03131"), Some(c));
        let t = Rgba { a: 0x80, ..c };
        assert_eq!(to_hex(t), "#e0313180");
        assert_eq!(parse_hex(&to_hex(t)), Some(t));
        assert_eq!(
            parse_hex("#fa0").map(|c| (c.r, c.g, c.b)),
            Some((255, 170, 0))
        );
        for bad in ["", "#12345", "#ggg", "#1234567", "red", "#ab cd e"] {
            assert_eq!(parse_hex(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn text_formats_match_common_converters() {
        let rgb = |r, g, b| Rgba { r, g, b, a: 255 };
        let all = |c| Format::ALL.map(|f| f.format(c));
        assert_eq!(
            all(rgb(0, 103, 237)),
            [
                "#0067ed",
                "0, 103, 237",
                "100%, 57%, 0%, 7%",
                "214°, 100%, 93%",
                "214°, 100%, 46%",
            ]
        );
        // Black has no ink but K, and no hue or saturation.
        assert_eq!(
            all(rgb(0, 0, 0))[2..],
            ["0%, 0%, 0%, 100%", "0°, 0%, 0%", "0°, 0%, 0%"]
        );
        assert_eq!(
            all(rgb(255, 255, 255))[2..],
            ["0%, 0%, 0%, 0%", "0°, 0%, 100%", "0°, 0%, 100%"]
        );
        // A hue just under 360° rounds to 0°, not 360°.
        assert_eq!(Format::Hsl.format(rgb(255, 0, 1)), "0°, 100%, 50%");
        assert_eq!(Format::Hsl.format(rgb(255, 128, 0)), "30°, 100%, 50%");
        // Translucent pixels (images opened from files) keep alpha in hex.
        assert_eq!(
            Format::Hex.format(Rgba {
                a: 0x80,
                ..rgb(1, 2, 3)
            }),
            "#01020380"
        );
    }
}
