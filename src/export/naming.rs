//! Output file naming.
//!
//! Templates use `{variable}` placeholders:
//!
//! | variable     | value                                             |
//! |--------------|---------------------------------------------------|
//! | `{name}`     | source file name without extension                |
//! | `{from}`     | cut start, `HH.MM.SS.mmm`                         |
//! | `{to}`       | cut end, `HH.MM.SS.mmm`                           |
//! | `{duration}` | cut length, `HH.MM.SS.mmm`                        |
//! | `{n}`        | 1-based position among exported cuts, zero padded |
//! | `{label}`    | the cut label (may be empty)                      |
//! | `{suffix}`   | `-label`, or `-cutN` when there are several cuts  |
//! | `{date}`     | export date, `YYYY-MM-DD`                         |
//!
//! The extension is appended automatically.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::timecode;

pub const VARIABLES: &[&str] = &["name", "from", "to", "duration", "n", "label", "suffix", "date"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TemplateError {
    Empty,
    Unclosed,
    UnknownVariable(String),
}

impl std::fmt::Display for TemplateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TemplateError::Empty => write!(f, "The file name template is empty"),
            TemplateError::Unclosed => write!(f, "A \u{201c}{{\u{201d} is never closed"),
            TemplateError::UnknownVariable(v) => write!(f, "Unknown variable {{{v}}}"),
        }
    }
}

impl std::error::Error for TemplateError {}

/// Values substituted into a template.
#[derive(Debug, Clone, Default)]
pub struct NameVars {
    pub name: String,
    pub from: f64,
    pub to: f64,
    pub index: usize,
    pub count: usize,
    pub label: String,
    pub date: String,
}

impl NameVars {
    fn get(&self, key: &str) -> Option<String> {
        let width = self.count.max(1).to_string().len();
        Some(match key {
            "name" => self.name.clone(),
            "from" => timecode::format_for_filename(self.from),
            "to" => timecode::format_for_filename(self.to),
            "duration" => timecode::format_for_filename((self.to - self.from).max(0.0)),
            "n" => format!("{:0width$}", self.index + 1),
            "label" => sanitize(&self.label),
            "suffix" => {
                let label = sanitize(&self.label);
                if !label.is_empty() {
                    format!("-{label}")
                } else if self.count > 1 {
                    format!("-cut{:0width$}", self.index + 1)
                } else {
                    String::new()
                }
            }
            "date" => self.date.clone(),
            _ => return None,
        })
    }
}

/// Checks a template for syntax errors without rendering it.
pub fn validate(template: &str) -> Result<(), TemplateError> {
    render(template, &NameVars { name: "x".into(), ..Default::default() }).map(|_| ())
}

/// Renders `template`, returning a sanitized file stem.
pub fn render(template: &str, vars: &NameVars) -> Result<String, TemplateError> {
    if template.trim().is_empty() {
        return Err(TemplateError::Empty);
    }
    let mut out = String::new();
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        let close = after.find('}').ok_or(TemplateError::Unclosed)?;
        let key = after[..close].trim();
        let value =
            vars.get(&key.to_ascii_lowercase()).ok_or_else(|| TemplateError::UnknownVariable(key.to_string()))?;
        out.push_str(&value);
        rest = &after[close + 1..];
    }
    out.push_str(rest);
    let stem = sanitize(&out);
    if stem.is_empty() {
        return Err(TemplateError::Empty);
    }
    Ok(stem)
}

/// Makes a string safe to use as a file name on all major platforms.
pub fn sanitize(input: &str) -> String {
    let mut out: String = input
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();
    // Windows forbids trailing dots and spaces; leading dots hide files.
    out = out.trim().trim_end_matches('.').trim_start_matches('.').trim().to_string();
    if out.chars().count() > 200 {
        out = out.chars().take(200).collect();
    }
    out
}

/// Source file name without its extension.
pub fn file_stem(path: &Path) -> String {
    path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "output".into())
}

