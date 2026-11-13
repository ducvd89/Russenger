#![cfg_attr(windows, windows_subsystem = "windows")]

mod app;
mod backend;
mod cache;
mod db;
mod emoji;
mod model;
mod notify;
mod theme;
mod tray;
mod vault;

use eframe::egui;
use std::sync::Arc;

/// Segoe UI first (Latin + Vietnamese), then emoji/symbol fonts as fallbacks.
/// egui renders COLR emoji monochrome; this still beats empty boxes.
fn install_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    let windir = std::env::var("WINDIR").unwrap_or_else(|_| "C:\\Windows".into());
    let try_load = |fonts: &mut egui::FontDefinitions, name: &str, file: &str, front: bool| {
        if let Ok(bytes) = std::fs::read(format!("{windir}\\Fonts\\{file}")) {
            fonts.font_data.insert(name.to_owned(), Arc::new(egui::FontData::from_owned(bytes)));
            for fam in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
                let list = fonts.families.entry(fam).or_default();
                if front {
                    list.insert(0, name.to_owned());
                } else {
                    list.push(name.to_owned());
                }
            }
        }
    };
    try_load(&mut fonts, "segoeui", "segoeui.ttf", true);
    try_load(&mut fonts, "segoeemoji", "seguiemj.ttf", false);
    try_load(&mut fonts, "segoesym", "seguisym.ttf", false);

    // Real semibold weight (egui's strong() only changes colour). Always define the family.
    let mut semibold: Vec<String> = Vec::new();
    if let Ok(bytes) = std::fs::read(format!("{windir}\\Fonts\\seguisb.ttf")) {
        fonts.font_data.insert("segoeuisb".to_owned(), Arc::new(egui::FontData::from_owned(bytes)));
        semibold.push("segoeuisb".to_owned());
    }
    semibold.extend(fonts.families.get(&egui::FontFamily::Proportional).cloned().unwrap_or_default());
    fonts.families.insert(egui::FontFamily::Name("semibold".into()), semibold);
    ctx.set_fonts(fonts);
}

fn window_icon() -> egui::IconData {
    let img = image::load_from_memory(include_bytes!("../../assets/icon.png")).map(|i| i.to_rgba8());
    match img {
        Ok(rgba) => egui::IconData { width: rgba.width(), height: rgba.height(), rgba: rgba.into_raw() },
        Err(_) => egui::IconData::default(),
    }
}

/// Leave a trace of a crash in %LOCALAPPDATA%\\Relay\\crash.log (the program has no console).
fn install_crash_log() {
    std::panic::set_hook(Box::new(|info| {
        use std::io::Write;
        let bt = std::backtrace::Backtrace::force_capture();
        let thread = std::thread::current();
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(vault::data_dir().join("crash.log")) {
            let _ = writeln!(f, "--- {} thread {:?}\n{info}\n{bt}", chrono::Local::now().format("%Y-%m-%d %H:%M:%S"), thread.name());
        }
    }));
}

fn main() -> eframe::Result {
    install_crash_log();
    // Tie this process to the same app identity as the Start Menu shortcut and the toasts, so the
    // taskbar button gets the app's own name and icon.
    {
        let id: Vec<u16> = "Russenger.Window".encode_utf16().chain(std::iter::once(0)).collect();
        unsafe {
            windows_sys::Win32::UI::Shell::SetCurrentProcessExplicitAppUserModelID(id.as_ptr());
        }
    }
    // russenger --import-cookies <file>: store a "name=value; ..." cookie file in the DPAPI vault.
    let args: Vec<String> = std::env::args().collect();
    if args.len() == 3 && args[1] == "--import-cookies" {
        if let Ok(c) = std::fs::read_to_string(&args[2]) {
            vault::save_cookies(c.trim());
        }
        return Ok(());
    }
    if args.len() == 2 && args[1] == "--register-only" {
        notify::register_app_id();
        notify::ensure_shortcut();
        std::thread::sleep(std::time::Duration::from_secs(8)); // let the shortcut script finish
        return Ok(());
    }
    // register the toast identity at every start, long before the first notification
    notify::register_app_id();
    notify::ensure_shortcut();
    if args.len() == 2 && args[1] == "--test-toast" {
        notify::toast("Russenger", "Thông báo thử", "test");
        std::thread::sleep(std::time::Duration::from_secs(3));
        return Ok(());
    }
    let demo = args.iter().any(|a| a == "--demo");
    if !demo && !vault::acquire_single_instance() {
        // Two helpers on one wadevice.db would corrupt the E2EE sessions. Bring the running
        // window forward (it may be hidden in the tray) instead.
        tray::show_existing();
        return Ok(());
    }
    vault::kill_children_on_exit();

    let mut options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1080.0, 720.0])
            .with_min_inner_size([640.0, 420.0])
            .with_title("Russenger")
            // started by Windows at sign-in: stay in the tray until the icon is clicked
            .with_visible(!args.iter().any(|a| a == "--minimized"))
            .with_icon(window_icon()),
        ..Default::default()
    };
    // While the window is dragged Windows sends a move event for every step and egui redraws for
    // each one; with vsync on, every redraw then waits for the display, which makes dragging feel
    // sluggish. Present without waiting, and keep at most one frame queued.
    options.wgpu_options.present_mode = eframe::wgpu::PresentMode::AutoNoVsync;
    options.wgpu_options.desired_maximum_frame_latency = Some(1);
    eframe::run_native(
        "Russenger",
        options,
        Box::new(|cc| {
            egui_extras::install_image_loaders(&cc.egui_ctx);
            install_fonts(&cc.egui_ctx);
            Ok(Box::new(app::App::new(cc)))
        }),
    )
}
