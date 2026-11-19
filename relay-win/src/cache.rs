//! Picture / video cache on disk with a size limit.
//!
//! Anything shown from Facebook's CDN is also stored under %LOCALAPPDATA%\Relay\media-cache, and
//! media the helper decrypted from end-to-end encrypted chats lives in %APPDATA%\Relay\media. The
//! two folders together are kept below the limit chosen in Settings: when they grow past it the
//! least recently used files are deleted. A deleted picture is simply fetched again when needed
//! (end-to-end encrypted ones cannot be, they show a note instead). Emoji pictures are tiny and
//! kept in their own folder that is never trimmed.
use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime};

const IMAGE_MAX: u64 = 64 * 1024 * 1024;
const VIDEO_MAX: u64 = 600 * 1024 * 1024;
const TOUCH_EVERY: Duration = Duration::from_secs(600);

static CTX: OnceLock<eframe::egui::Context> = OnceLock::new();
static LIMIT: AtomicU64 = AtomicU64::new(1024 * 1024 * 1024);
static INDEX: Mutex<Option<HashMap<String, (PathBuf, Instant)>>> = Mutex::new(None);
static PENDING: Mutex<Option<HashSet<String>>> = Mutex::new(None);
static TRIM_BUSY: Mutex<bool> = Mutex::new(false);

pub fn cache_dir() -> PathBuf {
    let d = crate::vault::data_dir().join("media-cache");
    let _ = std::fs::create_dir_all(&d);
    d
}

fn emoji_dir() -> PathBuf {
    let d = crate::vault::data_dir().join("emoji-cache");
    let _ = std::fs::create_dir_all(&d);
    d
}

/// Where the helper writes decrypted end-to-end encrypted media.
pub fn e2ee_dir() -> PathBuf {
    std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_else(std::env::temp_dir).join("Relay").join("media")
}

pub fn init(ctx: &eframe::egui::Context, limit_mb: u64) {
    let _ = CTX.set(ctx.clone());
    set_limit_mb(limit_mb);
    std::thread::spawn(|| {
        build_index();
        trim();
    });
}

pub fn set_limit_mb(mb: u64) {
    LIMIT.store(mb.max(64) * 1024 * 1024, Ordering::Relaxed);
}

fn build_index() {
    let mut map = HashMap::new();
    for dir in [cache_dir(), emoji_dir()] {
        if let Ok(rd) = std::fs::read_dir(dir) {
            for e in rd.flatten() {
                let p = e.path();
                if p.extension().map(|x| x == "part").unwrap_or(false) {
                    let _ = std::fs::remove_file(&p);
                    continue;
                }
                if let Some(stem) = p.file_stem().and_then(|s| s.to_str()) {
                    map.insert(stem.to_string(), (p.clone(), Instant::now()));
                }
            }
        }
    }
    *INDEX.lock().unwrap() = Some(map);
}

/// Stable name for a URL: Facebook's links carry a changing signature after '?', which is dropped.
fn key(url: &str) -> String {
    let base = url.split(['?', '#']).next().unwrap_or(url);
    let mut h: u64 = 0xcbf29ce484222325;
    for b in base.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{h:016x}")
}

fn ext_of(data: &[u8], url: &str, default: &str) -> String {
    let sniff = if data.starts_with(&[0xFF, 0xD8]) {
        Some("jpg")
    } else if data.starts_with(&[0x89, b'P', b'N', b'G']) {
        Some("png")
    } else if data.starts_with(b"GIF8") {
        Some("gif")
    } else if data.len() > 12 && &data[..4] == b"RIFF" && &data[8..12] == b"WEBP" {
        Some("webp")
    } else if data.len() > 8 && &data[4..8] == b"ftyp" {
        Some("mp4")
    } else if data.starts_with(&[0x1A, 0x45, 0xDF, 0xA3]) {
        Some("webm")
    } else {
        None
    };
    if let Some(s) = sniff {
        return s.into();
    }
    let path = url.split(['?', '#']).next().unwrap_or("");
    match path.rsplit('.').next().map(|e| e.to_ascii_lowercase()) {
        Some(e) if matches!(e.as_str(), "jpg" | "jpeg" | "png" | "gif" | "webp" | "mp4" | "webm" | "m4a" | "mp3") => e,
        _ => default.into(),
    }
}

