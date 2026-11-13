//! Call window. Meta's call signalling (Rsys) is proprietary and nobody has reimplemented it, so
//! calls run in Facebook's own web client inside WebView2 (WebRTC works there). This process:
//!   1. reads the session cookies from stdin (never from argv),
//!   2. seeds them into its own persistent WebView2 profile,
//!   3. opens the conversation on facebook.com/messages in a HIDDEN window and presses the call
//!      button; only the call popup Facebook opens is shown, and the process exits when it closes.
//!
//!   russenger-call <thread-url> <audio|video>      (cookies on stdin)
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

use std::collections::HashSet;
use std::io::Read;
use tao::platform::windows::WindowExtWindows;
use tao::{
    dpi::LogicalSize,
    event::{Event, WindowEvent},
    event_loop::{ControlFlow, EventLoopBuilder},
    window::WindowBuilder,
};
use webview2_com::Microsoft::Web::WebView2::Win32::{
    ICoreWebView2_2, COREWEBVIEW2_PERMISSION_KIND, COREWEBVIEW2_PERMISSION_KIND_CAMERA, COREWEBVIEW2_PERMISSION_KIND_MICROPHONE, COREWEBVIEW2_PERMISSION_STATE_ALLOW,
};
use webview2_com::PermissionRequestedEventHandler;
use windows::core::{Interface, PCWSTR};
use windows_sys::Win32::Foundation::{HWND, LPARAM, RECT};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS};
use windows_sys::Win32::System::Threading::GetCurrentProcessId;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetClassNameW, GetWindow, GetWindowLongW, GetWindowRect, GetWindowTextLengthW, GetWindowThreadProcessId, IsWindowVisible, GWL_EXSTYLE, GW_OWNER,
};
use wry::{WebViewBuilder, WebViewExtWindows};

const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36 Edg/126.0.0.0";

#[derive(Debug, Clone, Copy)]
enum Tick {
    Poll,
    DebugRead,
}

fn icon() -> Option<tao::window::Icon> {
    let rgba = image::load_from_memory(include_bytes!("../../../assets/icon.png")).ok()?.to_rgba8();
    let (w, h) = (rgba.width(), rgba.height());
    tao::window::Icon::from_rgba(rgba.into_raw(), w, h).ok()
}

fn data_dir() -> std::path::PathBuf {
    let base = std::env::var_os("LOCALAPPDATA").map(std::path::PathBuf::from).unwrap_or_else(std::env::temp_dir);
    base.join("Relay").join("webview-call")
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Put the account's cookies into this WebView2 profile through its cookie manager
/// (wry has no setter, and document.cookie cannot create HttpOnly cookies).
fn seed_cookies(webview: &wry::WebView, cookies: &str) -> usize {
    let Ok(core) = (unsafe { webview.controller().CoreWebView2() }) else { return 0 };
    let Ok(core2) = core.cast::<ICoreWebView2_2>() else { return 0 };
    let Ok(mgr) = (unsafe { core2.CookieManager() }) else { return 0 };
    let expires = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0) + 31_536_000.0;
    let mut n = 0;
    for part in cookies.split(';') {
        let Some((name, value)) = part.trim().split_once('=') else { continue };
        if name.is_empty() || value.is_empty() {
            continue;
        }
        let (wn, wv, wd, wp) = (wide(name), wide(value), wide(".facebook.com"), wide("/"));
        unsafe {
            if let Ok(c) = mgr.CreateCookie(PCWSTR(wn.as_ptr()), PCWSTR(wv.as_ptr()), PCWSTR(wd.as_ptr()), PCWSTR(wp.as_ptr())) {
                let http_only = matches!(name, "xs" | "fr" | "sb" | "datr");
                let _ = c.SetIsSecure(true);
                let _ = c.SetIsHttpOnly(http_only);
                let _ = c.SetExpires(expires);
                if mgr.AddOrUpdateCookie(&c).is_ok() {
                    n += 1;
                }
            }
        }
    }
    n
}

/// WebView2 asks the host before a page may use the camera or microphone; without an answer the
/// web client reports "no access to camera and microphone". Calls need both, so allow them.
fn allow_media_permissions(webview: &wry::WebView) {
    let Ok(core) = (unsafe { webview.controller().CoreWebView2() }) else { return };
    let handler = PermissionRequestedEventHandler::create(Box::new(|_, args| {
        let Some(args) = args else { return Ok(()) };
        let mut kind = COREWEBVIEW2_PERMISSION_KIND::default();
        unsafe { args.PermissionKind(&mut kind)? };
        // only Facebook's own pages may use the camera and microphone
        let mut uri = windows::core::PWSTR::null();
        unsafe { args.Uri(&mut uri)? };
        let uri = webview2_com::take_pwstr(uri);
        let ours = ["https://www.facebook.com/", "https://facebook.com/", "https://www.messenger.com/", "https://web.facebook.com/"].iter().any(|h| uri.starts_with(h));
        if ours && (kind == COREWEBVIEW2_PERMISSION_KIND_MICROPHONE || kind == COREWEBVIEW2_PERMISSION_KIND_CAMERA) {
            unsafe { args.SetState(COREWEBVIEW2_PERMISSION_STATE_ALLOW)? };
        }
        Ok(())
    }));
    let mut token = Default::default();
    unsafe {
        let _ = core.add_PermissionRequested(&handler, &mut token);
    }
}

