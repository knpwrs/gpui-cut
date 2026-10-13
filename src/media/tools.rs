//! Locating the `ffmpeg` and `ffprobe` executables.
//!
//! Apps launched from the Finder/Dock do not inherit the shell `PATH`, so on
//! top of `PATH` we also look in the usual package manager locations.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

const EXTRA_DIRS: &[&str] = &["/opt/homebrew/bin", "/usr/local/bin", "/opt/local/bin", "/usr/bin", "/snap/bin"];

#[derive(Debug, Clone)]
pub struct Tools {
    pub ffmpeg: PathBuf,
    pub ffprobe: PathBuf,
}

fn exe_name(name: &str) -> String {
    if cfg!(windows) { format!("{name}.exe") } else { name.to_string() }
}

fn is_executable(path: &Path) -> bool {
    path.is_file()
}

/// Searches for `name`, honoring an explicit `GPUI_CUT_<NAME>` override.
pub fn find(name: &str) -> Option<PathBuf> {
    let env_key = format!("GPUI_CUT_{}", name.to_ascii_uppercase());
    if let Some(explicit) = std::env::var_os(&env_key) {
        let path = PathBuf::from(explicit);
        if is_executable(&path) {
            return Some(path);
        }
    }
    let file = exe_name(name);
    let path_dirs = std::env::var_os("PATH").map(|p| std::env::split_paths(&p).collect::<Vec<_>>()).unwrap_or_default();
    path_dirs
        .into_iter()
        .chain(EXTRA_DIRS.iter().map(PathBuf::from))
        .map(|dir| dir.join(&file))
        .find(|candidate| is_executable(candidate))
}

static TOOLS: OnceLock<Result<Tools, String>> = OnceLock::new();

/// Returns the located tools, or a user-presentable error.
pub fn tools() -> Result<&'static Tools, String> {
    TOOLS
        .get_or_init(|| {
            let ffmpeg = find("ffmpeg").ok_or_else(|| {
                "ffmpeg was not found. Install it (e.g. `brew install ffmpeg`) or set GPUI_CUT_FFMPEG.".to_string()
            })?;
            let ffprobe = find("ffprobe").ok_or_else(|| {
                "ffprobe was not found. Install ffmpeg (e.g. `brew install ffmpeg`) or set GPUI_CUT_FFPROBE."
                    .to_string()
            })?;
            Ok(Tools { ffmpeg, ffprobe })
        })
        .as_ref()
        .map_err(Clone::clone)
}

pub fn ffmpeg_command() -> anyhow::Result<Command> {
    let tools = tools().map_err(anyhow::Error::msg)?;
    let mut cmd = Command::new(&tools.ffmpeg);
    hide_console(&mut cmd);
    Ok(cmd)
}

pub fn ffprobe_command() -> anyhow::Result<Command> {
    let tools = tools().map_err(anyhow::Error::msg)?;
    let mut cmd = Command::new(&tools.ffprobe);
    hide_console(&mut cmd);
    Ok(cmd)
}

#[cfg(windows)]
fn hide_console(cmd: &mut Command) {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    cmd.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(not(windows))]
fn hide_console(_cmd: &mut Command) {}
