//! Saving and loading work: project files, cut lists and app settings.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, bail};
use serde::{Deserialize, Serialize};

use crate::export::ExportOptions;
use crate::playlist::Playlist;
use crate::timecode;

pub const PROJECT_EXTENSION: &str = "gpuicut";
const PROJECT_VERSION: u32 = 1;

/// A saved playlist plus the export settings it was built with.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProjectFile {
    pub version: u32,
    pub playlist: Playlist,
    #[serde(default)]
    pub export: Option<ExportOptions>,
}

pub fn save_project(path: &Path, playlist: &Playlist, export: &ExportOptions) -> anyhow::Result<()> {
    let file = ProjectFile { version: PROJECT_VERSION, playlist: playlist.clone(), export: Some(export.clone()) };
    let json = serde_json::to_string_pretty(&file)?;
    write_atomically(path, json.as_bytes())
}

pub fn load_project(path: &Path) -> anyhow::Result<ProjectFile> {
    let bytes = std::fs::read(path).with_context(|| format!("could not read {}", path.display()))?;
    let file: ProjectFile = serde_json::from_slice(&bytes).context("this is not a valid project file")?;
    if file.version > PROJECT_VERSION {
        bail!("this project was saved by a newer version of the app");
    }
    Ok(file)
}

fn write_atomically(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    let tmp = path.with_extension("tmp-write");
    std::fs::write(&tmp, bytes).with_context(|| format!("could not write {}", path.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("could not write {}", path.display()))?;
    Ok(())
}

// --- CSV cut lists -------------------------------------------------------

/// A cut range read from a CSV file.
#[derive(Debug, Clone, PartialEq)]
pub struct CsvCut {
    pub start: f64,
    pub end: f64,
    pub label: String,
}

fn csv_escape(field: &str) -> String {
    if field.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", field.replace('"', "\"\""))
    } else {
        field.to_string()
    }
}

/// Writes `start,end,label` rows with times as `HH:MM:SS.mmm`.
pub fn cuts_to_csv(cuts: &[(f64, f64, String)]) -> String {
    let mut out = String::from("Start,End,Label\n");
    for (start, end, label) in cuts {
        out.push_str(&format!("{},{},{}\n", timecode::format(*start), timecode::format(*end), csv_escape(label)));
    }
    out
}

/// Splits one CSV record, honoring quotes. Returns `None` for a record that
/// continues on the next line (unterminated quote).
fn split_record(line: &str, delimiter: char) -> Option<Vec<String>> {
    let mut fields = Vec::new();
    let mut field = String::new();
    let mut quoted = false;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' if quoted && chars.peek() == Some(&'"') => {
                field.push('"');
                chars.next();
            }
            '"' if field.is_empty() && !quoted => quoted = true,
            '"' if quoted => quoted = false,
            c if c == delimiter && !quoted => fields.push(std::mem::take(&mut field)),
            c => field.push(c),
        }
    }
    if quoted {
        return None;
    }
    fields.push(field);
    Some(fields)
}

/// Parses a cut list. Accepts `,`, `;` or tab delimited files with an
/// optional header; times may be seconds or timecodes; the end may be empty
/// (meaning "to the end").
pub fn cuts_from_csv(text: &str, duration: f64) -> anyhow::Result<Vec<CsvCut>> {
    let first_line = text.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
    // Tabs and semicolons are only used as delimiters, while commas may also
    // be decimal separators, so prefer them when present.
    let delimiter = ['\t', ';'].into_iter().find(|d| first_line.contains(*d)).unwrap_or(',');
    let mut cuts = Vec::new();
    let mut pending = String::new();
    for (line_no, raw) in text.lines().enumerate() {
        if !pending.is_empty() {
            pending.push('\n');
        }
        pending.push_str(raw);
        let Some(fields) = split_record(&pending, delimiter) else {
            continue;
        };
        let record = std::mem::take(&mut pending);
        if record.trim().is_empty() {
            continue;
        }
        let start_field = fields.first().map(|s| s.trim()).unwrap_or("");
        if cuts.is_empty() && start_field.eq_ignore_ascii_case("start") {
            continue; // header
        }
        let start = if start_field.is_empty() {
            0.0
        } else {
            timecode::parse(start_field).with_context(|| format!("line {}: invalid start time", line_no + 1))?
        };
        let end_field = fields.get(1).map(|s| s.trim()).unwrap_or("");
        let end = if end_field.is_empty() {
            duration
        } else {
            timecode::parse(end_field).with_context(|| format!("line {}: invalid end time", line_no + 1))?
        };
        let label = fields.get(2).map(|s| s.trim().to_string()).unwrap_or_default();
        if end <= start {
            bail!("line {}: the end must come after the start", line_no + 1);
        }
        cuts.push(CsvCut { start, end, label });
    }
    if cuts.is_empty() {
        bail!("no cuts were found in this file");
    }
    Ok(cuts)
}

