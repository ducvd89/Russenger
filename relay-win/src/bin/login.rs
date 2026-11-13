//! Separate login process: shows facebook.com/login in WebView2, and once the
//! session cookies (c_user + xs) exist prints "name=value; ..." to stdout and exits.
//! Runs out-of-process so it never fights eframe's event loop.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

use std::io::Write;
use tao::{
    dpi::LogicalSize,
    event::{Event, WindowEvent},
    event_loop::{ControlFlow, EventLoopBuilder},
    window::WindowBuilder,
};
use wry::WebViewBuilder;

const LOGIN_URL: &str = "https://www.facebook.com/login";
const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36 Edg/126.0.0.0";

#[derive(Debug, Clone, Copy)]
enum Tick {
    Poll,
}

fn data_dir() -> std::path::PathBuf {
    let base = std::env::var_os("LOCALAPPDATA").map(std::path::PathBuf::from).unwrap_or_else(std::env::temp_dir);
    base.join("Relay").join("webview")
}

fn icon() -> Option<tao::window::Icon> {
    let rgba = image::load_from_memory(include_bytes!("../../../assets/icon.png")).ok()?.to_rgba8();
    let (w, h) = (rgba.width(), rgba.height());
    tao::window::Icon::from_rgba(rgba.into_raw(), w, h).ok()
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
    let webview = WebViewBuilder::with_web_context(&mut ctx)
        .with_url(LOGIN_URL)
        .with_user_agent(UA)
        .build(&window)
        .expect("webview (is the WebView2 runtime installed?)");

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
                if let Ok(cookies) = webview.cookies_for_url("https://www.facebook.com/") {
                    let has = |n: &str| cookies.iter().any(|c| c.name() == n && !c.value().is_empty());
                    if has("c_user") && has("xs") {
                        let line = cookies
                            .iter()
                            .map(|c| format!("{}={}", c.name(), c.value()))
                            .collect::<Vec<_>>()
                            .join("; ");
                        let mut out = std::io::stdout();
                        let _ = out.write_all(line.as_bytes());
                        let _ = out.flush();
                        *flow = ControlFlow::Exit;
                    }
                }
            }
            Event::WindowEvent { event: WindowEvent::CloseRequested, .. } => {
                *flow = ControlFlow::Exit;
            }
            _ => {}
        }
    });
}