fn lookup(k: &str) -> Option<PathBuf> {
    let mut g = INDEX.lock().ok()?;
    let map = g.as_mut()?;
    let (p, touched) = map.get_mut(k)?;
    if touched.elapsed() > TOUCH_EVERY {
        // recently used files survive the next trim
        if let Ok(f) = std::fs::OpenOptions::new().write(true).open(&*p) {
            let _ = f.set_modified(SystemTime::now());
        }
        *touched = Instant::now();
    }
    Some(p.clone())
}

fn fetch(url: &str, max: u64) -> Result<Vec<u8>, String> {
    let resp = ureq::get(url).timeout(Duration::from_secs(120)).call().map_err(|e| e.to_string())?;
    let mut data = Vec::new();
    resp.into_reader().take(max + 1).read_to_end(&mut data).map_err(|e| e.to_string())?;
    if data.is_empty() || data.len() as u64 > max {
        return Err("empty or too large".into());
    }
    Ok(data)
}

struct Job {
    url: String,
    k: String,
    dir: PathBuf,
    max: u64,
    default_ext: &'static str,
    trim_after: bool,
}

static QUEUE: Mutex<Option<std::sync::mpsc::Sender<Job>>> = Mutex::new(None);

/// Downloads run on a few worker threads (a chat full of pictures must not start hundreds).
fn sender() -> std::sync::mpsc::Sender<Job> {
    let mut g = QUEUE.lock().unwrap();
    if let Some(tx) = g.as_ref() {
        return tx.clone();
    }
    let (tx, rx) = std::sync::mpsc::channel::<Job>();
    let rx = std::sync::Arc::new(Mutex::new(rx));
    for _ in 0..3 {
        let rx = rx.clone();
        std::thread::spawn(move || loop {
            let job = match rx.lock().unwrap().recv() {
                Ok(j) => j,
                Err(_) => return,
            };
            run(job);
        });
    }
    *g = Some(tx.clone());
    tx
}

fn schedule(url: &str, k: String, dir: PathBuf, max: u64, default_ext: &'static str, trim_after: bool) {
    {
        let mut g = PENDING.lock().unwrap();
        let set = g.get_or_insert_with(HashSet::new);
        if set.len() > 200 || set.contains(&k) {
            return; // already on its way, or far too much is queued: it is asked for again later
        }
        // a link that just failed is not tried again at once (it would be retried on every frame)
        if let Some(at) = FAILED.lock().unwrap().as_ref().and_then(|m| m.get(url)) {
            if at.elapsed() < Duration::from_secs(60) {
                return;
            }
        }
        set.insert(k.clone());
    }
    let _ = sender().send(Job { url: url.to_string(), k, dir, max, default_ext, trim_after });
}

fn run(job: Job) {
    let Job { url, k, dir, max, default_ext, trim_after } = job;
    {
        let fetched = fetch(&url, max);
        if fetched.is_err() {
            let mut failures = FAILED.lock().unwrap();
            let failures = failures.get_or_insert_with(HashMap::new);
            if failures.len() >= 2048 {
                failures.retain(|_, at| at.elapsed() < Duration::from_secs(60));
            }
            failures.insert(url.clone(), Instant::now());
        }
        if let Ok(data) = fetched {
            let ext = ext_of(&data, &url, default_ext);
            let dest = dir.join(format!("{k}.{ext}"));
            let part = dir.join(format!("{k}.part"));
            if std::fs::write(&part, &data).is_ok() && std::fs::rename(&part, &dest).is_ok() {
                if let Some(map) = INDEX.lock().unwrap().as_mut() {
                    map.insert(k.clone(), (dest, Instant::now()));
                }
                if trim_after {
                    trim();
                }
                if let Some(c) = CTX.get() {
                    c.request_repaint();
                }
            }
        }
        if let Some(set) = PENDING.lock().unwrap().as_mut() {
            set.remove(&k);
        }
    }
}

fn file_uri(p: &Path) -> String {
    format!("file://{}", p.to_string_lossy().replace('\\', "/"))
}