// --- files opened by the OS ----------------------------------------------

static OPEN_REQUESTS: std::sync::Mutex<Vec<PathBuf>> = std::sync::Mutex::new(Vec::new());

/// Queues files the OS asked the app to open (e.g. from Finder).
pub fn request_open(urls: Vec<String>) {
    let paths = urls.iter().filter_map(|u| file_url_to_path(u));
    if let Ok(mut queue) = OPEN_REQUESTS.lock() {
        queue.extend(paths);
    }
}

/// Takes the queued open requests.
pub fn take_open_requests() -> Vec<PathBuf> {
    OPEN_REQUESTS.lock().map(|mut q| std::mem::take(&mut *q)).unwrap_or_default()
}

/// Converts a `file://` URL (percent-encoded) to a path.
pub fn file_url_to_path(url: &str) -> Option<PathBuf> {
    let rest = url.strip_prefix("file://")?;
    // Skip an optional host ("file://localhost/…").
    let path = &rest[rest.find('/')?..];
    let bytes = path.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Ok(b) = u8::from_str_radix(&path[i + 1..i + 3], 16)
        {
            out.push(b);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8(out).ok().map(PathBuf::from)
}

// --- settings ------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub export: ExportOptions,
    pub volume: f32,
    pub dark_mode: bool,
    pub recent_files: Vec<PathBuf>,
    pub show_waveform: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            export: ExportOptions::default(),
            volume: 0.8,
            dark_mode: true,
            recent_files: Vec::new(),
            show_waveform: true,
        }
    }
}

impl Settings {
    fn path() -> Option<PathBuf> {
        // GPUI_CUT_CONFIG_DIR redirects settings (used by tests).
        std::env::var_os("GPUI_CUT_CONFIG_DIR")
            .map(PathBuf::from)
            .or_else(|| dirs::config_dir().map(|d| d.join("gpui-cut")))
            .map(|d| d.join("settings.json"))
    }

    pub fn load() -> Self {
        Self::path()
            .and_then(|p| std::fs::read(p).ok())
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) {
        let Some(path) = Self::path() else { return };
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(json) = serde_json::to_string_pretty(self)
            && let Err(e) = write_atomically(&path, json.as_bytes())
        {
            eprintln!("could not save settings: {e:#}");
        }
    }

