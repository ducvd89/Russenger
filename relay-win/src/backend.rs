//! Talks to relay-helper (Go, messagix + E2EE) over JSON-lines on stdio.
//! The helper is one-shot per session: it exits after a failed login, so the
//! owner must respawn it (see `is_alive`).
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{channel, Receiver};

pub struct Backend {
    child: Child,
    stdin: ChildStdin,
    pub events: Receiver<Value>,
}

fn helper_path() -> PathBuf {
    let exe_dir = std::env::current_exe().ok().and_then(|p| p.parent().map(|p| p.to_path_buf()));
    [
        exe_dir.map(|d| d.join("relay-helper.exe")),
        Some(PathBuf::from("../bin/relay-helper.exe")),
        Some(PathBuf::from("bin/relay-helper.exe")),
    ]
    .into_iter()
    .flatten()
    .find(|p| p.exists())
    .unwrap_or_else(|| PathBuf::from("relay-helper.exe"))
}

impl Backend {
    /// `on_event` runs on the reader thread after each event is queued
    /// (e.g. to request a UI repaint).
    pub fn spawn(on_event: impl Fn() + Send + 'static) -> std::io::Result<Self> {
        let mut cmd = Command::new(helper_path());
        // stderr goes to a log file: an undrained pipe would eventually block the helper.
        let log = std::fs::File::create(crate::vault::data_dir().join("relay-helper.log"))?;
        // messenger.com was retired in 2026; talk to facebook.com unless overridden.
        if std::env::var_os("RELAY_PLATFORM").is_none() {
            cmd.env("RELAY_PLATFORM", "facebook");
        }
        cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::from(log));
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        }
        let mut child = cmd.spawn()?;
        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if let Ok(v) = serde_json::from_str::<Value>(&line) {
                    if tx.send(v).is_err() {
                        return;
                    }
                    on_event();
                }
            }
            let _ = tx.send(json!({"type": "exited"}));
            on_event();
        });
        Ok(Self { child, stdin, events: rx })
    }

    pub fn is_alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    pub fn send(&mut self, cmd: Value) {
        let _ = writeln!(self.stdin, "{}", cmd);
        let _ = self.stdin.flush();
    }

    /// Cookies are sent over stdin only, never via argv.
    pub fn login(&mut self, cookies: &str) {
        self.send(json!({"cmd": "login", "cookies": cookies}));
    }
    pub fn send_text(&mut self, thread: &str, text: &str, tag: &str) {
        self.send(json!({"cmd": "send", "thread": thread, "text": text, "clientTag": tag}));
    }
    pub fn refresh_thread(&mut self, thread: &str) {
        self.send(json!({"cmd": "refreshThread", "thread": thread}));
    }
}

impl Drop for Backend {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
