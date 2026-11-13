//! Windows toast notifications via a hidden PowerShell, plus helpers that start other programs.
use std::process::{Command, Stdio};

fn b64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for c in data.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        out.push(T[(n >> 18 & 63) as usize] as char);
        out.push(T[(n >> 12 & 63) as usize] as char);
        out.push(if c.len() > 1 { T[(n >> 6 & 63) as usize] as char } else { '=' });
        out.push(if c.len() > 2 { T[(n & 63) as usize] as char } else { '=' });
    }
    out
}

// A fresh id: Windows remembers what it first saw for an id, and the old one was shown
// right after being written, so it stuck on the raw name without an icon.
const APP_ID: &str = "Russenger.Desktop";

/// Writes the app icon next to the data so Windows can show it on toasts.
fn icon_path() -> std::path::PathBuf {
    let p = crate::vault::data_dir().join("icon.png");
    let bytes = include_bytes!("../../assets/icon.png");
    if std::fs::metadata(&p).map(|m| m.len() != bytes.len() as u64).unwrap_or(true) {
        let _ = std::fs::write(&p, bytes);
    }
    p
}

const SHORTCUT_PS: &str = r#"
$ErrorActionPreference = 'Stop'
Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
using System.Text;
public static class RussengerShortcut {
  [ComImport, Guid("00021401-0000-0000-C000-000000000046")] class CShellLink {}
  [ComImport, InterfaceType(ComInterfaceType.InterfaceIsIUnknown), Guid("000214F9-0000-0000-C000-000000000046")]
  interface IShellLinkW {
    void GetPath([Out, MarshalAs(UnmanagedType.LPWStr)] StringBuilder f, int cch, IntPtr pfd, uint fl);
    void GetIDList(out IntPtr p); void SetIDList(IntPtr p);
    void GetDescription([Out, MarshalAs(UnmanagedType.LPWStr)] StringBuilder n, int cch);
    void SetDescription([MarshalAs(UnmanagedType.LPWStr)] string n);
    void GetWorkingDirectory([Out, MarshalAs(UnmanagedType.LPWStr)] StringBuilder d, int cch);
    void SetWorkingDirectory([MarshalAs(UnmanagedType.LPWStr)] string d);
    void GetArguments([Out, MarshalAs(UnmanagedType.LPWStr)] StringBuilder a, int cch);
    void SetArguments([MarshalAs(UnmanagedType.LPWStr)] string a);
    void GetHotkey(out short h); void SetHotkey(short h);
    void GetShowCmd(out int s); void SetShowCmd(int s);
    void GetIconLocation([Out, MarshalAs(UnmanagedType.LPWStr)] StringBuilder p, int cch, out int i);
    void SetIconLocation([MarshalAs(UnmanagedType.LPWStr)] string p, int i);
    void SetRelativePath([MarshalAs(UnmanagedType.LPWStr)] string p, uint r);
    void Resolve(IntPtr h, uint f);
    void SetPath([MarshalAs(UnmanagedType.LPWStr)] string p);
  }
  [ComImport, InterfaceType(ComInterfaceType.InterfaceIsIUnknown), Guid("886D8EEB-8CF2-4446-8D02-CDBA1DBDCF99")]
  interface IPropertyStore {
    void GetCount(out uint c); void GetAt(uint i, out PropertyKey k);
    void GetValue(ref PropertyKey k, out PropVariant v);
    void SetValue(ref PropertyKey k, ref PropVariant v); void Commit();
  }
  [ComImport, InterfaceType(ComInterfaceType.InterfaceIsIUnknown), Guid("0000010b-0000-0000-C000-000000000046")]
  interface IPersistFile {
    void GetClassID(out Guid g); [PreserveSig] int IsDirty();
    void Load([MarshalAs(UnmanagedType.LPWStr)] string f, uint m);
    void Save([MarshalAs(UnmanagedType.LPWStr)] string f, [MarshalAs(UnmanagedType.Bool)] bool r);
    void SaveCompleted([MarshalAs(UnmanagedType.LPWStr)] string f);
    void GetCurFile([MarshalAs(UnmanagedType.LPWStr)] out string f);
  }
  [StructLayout(LayoutKind.Sequential, Pack = 4)] public struct PropertyKey { public Guid fmtid; public uint pid; }
  [StructLayout(LayoutKind.Explicit)] public struct PropVariant { [FieldOffset(0)] public ushort vt; [FieldOffset(8)] public IntPtr p; }
  public static void Create(string lnk, string target, string icon, string aumid) {
    var link = (IShellLinkW)new CShellLink();
    link.SetPath(target); link.SetIconLocation(icon, 0); link.SetDescription("Russenger");
    var store = (IPropertyStore)link;
    var key = new PropertyKey { fmtid = new Guid("9F4C2855-9F79-4B39-A8D0-E1D42DE1D5F3"), pid = 5 };
    var pv = new PropVariant { vt = 31, p = Marshal.StringToCoTaskMemUni(aumid) };
    store.SetValue(ref key, ref pv); store.Commit();
    ((IPersistFile)link).Save(lnk, true);
  }
}
'@
[RussengerShortcut]::Create('@LNK@', '@EXE@', '@ICO@', '@ID@')
"#;

