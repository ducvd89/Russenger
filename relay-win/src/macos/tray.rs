//! Menu bar controls for the main window.
use eframe::egui::{Context, ViewportCommand};
use std::sync::atomic::{AtomicBool, Ordering};
use tray_icon::menu::{Menu, MenuEvent, MenuItem};
use tray_icon::{TrayIcon, TrayIconBuilder};

pub static PARKED: AtomicBool = AtomicBool::new(false);

pub fn hide(ctx: &Context) {
    PARKED.store(true, Ordering::Relaxed);
    ctx.send_viewport_cmd(ViewportCommand::Minimized(true));
}

pub fn show(ctx: &Context) {
    PARKED.store(false, Ordering::Relaxed);
    ctx.send_viewport_cmd(ViewportCommand::Minimized(false));
    ctx.send_viewport_cmd(ViewportCommand::Focus);
}

pub fn create(ctx: Context) -> Option<TrayIcon> {
    let menu = Menu::new();
    let open = MenuItem::with_id("open", "Mở Russenger", true, None);
    let quit = MenuItem::with_id("quit", "Thoát", true, None);
    menu.append_items(&[&open, &quit]).ok()?;
    let img = image::load_from_memory(include_bytes!("../../../assets/icon.png"))
        .ok()?
        .resize_exact(36, 36, image::imageops::FilterType::Lanczos3)
        .to_rgba8();
    let icon = tray_icon::Icon::from_rgba(img.into_raw(), 36, 36).ok()?;
    let tray = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip("Russenger")
        .with_icon(icon)
        .build()
        .ok()?;
    std::thread::spawn(move || {
        for event in MenuEvent::receiver() {
            match event.id.0.as_str() {
                "open" => show(&ctx),
                "quit" => {
                    PARKED.store(false, Ordering::Relaxed);
                    crate::notify::QUIT_REQUESTED.store(true, Ordering::Relaxed);
                    ctx.send_viewport_cmd(ViewportCommand::Close);
                    ctx.request_repaint();
                }
                _ => {}
            }
        }
    });
    Some(tray)
}

pub fn show_existing() {
    let _ = std::fs::write(crate::notify::open_request_file(), "");
}
