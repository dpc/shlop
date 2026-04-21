//! Terminal color type with JSON5-friendly deserialization.
//!
//! Colors can be specified as:
//! - Named: `"red"`, `"dark_blue"`, `"grey"`, etc.
//! - Hex RGB: `"#ff8800"`

use std::fmt;

/// A terminal color.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Color {
    Black,
    DarkRed,
    DarkGreen,
    DarkYellow,
    DarkBlue,
    DarkMagenta,
    DarkCyan,
    DarkGrey,
    Red,
    Green,
    Yellow,
    Blue,
    Magenta,
    Cyan,
    White,
    Grey,
    Rgb { r: u8, g: u8, b: u8 },
}

impl Color {
    /// Parses a color from a string (named or `#rrggbb` hex).
    pub fn parse(s: &str) -> Result<Self, ColorParseError> {
        if let Some(hex) = s.strip_prefix('#') {
            return Self::parse_hex(hex);
        }
        match s.to_lowercase().as_str() {
            "black" => Ok(Self::Black),
            "dark_red" | "darkred" => Ok(Self::DarkRed),
            "dark_green" | "darkgreen" => Ok(Self::DarkGreen),
            "dark_yellow" | "darkyellow" => Ok(Self::DarkYellow),
            "dark_blue" | "darkblue" => Ok(Self::DarkBlue),
            "dark_magenta" | "darkmagenta" => Ok(Self::DarkMagenta),
            "dark_cyan" | "darkcyan" => Ok(Self::DarkCyan),
            "dark_grey" | "darkgrey" | "dark_gray" | "darkgray" => Ok(Self::DarkGrey),
            "red" => Ok(Self::Red),
            "green" => Ok(Self::Green),
            "yellow" => Ok(Self::Yellow),
            "blue" => Ok(Self::Blue),
            "magenta" => Ok(Self::Magenta),
            "cyan" => Ok(Self::Cyan),
            "white" => Ok(Self::White),
            "grey" | "gray" => Ok(Self::Grey),
            _ => Err(ColorParseError(s.to_owned())),
        }
    }

    fn parse_hex(hex: &str) -> Result<Self, ColorParseError> {
        if hex.len() != 6 {
            return Err(ColorParseError(format!("#{hex}")));
        }
        let r =
            u8::from_str_radix(&hex[0..2], 16).map_err(|_| ColorParseError(format!("#{hex}")))?;
        let g =
            u8::from_str_radix(&hex[2..4], 16).map_err(|_| ColorParseError(format!("#{hex}")))?;
        let b =
            u8::from_str_radix(&hex[4..6], 16).map_err(|_| ColorParseError(format!("#{hex}")))?;
        Ok(Self::Rgb { r, g, b })
    }
}

/// Error returned when a color string cannot be parsed.
#[derive(Clone, Debug)]
pub struct ColorParseError(String);

impl fmt::Display for ColorParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid color: {:?}", self.0)
    }
}

impl std::error::Error for ColorParseError {}

impl<'de> serde::Deserialize<'de> for Color {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        Color::parse(&s).map_err(serde::de::Error::custom)
    }
}
