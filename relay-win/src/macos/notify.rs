//! macOS notifications and Finder/browser integration.
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::AtomicBool;
use std::sync::Mutex;

pub static QUIT_REQUESTED: AtomicBool = AtomicBool::new(false);
static OPEN_REQUEST: Mutex<Option<String>> = Mutex::new(None);

fn run(mut command: Command) {
    // Reap commands off the UI thread.
    if let Ok(mut child) = command.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn() {
        std::thread::spawn(move || {
            let _ = child.wait();
        });
    }
}

pub fn toast(title: &str, body: &str, tag: &str) {
    toast_for(title, body, tag, "");
}

pub fn toast_for(title: &str, body: &str, _tag: &str, _thread: &str) {
    let mut command = Command::new("/usr/bin/osascript");
    command.args([
        "-e",
        "on run argv",
        "-e",
        "display notification (item 2 of argv) with title (item 1 of argv)",
        "-e",
        "end run",
        "--",
        title,
        body,
    ]);
    run(command);
}

fn web_url(target: &str) -> Option<&str> {
    let url = target.trim();
    ((url.starts_with("https://") || url.starts_with("http://")) && !url.chars().any(char::is_control)).then_some(url)
}

fn open_web(target: &str) -> bool {
    let Some(url) = web_url(target) else { return false };
    let Ok(mut child) = Command::new("/usr/bin/open").args(["--", url]).spawn() else {
        return false;
    };
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    true
}

pub fn open_url(target: &str) {
    open_web(target);
}

pub fn reveal(path: &str) {
    let mut command = Command::new("/usr/bin/open");
    command.args(["-R", "--", path]);
    run(command);
}

/// Use Facebook's own WebRTC client in the default browser.
pub fn call(thread_url: &str, _video: bool, _cookies: &str) -> bool {
    open_web(thread_url)
}

pub fn play(target: &str) {
    if let Some(url) = web_url(target) {
        if let Some(exe) = crate::paths::sibling_executable("russenger-player") {
            let mut command = Command::new(exe);
            command.arg(url);
            run(command);
        }
        return;
    }
    let path = target.strip_prefix("file://").unwrap_or(target);
    let (Ok(path), Ok(data)) = (std::fs::canonicalize(path), std::fs::canonicalize(crate::vault::data_dir())) else {
        return;
    };
    if !(path.starts_with(data.join("media")) || path.starts_with(data.join("media-cache"))) {
        return;
    }
    let extension = path.extension().and_then(|ext| ext.to_str());
    if matches!(extension, Some("mp4" | "m4a" | "mov" | "aac" | "mp3" | "ogg" | "webm" | "wav")) {
        let mut command = Command::new("/usr/bin/open");
        command.arg("--").arg(path);
        run(command);
    }
}

fn launch_agent() -> Option<PathBuf> {
    Some(std::env::home_dir()?.join("Library/LaunchAgents/io.github.ducvd89.Russenger.plist"))
}

pub fn autostart_enabled() -> bool {
    launch_agent().is_some_and(|p| p.is_file())
}

pub fn set_autostart(on: bool) {
    let Some(path) = launch_agent() else { return };
    if !on {
        let _ = std::fs::remove_file(path);
        return;
    }
    let Ok(exe) = std::env::current_exe() else { return };
    let Some(bundle) = exe.ancestors().find(|path| path.extension().is_some_and(|ext| ext == "app")) else {
        return;
    };
    let bundle = bundle.to_string_lossy().replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
    let plist = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>Label</key><string>io.github.ducvd89.Russenger</string>
<key>ProgramArguments</key><array><string>/usr/bin/open</string><string>{bundle}</string></array>
<key>RunAtLoad</key><true/>
</dict></plist>"#
    );
    if std::fs::create_dir_all(path.parent().unwrap()).is_ok() {
        let _ = std::fs::write(path, plist);
    }
}

pub fn open_request_file() -> PathBuf {
    crate::vault::data_dir().join("open-thread.txt")
}

pub fn watch_open_requests(ctx: eframe::egui::Context) {
    std::thread::spawn(move || {
        let request = open_request_file();
        loop {
            std::thread::sleep(std::time::Duration::from_millis(250));
            if let Ok(thread) = std::fs::read_to_string(&request) {
                let _ = std::fs::remove_file(&request);
                *OPEN_REQUEST.lock().unwrap() = Some(thread);
                ctx.request_repaint();
            }
        }
    });
}

pub fn take_open_request() -> Option<String> {
    OPEN_REQUEST.lock().unwrap().take()
}