/// Picks a path in `dir` that neither exists on disk nor is already `taken`.
/// Appends ` (2)`, ` (3)`… to the stem as needed.
pub fn unique_path(
    dir: &Path,
    stem: &str,
    ext: &str,
    taken: &HashSet<PathBuf>,
    exists: impl Fn(&Path) -> bool,
) -> PathBuf {
    let make = |n: usize| {
        let name = if n == 1 { format!("{stem}.{ext}") } else { format!("{stem} ({n}).{ext}") };
        dir.join(name)
    };
    (1..).map(make).find(|p| !taken.contains(p) && !exists(p)).expect("an unused name always exists")
}

/// Today's date as `YYYY-MM-DD` (UTC), without pulling in a date crate.
pub fn today() -> String {
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    civil_date((secs / 86_400) as i64)
}

/// Converts days since 1970-01-01 to `YYYY-MM-DD` (proleptic Gregorian).
pub fn civil_date(days: i64) -> String {
    // Howard Hinnant's days_from_civil inverse.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars() -> NameVars {
        NameVars {
            name: "holiday".into(),
            from: 65.25,
            to: 70.0,
            index: 2,
            count: 12,
            label: "Best bit".into(),
            date: "2026-01-02".into(),
        }
    }

    #[test]
    fn renders_default_template() {
        let out = render(super::super::DEFAULT_SEPARATE_TEMPLATE, &vars()).unwrap();
        assert_eq!(out, "holiday-00.01.05.250-00.01.10.000-Best bit");
    }

    #[test]
    fn suffix_falls_back_to_number() {
        let mut v = vars();
        v.label.clear();
        assert_eq!(render("{name}{suffix}", &v).unwrap(), "holiday-cut03");
        v.count = 1;
        assert_eq!(render("{name}{suffix}", &v).unwrap(), "holiday");
    }

    #[test]
    fn renders_all_variables() {
        let out = render("{n}_{label}_{duration}_{date}", &vars()).unwrap();
        assert_eq!(out, "03_Best bit_00.00.04.750_2026-01-02");
        assert_eq!(render("{NAME}", &vars()).unwrap(), "holiday");
    }

    #[test]
    fn template_errors() {
        assert_eq!(render("{nope}", &vars()), Err(TemplateError::UnknownVariable("nope".into())));
        assert_eq!(render("{name", &vars()), Err(TemplateError::Unclosed));
        assert_eq!(render("  ", &vars()), Err(TemplateError::Empty));
        assert_eq!(render("{label}", &NameVars::default()), Err(TemplateError::Empty));
        assert!(validate("{name}-{n}").is_ok());
    }

    #[test]
    fn sanitizes_names() {
        assert_eq!(sanitize("a/b\\c:d*e?f\"g<h>i|j"), "a_b_c_d_e_f_g_h_i_j");
        assert_eq!(sanitize("  .hidden. "), "hidden");
        assert_eq!(sanitize("line\nbreak"), "line_break");
        assert_eq!(sanitize(&"x".repeat(300)).len(), 200);
    }

    #[test]
    fn unique_paths_avoid_collisions() {
        let dir = Path::new("/out");
        let mut taken = HashSet::new();
        let exists = |p: &Path| p == Path::new("/out/a.mp4");
        let first = unique_path(dir, "a", "mp4", &taken, exists);
        assert_eq!(first, PathBuf::from("/out/a (2).mp4"));
        taken.insert(first);
        let second = unique_path(dir, "a", "mp4", &taken, exists);
        assert_eq!(second, PathBuf::from("/out/a (3).mp4"));
        assert_eq!(unique_path(dir, "b", "mkv", &taken, exists), PathBuf::from("/out/b.mkv"));
    }

    #[test]
    fn civil_dates() {
        assert_eq!(civil_date(0), "1970-01-01");
        assert_eq!(civil_date(19_723), "2024-01-01");
        assert_eq!(civil_date(19_782), "2024-02-29");
        assert_eq!(today().len(), 10);
    }
}
