use std::path::PathBuf;

pub fn data_dir() -> PathBuf {
    #[cfg(windows)]
    let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from);
    #[cfg(target_os = "macos")]
    let base = std::env::home_dir().map(|p| p.join("Library/Application Support"));
    let app = if cfg!(windows) { "Relay" } else { "Russenger" };
    base.unwrap_or_else(std::env::temp_dir).join(app)
}

#[allow(dead_code)] // The WebView executables only use data_dir.
pub fn executable_name(stem: &str) -> String {
    format!("{stem}{}", std::env::consts::EXE_SUFFIX)
}

#[allow(dead_code)]
pub fn sibling_executable(stem: &str) -> Option<PathBuf> {
    let current = std::env::current_exe().ok()?;
    Some(current.parent()?.join(executable_name(stem)))
}
