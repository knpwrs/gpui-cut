//! Parsing and formatting of timestamps.
//!
//! Everything in the app is measured in seconds as `f64`. This module turns
//! those seconds into human readable timecodes and back again, and implements
//! the small "jump" language accepted by the jump-to-timestamp field.

use std::fmt;

/// Formats seconds as `HH:MM:SS.mmm` (hours are always shown).
pub fn format(seconds: f64) -> String {
    format_with(seconds, true, 3)
}

/// Formats seconds as a compact timecode, omitting hours when zero
/// (e.g. `03:07.250` or `1:03:07.250`).
pub fn format_short(seconds: f64) -> String {
    format_with(seconds, false, 3)
}

/// Formats seconds using `:`-separated fields.
///
/// * `always_hours` — include the hour field even when it is zero.
/// * `decimals` — number of fractional digits (0..=6).
pub fn format_with(seconds: f64, always_hours: bool, decimals: usize) -> String {
    let decimals = decimals.min(6);
    let negative = seconds < 0.0;
    let scale = 10u64.pow(decimals as u32);
    // Round once on the smallest displayed unit so 59.9996 never prints as 00:59.1000.
    let total_units = (seconds.abs() * scale as f64).round() as u64;
    let frac = total_units % scale;
    let total_secs = total_units / scale;
    let h = total_secs / 3600;
    let m = (total_secs / 60) % 60;
    let s = total_secs % 60;

    let mut out = String::with_capacity(16);
    if negative && total_units > 0 {
        out.push('-');
    }
    if always_hours || h > 0 {
        if always_hours {
            out.push_str(&format!("{h:02}:{m:02}:{s:02}"));
        } else {
            out.push_str(&format!("{h}:{m:02}:{s:02}"));
        }
    } else {
        out.push_str(&format!("{m:02}:{s:02}"));
    }
    if decimals > 0 {
        out.push_str(&format!(".{frac:0width$}", width = decimals));
    }
    out
}

/// Formats seconds in a filename-safe way: `HH.MM.SS.mmm`.
pub fn format_for_filename(seconds: f64) -> String {
    format(seconds).replace(':', ".")
}

/// A human friendly duration such as `1h 02m 03s`, `4m 05s` or `12.5s`.
pub fn format_duration_human(seconds: f64) -> String {
    let seconds = seconds.max(0.0);
    if seconds < 60.0 {
        let rounded = (seconds * 10.0).round() / 10.0;
        if rounded.fract() == 0.0 {
            return format!("{rounded:.0}s");
        }
        return format!("{rounded:.1}s");
    }
    let total = seconds.round() as u64;
    let h = total / 3600;
    let m = (total / 60) % 60;
    let s = total % 60;
    if h > 0 { format!("{h}h {m:02}m {s:02}s") } else { format!("{m}m {s:02}s") }
}

/// Formats a timestamp for the ruler: drops the fractional part when the tick
/// spacing is whole seconds.
pub fn format_ruler(seconds: f64, step: f64) -> String {
    let decimals = if step >= 1.0 {
        0
    } else if step >= 0.1 {
        1
    } else if step >= 0.01 {
        2
    } else {
        3
    };
    format_with(seconds, false, decimals)
}

/// Errors produced while parsing a timestamp.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseError {
    Empty,
    Invalid(String),
    /// A frame number was given but the frame rate is unknown.
    NoFrameRate,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ParseError::Empty => write!(f, "Enter a time"),
            ParseError::Invalid(s) => write!(f, "\u{201c}{s}\u{201d} is not a valid time"),
            ParseError::NoFrameRate => write!(f, "Frame numbers need a video stream"),
        }
    }
}

impl std::error::Error for ParseError {}

fn parse_number(s: &str) -> Option<f64> {
    let s = s.trim().replace(',', ".");
    if s.is_empty() || s.starts_with('+') || s.starts_with('-') {
        return None;
    }
    if !s.chars().all(|c| c.is_ascii_digit() || c == '.') || s.matches('.').count() > 1 {
        return None;
    }
    if s == "." {
        return None;
    }
    s.parse::<f64>().ok()
}