/// Press the header's call button once the page has rendered (English + Vietnamese labels).
/// With `dry` (self-test) it only records that the button exists and never places a call.
fn click_script(video: bool, dry: bool) -> String {
    let re = if video { "video call|gọi video|cuộc gọi video" } else { "voice call|audio call|gọi thoại|gọi âm thanh|cuộc gọi thoại|cuộc gọi âm thanh" };
    let act = if dry { "window.__found=true;" } else { "els[i].click();window.__clicked=true;" };
    format!(
        "(function(){{var re=/{re}/i,done=false,t0=Date.now();\
         var iv=setInterval(function(){{if(done||Date.now()-t0>30000){{clearInterval(iv);return;}}\
         var els=document.querySelectorAll('[aria-label]');\
         for(var i=0;i<els.length;i++){{var l=els[i].getAttribute('aria-label')||'';\
         if(re.test(l)&&els[i].getAttribute('role')!=='img'){{{act}done=true;break;}}}}}},1000);}})();"
    )
}

/// This process plus every process it started (WebView2's browser process, call popups...).
fn process_tree() -> HashSet<u32> {
    let me = unsafe { GetCurrentProcessId() };
    let mut pairs: Vec<(u32, u32)> = Vec::new(); // (pid, parent)
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snap as isize != -1 {
            let mut e: PROCESSENTRY32W = std::mem::zeroed();
            e.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
            let mut ok = Process32FirstW(snap, &mut e) != 0;
            while ok {
                pairs.push((e.th32ProcessID, e.th32ParentProcessID));
                ok = Process32NextW(snap, &mut e) != 0;
            }
            windows_sys::Win32::Foundation::CloseHandle(snap);
        }
    }
    let mut set: HashSet<u32> = HashSet::from([me]);
    loop {
        let before = set.len();
        for (pid, parent) in &pairs {
            if set.contains(parent) {
                set.insert(*pid);
            }
        }
        if set.len() == before {
            return set;
        }
    }
}

struct Scan {
    pids: HashSet<u32>,
    main: isize,
    count: u32,
    /// class + size of every counted window (for the self-test report)
    details: Vec<String>,
}

unsafe extern "system" fn scan_window(hwnd: HWND, lparam: LPARAM) -> i32 {
    let scan = &mut *(lparam as *mut Scan);
    if IsWindowVisible(hwnd) != 0 && hwnd as isize != scan.main {
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, &mut pid);
        if scan.pids.contains(&pid) {
            // A real call window is a sizeable titled window; WebView2 also keeps tiny or
            // message-only helper windows that are flagged visible, which must not count.
            let mut rect: RECT = std::mem::zeroed();
            GetWindowRect(hwnd, &mut rect);
            let (w, h) = (rect.right - rect.left, rect.bottom - rect.top);
            let mut class = [0u16; 64];
            let n = GetClassNameW(hwnd, class.as_mut_ptr(), 64) as usize;
            let class = String::from_utf16_lossy(&class[..n]);
            // the real popup has a title; WebView2 also leaves an untitled twin behind
            let titled = GetWindowTextLengthW(hwnd) > 0;
            let owner = GetWindow(hwnd, GW_OWNER) as isize;
            let ex = GetWindowLongW(hwnd, GWL_EXSTYLE) as u32;
            scan.details.push(format!("{class}:{w}x{h}:titled={titled}:pid={pid}:owner={owner}:ex={ex:#x}:hwnd={}", hwnd as isize));
            if w >= 200 && h >= 150 && titled {
                scan.count += 1;
            }
        }
    }
    1
}

fn scan_popups(main: isize) -> Scan {
    let mut scan = Scan { pids: process_tree(), main, count: 0, details: Vec::new() };
    unsafe {
        EnumWindows(Some(scan_window), &mut scan as *mut Scan as LPARAM);
    }
    scan
}

/// Visible, sizeable top-level windows of ours (and WebView2's) other than the hidden launcher
/// window: that is the call window Facebook opens as a popup.
fn visible_popups(main: isize) -> u32 {
    scan_popups(main).count
}

