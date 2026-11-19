//! Small media player window: plays a video/audio URL or local file in WebView2's
//! built-in player. Started by the app with the target as the only argument.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

#[path = "../paths.rs"]
mod paths;

use tao::{
    dpi::LogicalSize,
    event::{Event, WindowEvent},
    event_loop::{ControlFlow, EventLoop},
    window::WindowBuilder,
};
use wry::WebViewBuilder;

fn icon() -> Option<tao::window::Icon> {
    let rgba = image::load_from_memory(include_bytes!("../../../assets/icon.png")).ok()?.to_rgba8();
    let (w, h) = (rgba.width(), rgba.height());
    tao::window::Icon::from_rgba(rgba.into_raw(), w, h).ok()
}

fn data_dir() -> std::path::PathBuf {
    paths::data_dir().join("webview-player")
}

fn main() {
    let Some(arg) = std::env::args().nth(1) else { return };
    let url = if arg.starts_with("http://") || arg.starts_with("https://") || arg.starts_with("file://") {
        arg.clone()
    } else {
        format!("file:///{}", arg.replace('\\', "/"))
    };

    let event_loop = EventLoop::new();
    let window = WindowBuilder::new()
        .with_title("Russenger - Trình phát")
        .with_inner_size(LogicalSize::new(960.0, 600.0))
        .with_window_icon(icon())
        .build(&event_loop)
        .expect("window");

    let _ = std::fs::create_dir_all(data_dir());
    let mut ctx = wry::WebContext::new(Some(data_dir()));
    let _webview = WebViewBuilder::with_web_context(&mut ctx)
        .with_url(&url)
        // direct media URLs open in WebView2's built-in player; start it right away
        .with_initialization_script("document.addEventListener('DOMContentLoaded',function(){var v=document.querySelector('video,audio');if(v){v.play().catch(function(){});}});")
        .build(&window)
        .expect("media player webview");

    event_loop.run(move |event, _, flow| {
        *flow = ControlFlow::Wait;
        if let Event::WindowEvent { event: WindowEvent::CloseRequested, .. } = event {
            *flow = ControlFlow::Exit;
        }
    });
}
