//! Notification-area icon: closing the window can keep Russenger running in the background,
//! and a click on the icon (or on "Mở Russenger") brings the window back.
use tray_icon::menu::{Menu, MenuEvent, MenuItem};
use tray_icon::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};
use windows_sys::Win32::Foundation::{BOOL, HWND, LPARAM, RECT};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetWindowLongPtrW, GetWindowRect, GetWindowTextW, GetWindowThreadProcessId, SetForegroundWindow, SetWindowLongPtrW, SetWindowPos, ShowWindow, GWL_EXSTYLE, SWP_NOACTIVATE, SWP_NOSIZE,
    SWP_NOZORDER, SW_HIDE, SW_RESTORE, SW_SHOW, SW_SHOWNOACTIVATE, WS_EX_APPWINDOW, WS_EX_TOOLWINDOW,
};

// A window that is really hidden (SW_HIDE) is no longer redrawn, so the program would stop
// handling messages and showing notifications. "Hidden" therefore means: no taskbar button and
// parked far off screen; the window stays alive and keeps running.
/// True while the window is parked in the tray (the heartbeat repaint is only needed then).
pub static PARKED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

static PARKED_AT: std::sync::Mutex<Option<(i32, i32)>> = std::sync::Mutex::new(None);

/// Give the window its icons straight from the program's own icon resource (the one the build
/// embeds), at the sizes the title bar and the taskbar ask for.
pub fn set_window_icons(hwnd: isize) {
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, LoadImageW, SendMessageW, ICON_BIG, ICON_SMALL, IMAGE_ICON, LR_SHARED, SM_CXICON, SM_CXSMICON, SM_CYICON, SM_CYSMICON, WM_SETICON};
    unsafe {
        let module = GetModuleHandleW(std::ptr::null());
        // winresource stores the icon as resource id 1
        let load = |w: i32, h: i32| LoadImageW(module, 1 as *const u16, IMAGE_ICON, w, h, LR_SHARED);
        let big = load(GetSystemMetrics(SM_CXICON), GetSystemMetrics(SM_CYICON));
        let small = load(GetSystemMetrics(SM_CXSMICON), GetSystemMetrics(SM_CYSMICON));
        if !big.is_null() {
            SendMessageW(hwnd as HWND, WM_SETICON, ICON_BIG as usize, big as isize);
        }
        if !small.is_null() {
            SendMessageW(hwnd as HWND, WM_SETICON, ICON_SMALL as usize, small as isize);
        }
    }
}

pub fn hide(hwnd: isize) {
    unsafe {
        let h = hwnd as HWND;
        let mut r: RECT = std::mem::zeroed();
        GetWindowRect(h, &mut r);
        let mut g = PARKED_AT.lock().unwrap();
        if g.is_none() && r.left > -30000 {
            *g = Some((r.left, r.top));
        }
        ShowWindow(h, SW_HIDE);
        let ex = GetWindowLongPtrW(h, GWL_EXSTYLE);
        SetWindowLongPtrW(h, GWL_EXSTYLE, (ex | WS_EX_TOOLWINDOW as isize) & !(WS_EX_APPWINDOW as isize));
        SetWindowPos(h, std::ptr::null_mut(), -32000, -32000, 0, 0, SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE);
        ShowWindow(h, SW_SHOWNOACTIVATE);
        PARKED.store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

pub fn show(hwnd: isize) {
    unsafe {
        let h = hwnd as HWND;
        ShowWindow(h, SW_HIDE);
        let ex = GetWindowLongPtrW(h, GWL_EXSTYLE);
        SetWindowLongPtrW(h, GWL_EXSTYLE, (ex & !(WS_EX_TOOLWINDOW as isize)) | WS_EX_APPWINDOW as isize);
        // the saved spot lives in the program that parked the window; another launch that brings
        // it back does not have it, so fall back to a fixed place on screen
        let saved = PARKED_AT.lock().unwrap().take();
        let mut r: RECT = std::mem::zeroed();
        GetWindowRect(h, &mut r);
        let target = saved.or(if r.left < -20000 { Some((120, 80)) } else { None });
        if let Some((x, y)) = target {
            SetWindowPos(h, std::ptr::null_mut(), x, y, 0, 0, SWP_NOSIZE | SWP_NOZORDER);
        }
        ShowWindow(h, SW_SHOW);
        ShowWindow(h, SW_RESTORE);
        SetForegroundWindow(h);
        PARKED.store(false, std::sync::atomic::Ordering::Relaxed);
    }
}

/// Create the tray icon. The returned value must be kept alive for the icon to stay.
pub fn create(hwnd: isize) -> Option<TrayIcon> {
    let menu = Menu::new();
    let open = MenuItem::with_id("open", "Mở Russenger", true, None);
    let quit = MenuItem::with_id("quit", "Thoát", true, None);
    menu.append_items(&[&open, &quit]).ok()?;
    let img = image::load_from_memory(include_bytes!("../../assets/icon.png")).ok()?.to_rgba8();
    let (w, h) = (img.width(), img.height());
    let icon = tray_icon::Icon::from_rgba(img.into_raw(), w, h).ok()?;
    let tray = TrayIconBuilder::new().with_menu(Box::new(menu)).with_menu_on_left_click(false).with_tooltip("Russenger").with_icon(icon).build().ok()?;
    std::thread::spawn(move || loop {
        while let Ok(ev) = TrayIconEvent::receiver().try_recv() {
            if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = ev {
                show(hwnd);
            }
        }
        while let Ok(ev) = MenuEvent::receiver().try_recv() {
            match ev.id.0.as_str() {
                "open" => show(hwnd),
                // the helper is tied to this process by a job object and ends with it
                "quit" => std::process::exit(0),
                _ => {}
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    });
    Some(tray)
}

/// A second launch brings the first one's window back (it may be hidden in the tray).
pub fn show_existing() {
    unsafe extern "system" fn each(hwnd: HWND, _: LPARAM) -> BOOL {
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, &mut pid);
        if pid != std::process::id() {
            let mut buf = [0u16; 64];
            let n = GetWindowTextW(hwnd, buf.as_mut_ptr(), 64) as usize;
            let title = String::from_utf16_lossy(&buf[..n.min(64)]);
            if title == "Russenger" || title.starts_with("Russenger (") {
                // only our own program's window: same executable name
                if let Some(name) = exe_of(pid) {
                    if name.eq_ignore_ascii_case("russenger.exe") {
                        show(hwnd as isize);
                        return 0;
                    }
                }
            }
        }
        1
    }
    unsafe {
        EnumWindows(Some(each), 0);
    }
}

fn exe_of(pid: u32) -> Option<String> {
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS};
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        let mut e: PROCESSENTRY32W = std::mem::zeroed();
        e.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        let mut ok = Process32FirstW(snap, &mut e);
        while ok != 0 {
            if e.th32ProcessID == pid {
                let len = e.szExeFile.iter().position(|c| *c == 0).unwrap_or(e.szExeFile.len());
                windows_sys::Win32::Foundation::CloseHandle(snap);
                return Some(String::from_utf16_lossy(&e.szExeFile[..len]));
            }
            ok = Process32NextW(snap, &mut e);
        }
        windows_sys::Win32::Foundation::CloseHandle(snap);
    }
    None
}