/// A Start Menu shortcut carrying our AppUserModelID is what makes Windows show the app's
/// name and icon on toasts. Created once (in the background) when it is missing.
pub fn ensure_shortcut() {
    let Some(appdata) = std::env::var_os("APPDATA") else { return };
    let lnk = std::path::PathBuf::from(appdata).join("Microsoft\\Windows\\Start Menu\\Programs\\Russenger.lnk");
    let Ok(exe) = std::env::current_exe() else { return };
    let ico = crate::vault::data_dir().join("icon.ico");
    let ico_bytes = include_bytes!("../../assets/icon.ico");
    if std::fs::metadata(&ico).map(|m| m.len() != ico_bytes.len() as u64).unwrap_or(true) {
        let _ = std::fs::write(&ico, ico_bytes);
    }
    // the stamp records which exe the shortcut points at, so a moved install fixes itself
    let stamp = crate::vault::data_dir().join("shortcut.stamp");
    let want = exe.to_string_lossy().to_string();
    if lnk.exists() && std::fs::read_to_string(&stamp).map(|s| s == want).unwrap_or(false) {
        return;
    }
    let q = |s: &str| s.replace('\'', "''");
    let script = SHORTCUT_PS
        .replace("@LNK@", &q(&lnk.to_string_lossy()))
        .replace("@EXE@", &q(&want))
        .replace("@ICO@", &q(&ico.to_string_lossy()))
        .replace("@ID@", APP_ID);
    let utf16: Vec<u8> = script.encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
    let mut cmd = Command::new("powershell.exe");
    cmd.args(["-NoProfile", "-NonInteractive", "-WindowStyle", "Hidden", "-EncodedCommand", &b64(&utf16)])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000);
    }
    if let Ok(mut child) = cmd.spawn() {
        std::thread::spawn(move || {
            if matches!(child.wait(), Ok(s) if s.success()) {
                let _ = std::fs::write(stamp, want);
            }
        });
    }
}

/// Registers our AppUserModelID (display name + icon) in HKCU at startup, well before the
/// first toast, so notifications are attributed to Russenger.
pub fn register_app_id() {
    use windows_sys::Win32::System::Registry::{RegCloseKey, RegCreateKeyExW, RegSetValueExW, HKEY, HKEY_CURRENT_USER, KEY_SET_VALUE, REG_DWORD, REG_SZ};
    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }
    let icon = icon_path();
    let subkey = wide(&format!("Software\\Classes\\AppUserModelId\\{APP_ID}"));
    unsafe {
        let mut key: HKEY = std::ptr::null_mut();
        if RegCreateKeyExW(HKEY_CURRENT_USER, subkey.as_ptr(), 0, std::ptr::null(), 0, KEY_SET_VALUE, std::ptr::null(), &mut key, std::ptr::null_mut()) != 0 {
            return;
        }
        let set_sz = |name: &str, value: &str| {
            let (n, v) = (wide(name), wide(value));
            RegSetValueExW(key, n.as_ptr(), 0, REG_SZ, v.as_ptr() as *const u8, (v.len() * 2) as u32);
        };
        set_sz("DisplayName", "Russenger");
        // .ico like other desktop apps use; the png also goes into the toast body below
        let ico = icon.with_extension("ico");
        let _ = std::fs::write(&ico, include_bytes!("../../assets/icon.ico"));
        set_sz("IconUri", &ico.to_string_lossy());
        set_sz("IconBackgroundColor", "FF0866FF");
        let n = wide("ShowInSettings");
        let one: u32 = 1;
        RegSetValueExW(key, n.as_ptr(), 0, REG_DWORD, &one as *const u32 as *const u8, 4);
        RegCloseKey(key);
    }
}