/// Address to show a picture from. A cached copy is used when there is one; otherwise the CDN
/// address is returned (shown from memory now) and a copy is saved in the background.
pub fn image_uri(url: &str) -> String {
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return file_uri(Path::new(url));
    }
    let k = key(url);
    if let Some(p) = lookup(&k) {
        return file_uri(&p);
    }
    schedule(url, k, cache_dir(), IMAGE_MAX, "jpg", true);
    url.to_string()
}

/// Same for emoji pictures (never trimmed).
pub fn emoji_uri(url: &str) -> String {
    let k = key(url);
    if let Some(p) = lookup(&k) {
        return file_uri(&p);
    }
    schedule(url, k, emoji_dir(), 1024 * 1024, "png", false);
    url.to_string()
}

/// What to give the player: the cached file if present, else the address itself while a copy is
/// saved for next time.
pub fn video_target(url: &str) -> String {
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return url.to_string();
    }
    let k = key(url);
    if let Some(p) = lookup(&k) {
        return p.to_string_lossy().to_string();
    }
    schedule(url, k, cache_dir(), VIDEO_MAX, "mp4", true);
    url.to_string()
}

static EXISTS: Mutex<Option<HashMap<String, (bool, Instant)>>> = Mutex::new(None);
static FAILED: Mutex<Option<HashMap<String, Instant>>> = Mutex::new(None);

/// Does a local media file still exist (end-to-end encrypted media can be trimmed away)?
/// The answer is remembered for a few seconds: this is asked for every picture on every frame.
pub fn exists(path: &str) -> bool {
    if path.starts_with("http") {
        return true;
    }
    let mut g = EXISTS.lock().unwrap();
    let map = g.get_or_insert_with(HashMap::new);
    if let Some((ok, at)) = map.get(path) {
        if at.elapsed() < Duration::from_secs(5) {
            return *ok;
        }
    }
    let ok = Path::new(path).exists();
    if map.len() > 5000 {
        map.clear();
    }
    map.insert(path.to_string(), (ok, Instant::now()));
    ok
}

fn files_in(dir: &Path, out: &mut Vec<(PathBuf, u64, SystemTime)>) {
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            if let Ok(m) = e.metadata() {
                if m.is_file() {
                    out.push((e.path(), m.len(), m.modified().unwrap_or(SystemTime::UNIX_EPOCH)));
                }
            }
        }
    }
}

/// Bytes used by the picture/video cache.
pub fn usage() -> u64 {
    let mut v = Vec::new();
    files_in(&cache_dir(), &mut v);
    files_in(&e2ee_dir(), &mut v);
    v.iter().map(|f| f.1).sum()
}

/// Delete least recently used files until the cache is under the limit.
pub fn trim() {
    {
        let mut busy = TRIM_BUSY.lock().unwrap();
        if *busy {
            return;
        }
        *busy = true;
    }
    let limit = LIMIT.load(Ordering::Relaxed);
    let mut v = Vec::new();
    files_in(&cache_dir(), &mut v);
    files_in(&e2ee_dir(), &mut v);
    let mut total: u64 = v.iter().map(|f| f.1).sum();
    if total > limit {
        v.sort_by_key(|f| f.2);
        for (p, len, _) in v {
            if total <= limit {
                break;
            }
            if std::fs::remove_file(&p).is_ok() {
                total = total.saturating_sub(len);
                if let (Some(map), Some(stem)) = (INDEX.lock().unwrap().as_mut(), p.file_stem().and_then(|s| s.to_str())) {
                    map.remove(stem);
                }
            }
        }
    }
    *TRIM_BUSY.lock().unwrap() = false;
}

/// Empty the picture/video cache.
pub fn clear() {
    for dir in [cache_dir(), e2ee_dir()] {
        if let Ok(rd) = std::fs::read_dir(dir) {
            for e in rd.flatten() {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
    if let Some(map) = INDEX.lock().unwrap().as_mut() {
        map.retain(|_, (p, _)| p.starts_with(emoji_dir()));
    }
}

/// Save a picture or file (local path or URL) to a path the user chose.
pub fn save_to(source: &str, dest: &Path) -> Result<(), String> {
    if source.starts_with("http://") || source.starts_with("https://") {
        std::fs::write(dest, fetch(source, VIDEO_MAX)?).map_err(|e| e.to_string())
    } else {
        std::fs::copy(source, dest).map(|_| ()).map_err(|e| e.to_string())
    }
}
