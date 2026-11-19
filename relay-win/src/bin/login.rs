//! Separate login process: shows facebook.com/login in the system webview, and once the
//! session cookies (c_user + xs) exist prints "name=value; ..." to stdout and exits.
//! Runs out-of-process so it never fights eframe's event loop.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

#[path = "../paths.rs"]
mod paths;

use std::io::Write;
use tao::{
    dpi::LogicalSize,
    event::{Event, WindowEvent},
    event_loop::{ControlFlow, EventLoopBuilder},
    window::WindowBuilder,
};
use wry::{cookie::Cookie, WebView, WebViewBuilder};

const LOGIN_URL: &str = "https://www.facebook.com/login";
#[cfg(windows)]
const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36 Edg/126.0.0.0";

#[derive(Debug, Clone, Copy)]
enum Tick {
    Poll,
}

fn data_dir() -> std::path::PathBuf {
    paths::data_dir().join("webview")
}

fn icon() -> Option<tao::window::Icon> {
    let rgba = image::load_from_memory(include_bytes!("../../../assets/icon.png")).ok()?.to_rgba8();
    let (w, h) = (rgba.width(), rgba.height());
    tao::window::Icon::from_rgba(rgba.into_raw(), w, h).ok()
}

#[cfg(target_os = "macos")]
fn facebook_root_cookie(cookie: &Cookie<'_>) -> bool {
    matches!(cookie.domain(), Some("facebook.com" | "www.facebook.com")) && cookie.path() == Some("/")
}

fn read_session(webview: &WebView) -> Option<String> {
    #[cfg(windows)]
    let cookies = webview.cookies_for_url("https://www.facebook.com/").ok()?;
    // Wry's WKWebView URL lookup compares domains for equality. Facebook sets
    // session cookies on the parent domain, so select those from WKHTTPCookieStore.
    #[cfg(target_os = "macos")]
    let cookies = webview.cookies().ok()?.into_iter().filter(facebook_root_cookie).collect();
    session_header(cookies)
}

fn session_header(cookies: Vec<Cookie<'_>>) -> Option<String> {
    let has = |name| cookies.iter().any(|c| c.name() == name && !c.value().is_empty());
    if !has("c_user") || !has("xs") {
        return None;
    }
    Some(cookies.iter().map(|c| format!("{}={}", c.name(), c.value())).collect::<Vec<_>>().join("; "))
}

fn main() {
    let event_loop = EventLoopBuilder::<Tick>::with_user_event().build();
    let window = WindowBuilder::new()
        .with_title("Russenger - Đăng nhập Facebook")
        .with_window_icon(icon())
        .with_inner_size(LogicalSize::new(520.0, 760.0))
        .build(&event_loop)
        .expect("window");

    let _ = std::fs::create_dir_all(data_dir());
    let mut ctx = wry::WebContext::new(Some(data_dir()));
    let builder = WebViewBuilder::with_web_context(&mut ctx).with_url(LOGIN_URL);
    #[cfg(windows)]
    let builder = builder.with_user_agent(UA);
    #[cfg(target_os = "macos")]
    let builder = builder.with_incognito(true);
    let webview = builder.build(&window).expect("Facebook login webview");

    let proxy = event_loop.create_proxy();
    std::thread::spawn(move || loop {
        std::thread::sleep(std::time::Duration::from_millis(1500));
        if proxy.send_event(Tick::Poll).is_err() {
            break;
        }
    });

    event_loop.run(move |event, _, flow| {
        *flow = ControlFlow::Wait;
        match event {
            Event::UserEvent(Tick::Poll) => {
                if let Some(session) = read_session(&webview) {
                    let mut out = std::io::stdout().lock();
                    let _ = out.write_all(session.as_bytes()).and_then(|_| out.flush());
                    *flow = ControlFlow::Exit;
                }
            }
            Event::WindowEvent {
                event: WindowEvent::CloseRequested,
                ..
            } => {
                *flow = ControlFlow::Exit;
            }
            _ => {}
        }
    });
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;

    #[test]
    fn hands_off_facebook_parent_domain_session() {
        let cookies = vec![
            Cookie::build(("c_user", "test-user")).domain(".facebook.com").path("/").build(),
            Cookie::build(("xs", "test-session")).domain(".facebook.com").path("/").http_only(true).build(),
            Cookie::build(("locale", "vi_VN")).domain("www.facebook.com").path("/").build(),
            Cookie::build(("sibling", "ignored")).domain("m.facebook.com").path("/").build(),
            Cookie::build(("other", "ignored")).domain("facebook.com.example").path("/").build(),
            Cookie::build(("checkpoint", "ignored")).domain(".facebook.com").path("/checkpoint").build(),
        ];
        assert_eq!(
            session_header(cookies.into_iter().filter(facebook_root_cookie).collect()).as_deref(),
            Some("c_user=test-user; xs=test-session; locale=vi_VN"),
        );
    }

    #[test]
    fn waits_for_both_session_cookies() {
        let cookies = vec![Cookie::new("c_user", "test-user"), Cookie::new("xs", "")];
        assert_eq!(session_header(cookies), None);
    }
}