    /// Moves `path` to the front of the recent files list.
    pub fn add_recent(&mut self, path: &Path) {
        self.recent_files.retain(|p| p != path);
        self.recent_files.insert(0, path.to_path_buf());
        self.recent_files.truncate(8);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn csv_roundtrip() {
        let csv = cuts_to_csv(&[(1.5, 3.0, "a, \"quoted\" one".into()), (65.0, 70.25, String::new())]);
        assert_eq!(
            csv,
            "Start,End,Label\n00:00:01.500,00:00:03.000,\"a, \"\"quoted\"\" one\"\n00:01:05.000,00:01:10.250,\n"
        );
        let cuts = cuts_from_csv(&csv, 100.0).unwrap();
        assert_eq!(
            cuts,
            vec![
                CsvCut { start: 1.5, end: 3.0, label: "a, \"quoted\" one".into() },
                CsvCut { start: 65.0, end: 70.25, label: String::new() },
            ]
        );
    }

    #[test]
    fn csv_variants() {
        let cuts = cuts_from_csv("1.5;3\n;4;x\n10;;end\n", 20.0).unwrap();
        assert_eq!(cuts[0], CsvCut { start: 1.5, end: 3.0, label: String::new() });
        assert_eq!(cuts[1], CsvCut { start: 0.0, end: 4.0, label: "x".into() });
        assert_eq!(cuts[2], CsvCut { start: 10.0, end: 20.0, label: "end".into() });
        let tabs = cuts_from_csv("start\tend\n0:01\t0:02\n", 5.0).unwrap();
        assert_eq!(tabs[0].end, 2.0);
    }

    #[test]
    fn csv_comma_decimals_with_semicolons() {
        let cuts = cuts_from_csv("1,5;3,5;Intro, part 1\n", 10.0).unwrap();
        assert_eq!(cuts[0], CsvCut { start: 1.5, end: 3.5, label: "Intro, part 1".into() });
    }

    #[test]
    fn csv_multiline_labels() {
        let cuts = cuts_from_csv("1,2,\"two\nlines\"\n3,4,x\n", 10.0).unwrap();
        assert_eq!(cuts.len(), 2);
        assert_eq!(cuts[0].label, "two\nlines");
    }

    #[test]
    fn csv_errors() {
        assert!(cuts_from_csv("", 1.0).is_err());
        assert!(cuts_from_csv("5,2\n", 10.0).is_err());
        let err = cuts_from_csv("1,2\nx,3\n", 10.0).unwrap_err();
        assert!(err.to_string().contains("line 2"), "{err}");
    }

    #[test]
    fn project_roundtrip() {
        let dir = std::env::temp_dir().join(format!("gpui-cut-project-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("p.gpuicut");
        let mut playlist = Playlist::new();
        playlist.add("/a.mp4", 1.0, 2.0, "x");
        let export = ExportOptions::default();
        save_project(&path, &playlist, &export).unwrap();
        let loaded = load_project(&path).unwrap();
        assert_eq!(loaded.playlist, playlist);
        assert_eq!(loaded.export, Some(export));
        std::fs::write(&path, "{\"version\": 99, \"playlist\": {\"cuts\": [], \"next_id\": 0}}").unwrap();
        assert!(load_project(&path).is_err());
    }

    #[test]
    fn file_urls() {
        assert_eq!(
            file_url_to_path("file:///Users/me/My%20Movie%20%C3%A9.mp4"),
            Some(PathBuf::from("/Users/me/My Movie é.mp4"))
        );
        assert_eq!(file_url_to_path("file://localhost/tmp/a.mp4"), Some(PathBuf::from("/tmp/a.mp4")));
        assert_eq!(file_url_to_path("https://example.com/a.mp4"), None);
        request_open(vec!["file:///tmp/x.mov".into(), "nope".into()]);
        assert_eq!(take_open_requests(), vec![PathBuf::from("/tmp/x.mov")]);
        assert!(take_open_requests().is_empty());
    }

    #[test]
    fn recent_files() {
        let mut s = Settings::default();
        for i in 0..10 {
            s.add_recent(Path::new(&format!("/f{i}")));
        }
        s.add_recent(Path::new("/f3"));
        assert_eq!(s.recent_files.len(), 8);
        assert_eq!(s.recent_files[0], PathBuf::from("/f3"));
        assert_eq!(s.recent_files.iter().filter(|p| *p == Path::new("/f3")).count(), 1);
    }

    #[test]
    fn settings_tolerate_missing_fields() {
        let s: Settings = serde_json::from_str("{\"volume\": 0.5}").unwrap();
        assert_eq!(s.volume, 0.5);
        assert!(s.show_waveform);
    }
}