fn main() {
    let mut args = std::env::args().skip(1);
    let Some(url) = args.next() else { return };
    let video = args.next().as_deref() == Some("video");

    let mut cookies = String::new();
    let _ = std::io::stdin().read_to_string(&mut cookies);

    let debug_path = std::env::var_os("RUSSENGER_CALL_DEBUG");

    let event_loop = EventLoopBuilder::<Tick>::with_user_event().build();
    // The page that places the call stays hidden: only the call window Facebook opens is shown.
    let window = WindowBuilder::new()
        .with_title(if video { "Russenger - Gọi video" } else { "Russenger - Gọi thoại" })
        .with_inner_size(LogicalSize::new(1000.0, 720.0))
        .with_window_icon(icon())
        .with_visible(false)
        .build(&event_loop)
        .expect("window");
    let main_hwnd = window.hwnd() as isize;

    let _ = std::fs::create_dir_all(data_dir());
    let mut ctx = wry::WebContext::new(Some(data_dir()));
    let webview = WebViewBuilder::with_web_context(&mut ctx)
        .with_url("about:blank")
        .with_user_agent(UA)
        .with_initialization_script(&click_script(video, debug_path.is_some()))
        .with_new_window_req_handler(|_| true) // let Facebook open its call popup
        .build(&window)
        .expect("webview (is the WebView2 runtime installed?)");

    // keep the page "visible" to WebView2 although its host window is hidden, so it renders
    // and the call button exists in the DOM
    unsafe {
        let _ = webview.controller().SetIsVisible(true);
    }
    allow_media_permissions(&webview);
    let seeded = seed_cookies(&webview, cookies.trim());
    let _ = webview.load_url(&url);

    let proxy = event_loop.create_proxy();
    std::thread::spawn(move || loop {
        std::thread::sleep(std::time::Duration::from_millis(1000));
        if proxy.send_event(Tick::Poll).is_err() {
            break;
        }
    });

    // self-test hook: report whether the hidden page rendered and the button exists (dry run)
    if debug_path.is_some() {
        let proxy = event_loop.create_proxy();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_secs(16));
            let _ = proxy.send_event(Tick::DebugRead);
        });
    }

    let started = std::time::Instant::now();
    let mut popup_seen = false;
    let mut popup_since: Option<std::time::Instant> = None;
    let mut baseline = 0u32;
    let mut low_polls = 0u32;
    let mut main_shown = false;

    event_loop.run(move |event, _, flow| {
        *flow = ControlFlow::Wait;
        match event {
            Event::UserEvent(Tick::Poll) => {
                if main_shown || debug_path.is_some() {
                    return;
                }
                let n = visible_popups(main_hwnd);
                if n > 0 {
                    let first = *popup_since.get_or_insert_with(std::time::Instant::now);
                    popup_seen = true;
                    // WebView2 shows a popup as a pair of top-level windows. Take the count in the
                    // first seconds as the baseline: if it drops below that and stays there, the
                    // call window was closed even though a leftover twin remains.
                    if first.elapsed() < std::time::Duration::from_secs(3) {
                        baseline = baseline.max(n);
                    }
                    if n < baseline {
                        low_polls += 1;
                        if low_polls >= 3 {
                            *flow = ControlFlow::Exit;
                        }
                    } else {
                        low_polls = 0;
                    }
                } else if popup_seen {
                    // the call window was closed: nothing left to keep alive
                    *flow = ControlFlow::Exit;
                } else if started.elapsed() > std::time::Duration::from_secs(45) {
                    // no call popup appeared (button not found, login page, PIN prompt...): show
                    // the page so the call can be started by hand
                    window.set_visible(true);
                    main_shown = true;
                }
            }
            Event::UserEvent(Tick::DebugRead) => {
                if let Some(path) = &debug_path {
                    let path = path.clone();
                    let scan = scan_popups(main_hwnd);
                    let popups = format!("{} {:?}", scan.count, scan.details);
                    let main_visible = unsafe { IsWindowVisible(main_hwnd as HWND) != 0 };
                    let popups = format!("{popups} main_visible={main_visible}");
                    let _ = webview.evaluate_script_with_callback(
                        "location.pathname + '|rows=' + document.querySelectorAll('[role=row],[role=gridcell]').length + '|aria=' + document.querySelectorAll('[aria-label]').length + '|vis=' + document.visibilityState + '|found=' + (window.__found===true)",
                        move |r| {
                            let _ = std::fs::write(&path, format!("seeded={seeded} popups={popups} page={r}"));
                        },
                    );
                }
            }
            Event::WindowEvent { event: WindowEvent::CloseRequested, .. } => *flow = ControlFlow::Exit,
            _ => {}
        }
    });
}