/// Parses an absolute, unsigned timestamp.
///
/// Accepted forms:
/// * `HH:MM:SS(.fff)`, `MM:SS(.fff)`, `SS(.fff)` (a `,` decimal separator is
///   accepted too)
/// * unit suffixes: `1h2m3.5s`, `90s`, `2m`, `1h 30m`, `250ms`
pub fn parse(input: &str) -> Result<f64, ParseError> {
    let s = input.trim();
    if s.is_empty() {
        return Err(ParseError::Empty);
    }
    let invalid = || ParseError::Invalid(s.to_string());

    if s.contains(':') {
        let parts: Vec<&str> = s.split(':').collect();
        if parts.len() > 3 {
            return Err(invalid());
        }
        let mut total = 0.0;
        let last = parts.len() - 1;
        for (i, part) in parts.iter().enumerate() {
            let value = if i == last {
                parse_number(part).ok_or_else(invalid)?
            } else {
                // Leading fields must be whole numbers.
                if part.contains('.') || part.contains(',') {
                    return Err(invalid());
                }
                parse_number(part).ok_or_else(invalid)?
            };
            // Minutes and seconds fields must be below 60 when a larger unit is present.
            if i > 0 && value >= 60.0 {
                return Err(invalid());
            }
            total = total * 60.0 + value;
        }
        return Ok(total);
    }

    if let Some(v) = parse_number(s) {
        return Ok(v);
    }

    parse_units(s).ok_or_else(invalid)
}

fn parse_units(s: &str) -> Option<f64> {
    let compact: String = s.chars().filter(|c| !c.is_whitespace()).collect();
    let lower = compact.to_ascii_lowercase();
    let mut total = 0.0;
    let mut number = String::new();
    let mut chars = lower.chars().peekable();
    let mut saw_unit = false;
    while let Some(c) = chars.next() {
        if c.is_ascii_digit() || c == '.' || c == ',' {
            number.push(c);
            continue;
        }
        let value = parse_number(&number)?;
        number.clear();
        let multiplier = match c {
            'h' => 3600.0,
            'm' if chars.peek() == Some(&'s') => {
                chars.next();
                0.001
            }
            'm' => 60.0,
            's' => 1.0,
            _ => return None,
        };
        total += value * multiplier;
        saw_unit = true;
    }
    if !number.is_empty() {
        // Trailing bare number after units, e.g. "1m30" means 1m 30s.
        if !saw_unit {
            return None;
        }
        total += parse_number(&number)?;
    }
    saw_unit.then_some(total)
}

/// Context needed to resolve relative jumps.
#[derive(Debug, Clone, Copy)]
pub struct JumpContext {
    pub current: f64,
    pub duration: f64,
    pub fps: Option<f64>,
}

/// Parses the jump-to-timestamp language and returns the absolute target,
/// clamped to `[0, duration]`.
///
/// On top of [`parse`] it understands:
/// * `+5`, `-1:30`, `+2m` — relative to the current position
/// * `50%` — a percentage of the duration
/// * `f1200`, `1200f` — a frame number (requires a known frame rate)
/// * `end` / `start`
pub fn parse_jump(input: &str, ctx: JumpContext) -> Result<f64, ParseError> {
    let s = input.trim();
    if s.is_empty() {
        return Err(ParseError::Empty);
    }
    let clamp = |t: f64| t.clamp(0.0, ctx.duration.max(0.0));
    let lower = s.to_ascii_lowercase();

    match lower.as_str() {
        "start" | "begin" | "beginning" => return Ok(0.0),
        "end" => return Ok(clamp(ctx.duration)),
        _ => {}
    }

    if let Some(rest) = s.strip_prefix('+') {
        return Ok(clamp(ctx.current + parse_offset(rest, ctx)?));
    }
    if let Some(rest) = s.strip_prefix('-') {
        return Ok(clamp(ctx.current - parse_offset(rest, ctx)?));
    }

    if let Some(pct) = s.strip_suffix('%') {
        let value = parse_number(pct).ok_or_else(|| ParseError::Invalid(s.to_string()))?;
        return Ok(clamp(ctx.duration * value / 100.0));
    }

    if let Some(frame) = parse_frame(&lower) {
        let fps = ctx.fps.filter(|f| *f > 0.0).ok_or(ParseError::NoFrameRate)?;
        return Ok(clamp(frame / fps));
    }

    parse(s).map(clamp)
}

fn parse_offset(rest: &str, ctx: JumpContext) -> Result<f64, ParseError> {
    let lower = rest.trim().to_ascii_lowercase();
    if let Some(frames) = parse_frame(&lower) {
        let fps = ctx.fps.filter(|f| *f > 0.0).ok_or(ParseError::NoFrameRate)?;
        return Ok(frames / fps);
    }
    parse(rest)
}

fn parse_frame(lower: &str) -> Option<f64> {
    let digits = lower.strip_prefix('f').or_else(|| lower.strip_suffix('f'))?.trim();
    if digits.is_empty() || !digits.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    digits.parse::<u64>().ok().map(|f| f as f64)
}