pub fn toast(title: &str, body: &str, tag: &str) {
    // The text travels in environment variables and is escaped by PowerShell itself, so no
    // character of a message (quotes of any kind included) can ever become script code.
    let body: String = body.chars().take(200).collect();
    let script = "[Windows.UI.Notifications.ToastNotificationManager, Windows.UI.Notifications, ContentType = WindowsRuntime] | Out-Null;          [Windows.Data.Xml.Dom.XmlDocument, Windows.Data.Xml.Dom.XmlDocument, ContentType = WindowsRuntime] | Out-Null;          $e = { param($s) [System.Security.SecurityElement]::Escape($s) };          $x = New-Object Windows.Data.Xml.Dom.XmlDocument;          $x.LoadXml('<toast><visual><binding template=\"ToastGeneric\"><image placement=\"appLogoOverride\" hint-crop=\"circle\" src=\"' + (& $e $env:RUSSENGER_TOAST_ICON) + '\"/><text>' + (& $e $env:RUSSENGER_TOAST_TITLE) + '</text><text>' + (& $e $env:RUSSENGER_TOAST_BODY) + '</text></binding></visual></toast>');          $t = [Windows.UI.Notifications.ToastNotification]::new($x);          if ($env:RUSSENGER_TOAST_TAG) { $t.Tag = $env:RUSSENGER_TOAST_TAG; $t.Group = 'messages' }; \
         [Windows.UI.Notifications.ToastNotificationManager]::CreateToastNotifier($env:RUSSENGER_TOAST_ID).Show($t)";
    let utf16: Vec<u8> = script.encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
    let mut cmd = Command::new("powershell.exe");
    cmd.args(["-NoProfile", "-NonInteractive", "-WindowStyle", "Hidden", "-EncodedCommand", &b64(&utf16)])
        .env("RUSSENGER_TOAST_TITLE", title)
        .env("RUSSENGER_TOAST_BODY", &body)
        .env("RUSSENGER_TOAST_ID", APP_ID)
        // a toast with the same tag replaces the earlier one instead of stacking
        .env("RUSSENGER_TOAST_TAG", tag.chars().filter(|c| c.is_ascii_alphanumeric()).take(40).collect::<String>())
        .env("RUSSENGER_TOAST_ICON", format!("file:///{}", icon_path().to_string_lossy().replace('\\', "/")))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000);
    }
    let _ = cmd.spawn();
}

/// Open a web link in the default browser. Only http(s) is ever handed to the shell: a link
/// from a message must never be able to start a program or open a local path.
pub fn open_url(target: &str) {
    let t = target.trim();
    if !(t.starts_with("https://") || t.starts_with("http://")) || t.chars().any(|c| c.is_control()) {
        return;
    }
    let mut cmd = Command::new("rundll32.exe");
    cmd.args(["url.dll,FileProtocolHandler", t]).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000);
    }
    let _ = cmd.spawn();
}

/// Show a file in Explorer (selected), without running it.
pub fn reveal(path: &str) {
    let mut cmd = Command::new("explorer.exe");
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.raw_arg(format!("/select,\"{}\"", path.replace('/', "\\").replace('"', "")));
    }
    let _ = cmd.spawn();
}

/// Start a call window for a conversation. Calls run in Facebook's web client inside
/// WebView2 (Meta's call signalling is proprietary); the cookies go over stdin, not argv.
pub fn call(thread_url: &str, video: bool, cookies: &str) -> bool {
    use std::io::Write;
    let exe = std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.join("russenger-call.exe")));
    let Some(exe) = exe.filter(|p| p.exists()) else { return false };
    let mut cmd = Command::new(exe);
    cmd.args([thread_url, if video { "video" } else { "audio" }]).stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::null());
    match cmd.spawn() {
        Ok(mut child) => {
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(cookies.as_bytes());
            }
            true
        }
        Err(_) => false,
    }
}

/// Play a video or audio file/URL in the small built-in player window.
pub fn play(target: &str) {
    let player = std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.join("russenger-player.exe")));
    match player.filter(|p| p.exists()) {
        Some(exe) => {
            let mut cmd = Command::new(exe);
            cmd.arg(target).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
            let _ = cmd.spawn();
        }
        None => open_url(target),
    }
}

const RUN_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Is Russenger set to start with Windows?
pub fn autostart_enabled() -> bool {
    use windows_sys::Win32::System::Registry::{RegCloseKey, RegOpenKeyExW, RegQueryValueExW, HKEY, HKEY_CURRENT_USER, KEY_QUERY_VALUE};
    unsafe {
        let mut key: HKEY = std::ptr::null_mut();
        if RegOpenKeyExW(HKEY_CURRENT_USER, wide(RUN_KEY).as_ptr(), 0, KEY_QUERY_VALUE, &mut key) != 0 {
            return false;
        }
        let r = RegQueryValueExW(key, wide("Russenger").as_ptr(), std::ptr::null(), std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut());
        RegCloseKey(key);
        r == 0
    }
}

/// Start with Windows on or off (per-user Run key).
pub fn set_autostart(on: bool) {
    use windows_sys::Win32::System::Registry::{RegCloseKey, RegDeleteValueW, RegOpenKeyExW, RegSetValueExW, HKEY, HKEY_CURRENT_USER, KEY_SET_VALUE, REG_SZ};
    unsafe {
        let mut key: HKEY = std::ptr::null_mut();
        if RegOpenKeyExW(HKEY_CURRENT_USER, wide(RUN_KEY).as_ptr(), 0, KEY_SET_VALUE, &mut key) != 0 {
            return;
        }
        let name = wide("Russenger");
        if on {
            if let Ok(exe) = std::env::current_exe() {
                let v = wide(&format!("\"{}\" --minimized", exe.to_string_lossy()));
                RegSetValueExW(key, name.as_ptr(), 0, REG_SZ, v.as_ptr() as *const u8, (v.len() * 2) as u32);
            }
        } else {
            RegDeleteValueW(key, name.as_ptr());
        }
        RegCloseKey(key);
    }
}