/// Rounds a time to the nearest frame boundary.
pub fn round_to_frame(seconds: f64, fps: f64) -> f64 {
    if fps <= 0.0 {
        return seconds;
    }
    (seconds * fps).round() / fps
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> JumpContext {
        JumpContext { current: 60.0, duration: 3600.0, fps: Some(25.0) }
    }

    #[test]
    fn formats_with_hours() {
        assert_eq!(format(0.0), "00:00:00.000");
        assert_eq!(format(83.5), "00:01:23.500");
        assert_eq!(format(3723.004), "01:02:03.004");
    }

    #[test]
    fn formats_short() {
        assert_eq!(format_short(7.25), "00:07.250");
        assert_eq!(format_short(3723.0), "1:02:03.000");
    }

    #[test]
    fn rounding_carries_into_seconds() {
        assert_eq!(format(59.9996), "00:01:00.000");
        assert_eq!(format_with(0.96, false, 1), "00:01.0");
    }

    #[test]
    fn formats_negative() {
        assert_eq!(format(-1.5), "-00:00:01.500");
        assert_eq!(format(-0.0001), "00:00:00.000");
    }

    #[test]
    fn filename_format_has_no_colons() {
        assert_eq!(format_for_filename(3723.5), "01.02.03.500");
    }

    #[test]
    fn human_durations() {
        assert_eq!(format_duration_human(0.0), "0s");
        assert_eq!(format_duration_human(12.34), "12.3s");
        assert_eq!(format_duration_human(65.0), "1m 05s");
        assert_eq!(format_duration_human(3725.0), "1h 02m 05s");
    }

    #[test]
    fn ruler_decimals_follow_step() {
        assert_eq!(format_ruler(65.0, 5.0), "01:05");
        assert_eq!(format_ruler(65.5, 0.5), "01:05.5");
    }

    #[test]
    fn parses_colon_forms() {
        assert_eq!(parse("1:23:45.5").unwrap(), 5025.5);
        assert_eq!(parse("01:30").unwrap(), 90.0);
        assert_eq!(parse("90").unwrap(), 90.0);
        assert_eq!(parse("12.25").unwrap(), 12.25);
        assert_eq!(parse("0:05,5").unwrap(), 5.5);
        assert_eq!(parse("  2:00 ").unwrap(), 120.0);
    }

    #[test]
    fn rejects_bad_colon_forms() {
        assert!(parse("1:2:3:4").is_err());
        assert!(parse("1:75").is_err());
        assert!(parse("1.5:00").is_err());
        assert!(parse("abc").is_err());
        assert!(parse("1::2").is_err());
        assert_eq!(parse(""), Err(ParseError::Empty));
    }

    #[test]
    fn parses_unit_forms() {
        assert_eq!(parse("1h2m3s").unwrap(), 3723.0);
        assert_eq!(parse("90s").unwrap(), 90.0);
        assert_eq!(parse("2m").unwrap(), 120.0);
        assert_eq!(parse("1h 30m").unwrap(), 5400.0);
        assert_eq!(parse("250ms").unwrap(), 0.25);
        assert_eq!(parse("1m30").unwrap(), 90.0);
        assert!(parse("5x").is_err());
    }

    #[test]
    fn jump_absolute_and_clamped() {
        assert_eq!(parse_jump("1:00:00", ctx()).unwrap(), 3600.0);
        assert_eq!(parse_jump("2:00:00", ctx()).unwrap(), 3600.0);
        assert_eq!(parse_jump("end", ctx()).unwrap(), 3600.0);
        assert_eq!(parse_jump("start", ctx()).unwrap(), 0.0);
    }

    #[test]
    fn jump_relative() {
        assert_eq!(parse_jump("+5", ctx()).unwrap(), 65.0);
        assert_eq!(parse_jump("-1:30", ctx()).unwrap(), 0.0);
        assert_eq!(parse_jump("-30s", ctx()).unwrap(), 30.0);
        assert_eq!(parse_jump("+25f", ctx()).unwrap(), 61.0);
    }

    #[test]
    fn jump_percent_and_frames() {
        assert_eq!(parse_jump("50%", ctx()).unwrap(), 1800.0);
        assert_eq!(parse_jump("f250", ctx()).unwrap(), 10.0);
        assert_eq!(parse_jump("250f", ctx()).unwrap(), 10.0);
        let no_fps = JumpContext { fps: None, ..ctx() };
        assert_eq!(parse_jump("f10", no_fps), Err(ParseError::NoFrameRate));
    }

    #[test]
    fn snapping() {
        assert_eq!(round_to_frame(1.0, 25.0), 1.0);
        assert!((round_to_frame(1.019, 25.0) - 1.0).abs() < 1e-9);
        assert!((round_to_frame(1.021, 25.0) - 1.04).abs() < 1e-9);
        assert!((round_to_frame(18.323, 30.0) - 18.333_333).abs() < 1e-5);
        assert_eq!(round_to_frame(1.5, 0.0), 1.5);
    }
}
