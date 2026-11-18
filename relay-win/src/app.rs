use crate::backend::Backend;
use crate::db::Db;
use crate::model::{is_placeholder, Message, Store, Thread};
use crate::theme::{self, Palette};
use crate::{cache, notify, vault};
use chrono::{Datelike, TimeZone};
use eframe::egui::{
    self, text::LayoutJob, Align, Align2, Color32, CornerRadius, FontId, Layout, Margin, RichText, Sense, TextFormat, Vec2,
};
use serde_json::json;
use std::collections::HashSet;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::time::{Duration, Instant};

const REACTIONS: [&str; 6] = ["👍", "❤️", "😂", "😮", "😢", "😡"];
const ROW_H: f32 = 62.0;

#[derive(PartialEq)]
enum Screen {
    Login,
    Main,
}

enum Action {
    Select(String),
    Send,
    Attach,
    React { thread: String, id: String, emoji: String, from_me: bool },
    Unsend { thread: String, id: String },
    StartEdit { id: String, text: String },
    StartReply { id: String, text: String, sender: String },
    CancelCompose,
    Mute { thread: String, muted: bool },
    DeleteThread(String),
    LoadEarlier,
    OpenUrl(String),
    SaveFile(String),
    Settings,
    Copy(String),
    Logout,
    Retry,
    RestoreHistory,
    ToggleTheme,
    ScrollBottom,
    ViewImage { thread: String, id: String },
    Play(String),
    Call { thread: String, video: bool },
    SendSticker { thread: String, id: String },
    MoreThreads,
}

/// Left-rail tabs, like Messenger web's sidebar (and Telegram's folders).
#[derive(Clone, PartialEq, Eq, Hash)]
enum Tab {
    Chats,
    Marketplace,
    Requests,
    Archive,
    Community(String),
}

/// Which tab a thread lives in (None = hidden: spam, disabled, ...).
fn tab_of(t: &Thread) -> Option<Tab> {
    let in_community = (17..=21).contains(&t.ttype) && !t.parent.is_empty() && t.parent != "0" && t.parent != "-1";
    if in_community {
        return Some(Tab::Community(t.parent.clone()));
    }
    if t.ttype == 5 {
        return Some(Tab::Marketplace);
    }
    match t.folder.as_str() {
        "" | "inbox" => Some(Tab::Chats),
        "requests" => Some(Tab::Requests),
        "archived" => Some(Tab::Archive),
        _ => None,
    }
}

/// What a click on a spot inside a bubble does (the bubble itself also senses clicks).
enum Hit {
    Url(String),
    Image(String),
    Play(String),
    File(String),
}

/// Full-window picture viewer with zoom, pan and previous/next.
struct Viewer {
    /// (message id, uri) of every picture in the thread
    items: Vec<(String, String)>,
    index: usize,
    zoom: f32,
    offset: Vec2,
}

pub struct App {
    ctx: egui::Context,
    store: Store,
    backend: Option<Backend>,
    screen: Screen,
    cookies: Option<String>,
    paste: String,
    login_status: String,
    login_rx: Option<Receiver<String>>,
    selected: Option<String>,
    input: String,
    reply: Option<(String, String, String)>,
    editing: Option<String>,
    search: String,
    tag: u64,
    last_toast: Instant,
    reconnect_at: Option<Instant>,
    pick_tx: Sender<(String, PathBuf)>,
    pick_rx: Receiver<(String, PathBuf)>,
    requested: HashSet<String>,
    focus_input: bool,
    pin: String,
    history_asked: bool,
    more_threads_asked: bool,
    backend_started: Instant,
    dark: bool,
    scroll_bottom: bool,
    seen_len: usize,
    /// threads already checked for old attachment messages that lack their picture
    repaired: HashSet<String>,
    viewer: Option<Viewer>,
    tab: Tab,
    emoji_open: bool,
    sticker_open: bool,
    /// failed connection attempts in a row (drives the retry delay)
    retry: u32,
    last_title: String,
    settings_open: bool,
    cache_mb: u64,
    cache_used: u64,
    cache_checked: Instant,
    autostart: bool,
    /// text of recently sent messages by client tag, so a failed send can be put back in the box
    sent_texts: std::collections::HashMap<String, String>,
    /// when the oldest not yet shown notification arrived (a burst is collected into one toast)
    pending_since: Option<Instant>,
    /// measured height of each message row (by id -> (layout signature, height)), so rows far outside
    /// the window can be skipped while automatically invalidating when text/media/link/grouping changes
    row_heights: std::collections::HashMap<String, (u64, f32)>,
    /// last incoming message id acknowledged to server per thread (so arriving history re-syncs E2EE receipts)
    last_read_msg_id: std::collections::HashMap<String, String>,
    /// per chat: is the view parked at the newest message (then it follows new/growing content)
    stuck_bottom: std::collections::HashMap<String, bool>,
    /// message hits for the text in the search box: (thread, text, time)
    search_hits: Vec<(String, String, f64)>,
    search_for: String,
    /// files waiting for "send?" confirmation: (path, temporary copy to delete afterwards)
    outgoing: Vec<(PathBuf, bool)>,
    paste_down: bool,
    tray: Option<tray_icon::TrayIcon>,
    hwnd: Option<isize>,
    /// closing the window keeps the program running in the tray
    tray_on: bool,
    sidebar_w: f32,
    unread_only: bool,
    keep_in_unread: Option<String>,
}

fn now_ms() -> u128 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0)
}

/// Local time zone offset, looked up at most once a minute (asking Windows for it is slow and a
/// chat with hundreds of messages needs it several times per message on every frame).
fn local_offset() -> chrono::FixedOffset {
    use std::sync::Mutex;
    static CACHE: Mutex<Option<(std::time::Instant, i32)>> = Mutex::new(None);
    let mut g = CACHE.lock().unwrap();
    let secs = match *g {
        Some((at, s)) if at.elapsed() < Duration::from_secs(60) => s,
        _ => {
            let s = chrono::Local::now().offset().local_minus_utc();
            *g = Some((std::time::Instant::now(), s));
            s
        }
    };
    chrono::FixedOffset::east_opt(secs).unwrap_or_else(|| chrono::FixedOffset::east_opt(0).unwrap())
}

fn local(ts: f64) -> Option<chrono::DateTime<chrono::FixedOffset>> {
    local_offset().timestamp_millis_opt(ts as i64).single()
}

fn hhmm(ts: f64) -> String {
    local(ts).map(|t| t.format("%H:%M").to_string()).unwrap_or_default()
}

/// Chat-list time: today HH:MM, within a week the weekday, otherwise dd/mm.
fn list_time(ts: f64) -> String {
    let Some(t) = local(ts) else { return String::new() };
    let now = chrono::Local::now();
    let days = (now.date_naive() - t.date_naive()).num_days();
    if days <= 0 {
        t.format("%H:%M").to_string()
    } else if days < 7 {
        ["CN", "T2", "T3", "T4", "T5", "T6", "T7"][t.weekday().num_days_from_sunday() as usize].to_string()
    } else {
        t.format("%d/%m").to_string()
    }
}

fn day_label(ts: f64) -> String {
    let Some(t) = local(ts) else { return String::new() };
    let days = (chrono::Local::now().date_naive() - t.date_naive()).num_days();
    match days {
        0 => "Hôm nay".into(),
        1 => "Hôm qua".into(),
        _ => t.format("%d/%m/%Y").to_string(),
    }
}

fn media_uri(r: &str) -> String {
    crate::cache::image_uri(r)
}

/// One toast for a batch of (thread, chat name, text): a single message as it is, several in the
/// same chat as a count plus the latest text, several chats as a short summary.
fn summarize_notifications(batch: &[(String, String, String)]) -> (String, String, String) {
    let mut threads: Vec<(&str, &str)> = Vec::new(); // (thread, name) in order of first message
    for (th, name, _) in batch {
        if !threads.iter().any(|(t, _)| t == th) {
            threads.push((th.as_str(), name.as_str()));
        }
    }
    let (last_th, last_name, last_text) = batch.last().map(|(a, b, c)| (a.as_str(), b.as_str(), c.as_str())).unwrap_or(("", "", ""));
    if batch.len() == 1 {
        return (last_name.to_string(), last_text.to_string(), last_th.to_string());
    }
    if threads.len() == 1 {
        return (last_name.to_string(), format!("{} tin nhắn mới · {}", batch.len(), last_text), last_th.to_string());
    }
    let names: Vec<&str> = threads.iter().take(3).map(|(_, n)| *n).collect();
    let more = threads.len().saturating_sub(3);
    let from = if more > 0 { format!("{} và {} cuộc trò chuyện khác", names.join(", "), more) } else { names.join(", ") };
    ("Russenger".to_string(), format!("{} tin nhắn mới từ {} cuộc trò chuyện: {}", batch.len(), threads.len(), from), "summary".to_string())
}

/// The part of a message around the first match of `q`, for a search result line.
fn around(text: &str, q: &str) -> String {
    let lt = text.to_lowercase();
    let Some(i) = lt.find(&q.to_lowercase()) else { return text.to_string() };
    let chars: Vec<char> = text.chars().collect();
    let start = lt[..i].chars().count().saturating_sub(12).min(chars.len());
    let s: String = chars[start..].iter().collect();
    if start > 0 { format!("…{s}") } else { s }
}

fn fmt_bytes(b: u64) -> String {
    if b >= 1 << 30 {
        format!("{:.1} GB", b as f64 / (1u64 << 30) as f64)
    } else {
        format!("{:.0} MB", b as f64 / (1u64 << 20) as f64)
    }
}

/// Truncate `text` with an ellipsis so it fits `max_w` (binary search on char count).
fn fit_text(ui: &egui::Ui, text: &str, font: &FontId, max_w: f32) -> String {
    let width = |s: &str| ui.painter().layout_no_wrap(s.to_string(), font.clone(), Color32::WHITE).size().x;
    let line: String = text.lines().next().unwrap_or("").to_string();
    if width(&line) <= max_w {
        return line;
    }
    let chars: Vec<char> = line.chars().collect();
    let (mut lo, mut hi) = (0usize, chars.len());
    while lo < hi {
        let mid = (lo + hi + 1) / 2;
        let s: String = chars[..mid].iter().collect::<String>() + "…";
        if width(&s) <= max_w {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    chars[..lo].iter().collect::<String>() + "…"
}

fn avatar(ui: &mut egui::Ui, url: Option<&str>, name: &str, key: &str, size: f32) {
    let r = CornerRadius::same((size / 2.0) as u8);
    match url {
        Some(u) if !u.is_empty() => {
            ui.add(egui::Image::new(crate::cache::image_uri(u)).fit_to_exact_size(Vec2::splat(size)).corner_radius(r));
        }
        _ => {
            let (rect, _) = ui.allocate_exact_size(Vec2::splat(size), Sense::hover());
            ui.painter().circle_filled(rect.center(), size / 2.0, theme::avatar_color(key));
            let ch = name.chars().find(|c| c.is_alphanumeric()).map(|c| c.to_uppercase().to_string()).unwrap_or_else(|| "?".into());
            ui.painter().text(rect.center(), Align2::CENTER_CENTER, ch, theme::semibold(size * 0.42), Color32::WHITE);
        }
    }
}

/// `russenger --demo`: fake data for checking the layout (no backend, no database).
fn demo_store() -> Store {
    let now = now_ms() as f64;
    let mut st = Store::default();
    st.self_id = "me".into();
    st.ready = true;
    st.history_state = "none".into();
    let names = [
        ("100", "Minh Anh", "Ok, mai gặp nhé 👍", 0.0, true, false),
        ("101", "Nhóm Gia đình", "Mẹ: Cuối tuần về ăn cơm", 3_600_000.0, true, true),
        ("102", "Team Dev", "Build xong rồi, anh check giúp em", 7_200_000.0, false, false),
        ("103", "Trang", "Đã gửi một ảnh", 86_400_000.0 * 1.2, false, false),
        ("104", "Hùng Nguyễn", "Hẹn cuối tuần cafe nhé, nhớ mang theo laptop để cài lại máy giúp mình", 86_400_000.0 * 3.0, false, false),
        ("105", "Lan", "Cảm ơn bạn nhiều!", 86_400_000.0 * 12.0, false, false),
    ];
    for (id, name, snip, ago, unread, muted) in names {
        st.threads.insert(
            id.into(),
            Thread { id: id.into(), name: name.into(), snippet: snip.into(), last_activity: now - ago, unread, muted, contact: if id == "101" || id == "102" { None } else { Some(id.into()) }, ..Default::default() },
        );
        st.contacts.insert(id.into(), name.into());
    }
    // tabs: marketplace, requests, archive, two communities with channels
    let mk = |id: &str, name: &str, snip: &str, ago: f64, unread: bool, ttype: i64, parent: &str, folder: &str| Thread {
        id: id.into(),
        name: name.into(),
        snippet: snip.into(),
        last_activity: now - ago,
        unread,
        ttype,
        parent: parent.into(),
        folder: folder.into(),
        ..Default::default()
    };
    for t in [
        mk("m1", "Bán Steam Deck OLED 1TB", "Còn hàng không bạn?", 5_400_000.0, true, 5, "", "inbox"),
        mk("r1", "Người lạ", "Chào bạn, mình muốn hỏi chút", 7_200_000.0, true, 1, "", "requests"),
        mk("a1", "Nhóm cũ", "Ok hết rồi nhé", 86_400_000.0 * 40.0, false, 2, "", "archived"),
        mk("c1a", "Thông báo chung", "Nội quy cộng đồng", 600_000.0, true, 18, "c1", "inbox"),
        mk("c1b", "Hỏi đáp phần cứng", "Có ai biết cách thay keo tản nhiệt?", 1_200_000.0, true, 18, "c1", "inbox"),
        mk("c2a", "Mua bán giao lưu", "Bán máy chơi game cũ", 9_000_000.0, false, 18, "c2", "inbox"),
    ] {
        st.threads.insert(t.id.clone(), t);
    }
    st.communities.insert("c1".into(), crate::model::Community { name: "Hội PC Handheld Việt Nam".into(), members: 457, ..Default::default() });
    st.communities.insert("c2".into(), crate::model::Community { name: "Group Không Người".into(), members: 180, ..Default::default() });
    st.contacts.insert("200".into(), "Mẹ".into());
    st.contacts.insert("201".into(), "Chị Hai".into());
    let m = |id: &str, sender: &str, text: &str, ago: f64| Message { id: id.into(), sender: sender.into(), text: text.into(), ts: now - ago, ..Default::default() };
    // emoji checks: mixed text, emoji only (big, no bubble) and a sticker (no bubble)
    let emoji_mix = m("e1", "100", "Hôm nay vui quá 😄🎉 đi chơi nhé 👍", 10.0 * 60_000.0);
    let emoji_big = m("e2", "me", "😂🔥", 9.0 * 60_000.0);
    let mut sticker = m("e3", "100", "", 8.0 * 60_000.0);
    sticker.media_kind = Some("sticker".into());
    sticker.media_ref = Some("https://www.gstatic.com/webp/gallery/4.webp".into());
    let min = 60_000.0;
    let mut a = vec![
        m("a1", "100", "Chào bạn, hôm nay rảnh không?", 86_400_000.0 + 40.0 * min),
        m("a2", "me", "Rảnh nè, có chuyện gì thế?", 86_400_000.0 + 38.0 * min),
        m("a3", "100", "Mình muốn hỏi về cái app Russenger bạn đang làm", 40.0 * min),
        m("a4", "100", "Giao diện giờ trông giống Telegram luôn đó", 39.0 * min),
        m("a5", "100", "Đẹp ghê 😄", 38.5 * min),
        m("a6", "me", "Cảm ơn! Vẫn đang chỉnh thêm vài chỗ", 20.0 * min),
        m("a7", "me", "Bong bóng gộp theo người gửi, có chip ngày ở trên", 19.0 * min),
        m("a8", "100", "Ok, mai gặp nhé 👍", 2.0 * min),
    ];
    a[7].reply_text = Some("Giao diện giờ trông giống Telegram luôn đó".into());
    // a shared-link card
    let mut card = m("a12", "100", "https://www.example.com/bai-viet", 120_000.0);
    card.link = Some(crate::model::LinkPreview {
        title: "Bài viết mẫu về Russenger".into(),
        description: "Mô tả ngắn của trang được chia sẻ, hiển thị bên dưới tiêu đề.".into(),
        source: "example.com".into(),
        url: "https://www.example.com/bai-viet".into(),
        image: "https://www.gstatic.com/webp/gallery/2.jpg".into(),
        ..Default::default()
    });
    // image checks: a remote URL and a local file
    let mut img_url = m("a9", "100", "[Photo]", 90_000.0);
    img_url.text = String::new();
    img_url.media_kind = Some("image".into());
    img_url.media_ref = Some("https://www.gstatic.com/webp/gallery/1.jpg".into());
    let mut img_file = m("a10", "me", "", 60_000.0);
    img_file.media_kind = Some("image".into());
    img_file.media_ref = Some(format!("{}\\Relay\\icon.png", std::env::var("LOCALAPPDATA").unwrap_or_default()));
    let mut img_file2 = m("a11", "100", "", 30_000.0);
    img_file2.media_kind = Some("image".into());
    img_file2.media_ref = img_file.media_ref.clone();
    a.push(emoji_mix);
    a.push(emoji_big);
    a.push(sticker);
    a[1].reactions = vec![("100".into(), "❤️".into())];
    if !std::env::args().any(|x| x == "--demo-emoji") {
        a.push(img_url);
        a.push(img_file);
        a.push(img_file2);
        a.push(card);
    }
    a[5].reactions = vec![("100".into(), "❤".into())];
    a[2].reactions = vec![("me".into(), "👍".into()), ("100".into(), "👍".into())];
    st.messages.insert("100".into(), a);
    st.messages.insert(
        "101".into(),
        vec![m("b1", "200", "Cuối tuần về ăn cơm nhé con", 90.0 * min), m("b2", "201", "Em nhớ mang theo cháu", 88.0 * min), m("b3", "201", "Chị làm món cá kho", 87.0 * min), m("b4", "me", "Dạ con về", 60.0 * min)],
    );
    st
}

fn rail_item_bg(ui: &egui::Ui, rect: egui::Rect, resp: &egui::Response, selected: bool, p: &Palette) {
    let t = ui.ctx().animate_bool_with_time(resp.id, resp.hovered(), 0.1);
    let fill = if selected { p.accent.gamma_multiply(0.18) } else { p.row_hover.gamma_multiply(t) };
    ui.painter().rect_filled(rect.shrink2(Vec2::new(6.0, 1.0)), CornerRadius::same(10), fill);
}

/// Small unread counter at the top-right corner of a rail item.
fn rail_badge(ui: &egui::Ui, rect: egui::Rect, n: usize, p: &Palette) {
    if n == 0 {
        return;
    }
    let text = if n > 99 { "99+".to_string() } else { n.to_string() };
    let g = ui.painter().layout_no_wrap(text, theme::semibold(10.0), Color32::WHITE);
    let size = Vec2::new((g.size().x + 10.0).max(18.0), 17.0);
    let r = egui::Rect::from_min_size(egui::pos2(rect.right() - size.x - 6.0, rect.top() + 3.0), size);
    ui.painter().rect_filled(r, CornerRadius::same(9), p.unread);
    ui.painter().galley(r.center() - g.size() / 2.0, g, Color32::WHITE);
}

/// Centered pill that hugs its text (date separators, system messages).
fn chip(ui: &mut egui::Ui, text: &str, fill: Color32) {
    let g = ui.painter().layout(text.to_string(), theme::body(12.0), Color32::WHITE, (ui.available_width() - 60.0).max(80.0));
    let size = g.size() + Vec2::new(20.0, 6.0);
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), size.y), Sense::hover());
    let r = egui::Rect::from_center_size(rect.center(), size);
    ui.painter().rect_filled(r, CornerRadius::same(12), fill);
    ui.painter().galley(r.min + Vec2::new(10.0, 3.0), g, Color32::WHITE);
}

struct RowMeta {
    first: bool,
    last: bool,
    new_day: bool,
}

/// Group consecutive messages: same sender, same day, < 5 min apart, not system.
fn row_metas(msgs: &[Message]) -> Vec<RowMeta> {
    let same = |a: &Message, b: &Message| {
        !a.system
            && !b.system
            && a.sender == b.sender
            && (b.ts - a.ts).abs() < 300_000.0
            && local(a.ts).map(|t| t.date_naive()) == local(b.ts).map(|t| t.date_naive())
    };
    let mut v = Vec::with_capacity(msgs.len());
    for i in 0..msgs.len() {
        let grouped_prev = i > 0 && same(&msgs[i - 1], &msgs[i]);
        let grouped_next = i + 1 < msgs.len() && same(&msgs[i], &msgs[i + 1]);
        let new_day = i == 0 || local(msgs[i - 1].ts).map(|t| t.date_naive()) != local(msgs[i].ts).map(|t| t.date_naive());
        v.push(RowMeta { first: !grouped_prev, last: !grouped_next, new_day });
    }
    v
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let db = Db::open().ok();
        let dark = db.as_ref().and_then(|d| d.get_setting("theme")).map(|t| t != "light").unwrap_or(true);
        theme::apply(&cc.egui_ctx, dark);
        let cache_mb: u64 = db.as_ref().and_then(|d| d.get_setting("cache_mb")).and_then(|v| v.parse().ok()).unwrap_or(1024);
        crate::cache::init(&cc.egui_ctx, cache_mb);
        let db_tray = db.as_ref().and_then(|d| d.get_setting("tray")).map(|v| v != "off").unwrap_or(true);
        let sidebar_w: f32 = db
            .as_ref()
            .and_then(|d| d.get_setting("sidebar_w"))
            .and_then(|v| v.parse::<f32>().ok())
            .unwrap_or(330.0)
            .clamp(260.0, 520.0);

        let demo = std::env::args().any(|a| a == "--demo");
        let (pick_tx, pick_rx) = channel();

        let mut app = App {
            ctx: cc.egui_ctx.clone(),
            store: if demo { demo_store() } else { Store::with_db(db) },
            backend: None,
            screen: Screen::Login,
            cookies: if demo { None } else { vault::load_cookies() },
            paste: String::new(),
            login_status: String::new(),
            login_rx: None,
            selected: None,
            input: String::new(),
            reply: None,
            editing: None,
            search: String::new(),
            tag: 0,
            last_toast: Instant::now() - Duration::from_secs(60),
            reconnect_at: None,
            pick_tx,
            pick_rx,
            requested: HashSet::new(),
            focus_input: false,
            pin: String::new(),
            history_asked: false,
            more_threads_asked: false,
            backend_started: Instant::now(),
            dark,
            scroll_bottom: false,
            seen_len: 0,
            repaired: HashSet::new(),
            viewer: None,
            tab: Tab::Chats,
            emoji_open: false,
            sticker_open: false,
            retry: 0,
            last_title: String::new(),
            settings_open: false,
            cache_mb,
            cache_used: 0,
            cache_checked: Instant::now(),
            autostart: notify::autostart_enabled(),
            sent_texts: Default::default(),
            pending_since: None,
            row_heights: Default::default(),
            last_read_msg_id: Default::default(),
            stuck_bottom: Default::default(),
            search_hits: Vec::new(),
            search_for: String::new(),
            outgoing: Vec::new(),
            paste_down: false,
            tray: None,
            hwnd: None,
            tray_on: db_tray,
            sidebar_w,
            unread_only: false,
            keep_in_unread: None,
        };
        {
            // a slow heartbeat so messages are still handled (and toasts shown) while the window is
            // hidden in the tray
            let c = cc.egui_ctx.clone();
            std::thread::spawn(move || loop {
                std::thread::sleep(Duration::from_millis(1000));
                if crate::tray::PARKED.load(std::sync::atomic::Ordering::Relaxed) {
                    c.request_repaint();
                }
            });
        }
        if !demo {
            notify::watch_open_requests(cc.egui_ctx.clone());
        }
        {
            use raw_window_handle::{HasWindowHandle, RawWindowHandle};
            app.hwnd = cc.window_handle().ok().and_then(|h| match h.as_raw() {
                RawWindowHandle::Win32(w) => Some(w.hwnd.get()),
                _ => None,
            });
            if let Some(h) = app.hwnd {
                crate::tray::set_window_icons(h);
            }
            if let (Some(h), false) = (app.hwnd, demo) {
                app.tray = crate::tray::create(h);
            }
        }
        if demo {
            app.screen = Screen::Main;
            app.selected = Some("100".into());
            if std::env::args().any(|a| a == "--demo-community") {
                app.tab = Tab::Community("c1".into());
                app.selected = None;
            }
            if std::env::args().any(|a| a == "--demo-viewer") {
                app.handle(Action::ViewImage { thread: "100".into(), id: "a10".into() });
            }
        } else if app.cookies.is_some() {
            app.screen = Screen::Main;
            app.start_backend();
        }
        app
    }

    fn start_backend(&mut self) {
        let Some(cookies) = self.cookies.clone() else { return };
        self.backend = None; // drop kills the old helper
        self.store.ready = false;
        self.store.exited = false;
        self.store.need_login = false;
        self.store.last_error = None;
        self.reconnect_at = None;
        self.history_asked = false;
        self.more_threads_asked = false;
        self.backend_started = Instant::now();
        self.store.history_state.clear();
        self.last_read_msg_id.clear();
        let c = self.ctx.clone();
        match Backend::spawn(move || c.request_repaint()) {
            Ok(mut b) => {
                b.login(&cookies);
                self.backend = Some(b);
            }
            Err(e) => self.store.last_error = Some(format!("Không chạy được relay-helper.exe: {e}")),
        }
    }

    /// Ask for older chats, starting after the oldest regular chat stored here.
    fn ask_more_threads(&mut self) {
        let oldest = self
            .store
            .threads
            .values()
            .filter(|t| !t.id.starts_with("e:") && tab_of(t) == Some(Tab::Chats) && t.last_activity > 0.0)
            .min_by(|a, b| a.last_activity.total_cmp(&b.last_activity))
            .map(|t| (t.id.clone(), t.last_activity as i64));
        let mut cmd = json!({"cmd": "loadMoreThreads"});
        if let Some((id, ts)) = oldest {
            if let Ok(k) = id.parse::<i64>() {
                cmd["refKey"] = json!(k);
                cmd["refTs"] = json!(ts);
            }
        }
        self.send_cmd(cmd);
    }

    fn send_cmd(&mut self, v: serde_json::Value) {
        if let Some(b) = self.backend.as_mut() {
            b.send(v);
        }
    }

    fn spawn_login_window(&mut self) {
        let exe = std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.join("russenger-login.exe")));
        let Some(exe) = exe.filter(|p| p.exists()) else {
            self.login_status = "Không tìm thấy russenger-login.exe cạnh ứng dụng.".into();
            return;
        };
        let (tx, rx) = channel();
        let ctx = self.ctx.clone();
        std::thread::spawn(move || {
            let out = Command::new(exe).stdin(Stdio::null()).stderr(Stdio::null()).output();
            let text = out.ok().map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).unwrap_or_default();
            let _ = tx.send(text);
            ctx.request_repaint();
        });
        self.login_rx = Some(rx);
        self.login_status = "Đang chờ bạn đăng nhập trong cửa sổ Facebook…".into();
    }

    fn finish_login(&mut self, cookies: String) {
        if !(cookies.contains("c_user=") && cookies.contains("xs=")) {
            self.login_status = "Cookie không hợp lệ: cần có c_user và xs (định dạng: tên=giá trị; tên=giá trị).".into();
            return;
        }
        // a different account must not inherit the previous one's chats and encryption keys
        let account = cookies.split(';').filter_map(|c| c.trim().strip_prefix("c_user=")).next().unwrap_or("").to_string();
        let saved = self.store.db.as_ref().and_then(|d| d.get_setting("account"));
        if saved.as_deref().map(|a| a != account).unwrap_or(false) {
            self.wipe_account_data();
        }
        if let Some(db) = &self.store.db {
            db.set_setting("account", &account);
        }
        vault::save_cookies(&cookies);
        self.cookies = Some(cookies);
        self.paste.clear();
        self.login_status.clear();
        self.screen = Screen::Main;
        self.start_backend();
    }

    /// Delete everything stored for the previous account (messages, keys, pictures).
    fn wipe_account_data(&mut self) {
        self.backend = None; // stops the helper so its files can be removed
        self.store = Store::with_db(None); // closes relay.db
        let d = vault::data_dir();
        for f in ["relay.db", "relay.db-wal", "relay.db-shm", "history-device.json"] {
            let _ = std::fs::remove_file(d.join(f));
        }
        if let Some(a) = std::env::var_os("APPDATA") {
            let r = PathBuf::from(a).join("Relay");
            for f in ["wadevice.db", "wadevice.db-wal", "wadevice.db-shm"] {
                let _ = std::fs::remove_file(r.join(f));
            }
        }
        cache::clear();
        self.store = Store::with_db(Db::open().ok());
        self.selected = None;
    }

    fn logout(&mut self) {
        // the sign-in window's browser profile keeps Facebook's own cookies; drop them so the next
        // sign-in really asks which account
        for dir in ["webview", "webview-call"] {
            let _ = std::fs::remove_dir_all(vault::data_dir().join(dir));
        }
        vault::clear_cookies();
        self.cookies = None;
        self.backend = None;
        self.selected = None;
        self.screen = Screen::Login;
        self.login_status = "Đã đăng xuất. Đăng nhập lại cùng tài khoản thì tin nhắn đã lưu vẫn còn; đổi tài khoản sẽ xóa dữ liệu cũ.".into();
    }

    fn pump_events(&mut self) {
        let mut evs = Vec::new();
        if let Some(b) = self.backend.as_ref() {
            while let Ok(ev) = b.events.try_recv() {
                evs.push(ev);
            }
        }
        let mut became_ready = false;
        if !evs.is_empty() {
            became_ready = evs.iter().any(|ev| ev.get("type").and_then(|v| v.as_str()) == Some("ready"));
            if let Some(db) = &self.store.db {
                db.begin();
            }
            for ev in &evs {
                self.store.apply(ev);
            }
            if let Some(db) = &self.store.db {
                db.commit();
            }
            if let Some(sel) = self.selected.clone() {
                let canon = self.store.canonical_thread(&sel);
                if canon != sel {
                    self.store.ensure_loaded(&canon);
                    self.selected = Some(canon);
                }
            }
        }
        if became_ready {
            if let Some(th) = self.selected.clone() {
                self.request_refresh(&th);
            }
        }
        if self.store.need_login {
            vault::clear_cookies();
            self.cookies = None;
            self.backend = None;
            self.screen = Screen::Login;
            self.login_status = "Phiên đăng nhập đã hết hạn, vui lòng đăng nhập lại.".into();
            self.store.need_login = false;
            self.store.exited = false;
        } else if self.store.exited && self.reconnect_at.is_none() && self.cookies.is_some() {
            // the helper is one-shot: respawn it, waiting longer after every failed attempt
            let wait = if self.store.ready { 3 } else { (10u64 << self.retry.min(5)).min(300) };
            if !self.store.ready {
                self.retry += 1;
            }
            self.reconnect_at = Some(Instant::now() + Duration::from_secs(wait));
        }
        if self.store.ready {
            self.retry = 0;
        }
        if let Some(at) = self.reconnect_at {
            if Instant::now() >= at {
                self.start_backend();
            } else {
                self.ctx.request_repaint_after(at - Instant::now());
            }
        }
    }

    fn pump_login(&mut self) {
        if let Some(rx) = &self.login_rx {
            if let Ok(text) = rx.try_recv() {
                self.login_rx = None;
                if text.is_empty() {
                    self.login_status = "Cửa sổ đăng nhập đã đóng mà chưa đăng nhập.".into();
                } else {
                    self.finish_login(text);
                }
            }
        }
        while let Ok((thread, path)) = self.pick_rx.try_recv() {
            self.send_cmd(json!({"cmd": "sendMedia", "thread": thread, "path": path.to_string_lossy(), "caption": ""}));
        }
    }

    fn request_refresh(&mut self, thread: &str) {
        let since_ts = if self.store.ready {
            self.store.pre_sync_max_ts.remove(thread)
        } else {
            self.store.pre_sync_max_ts.get(thread).copied()
        }
        .unwrap_or(0.0);
        if let Some(b) = &mut self.backend {
            b.refresh_thread(thread, since_ts as i64);
        }
    }

    fn latest_incoming_id(&self, thread: &str) -> String {
        self.store
            .messages
            .get(thread)
            .and_then(|l| {
                l.iter()
                    .rev()
                    .find(|m| !m.pending && !m.system && !self.store.self_id.is_empty() && m.sender != self.store.self_id)
                    .map(|m| m.id.split('#').next().unwrap_or(&m.id).to_string())
            })
            .unwrap_or_default()
    }

    fn mark_read(&mut self, thread: &str) {
        let prev_synced = self.store.threads.get(thread).map(|t| t.synced_read_up_to).unwrap_or(0.0);
        let thread_ts = self.store.threads.get(thread).map(|t| t.last_activity).unwrap_or(0.0);
        let stored_read = self.store.threads.get(thread).map(|t| t.read_up_to).unwrap_or(0.0);
        let mut max_msg_ts = 0.0f64;
        let mut last_in: Option<(String, String)> = None;
        let mut unread_ids: Vec<String> = Vec::new();
        if let Some(list) = self.store.messages.get(thread) {
            for m in list.iter().filter(|m| !m.pending) {
                max_msg_ts = max_msg_ts.max(m.ts);
                if !self.store.self_id.is_empty() && m.sender != self.store.self_id && !m.system {
                    let base_id = m.id.split('#').next().unwrap_or(&m.id).to_string();
                    if m.ts > prev_synced - 1000.0 && !unread_ids.contains(&base_id) {
                        unread_ids.push(base_id.clone());
                    }
                    last_in = Some((base_id, m.sender.clone()));
                }
            }
        } else if let Some(db) = &self.store.db {
            if let Some((id, sender)) = db.latest_incoming(thread, &self.store.self_id) {
                let base_id = id.split('#').next().unwrap_or(&id).to_string();
                unread_ids.push(base_id.clone());
                last_in = Some((base_id, sender));
            }
        }
        if unread_ids.len() > 25 {
            let drop_n = unread_ids.len() - 25;
            unread_ids.drain(0..drop_n);
        }
        let target_ts = thread_ts.max(max_msg_ts).max(stored_read);
        let (last_id, last_sender) = last_in.unwrap_or_default();
        let sent_to_server = self.store.ready && self.backend.is_some() && (target_ts > 0.0 || !last_id.is_empty());
        if sent_to_server {
            self.send_cmd(json!({
                "cmd": "markReadServer",
                "thread": thread,
                "id": last_id,
                "ids": unread_ids,
                "sender": last_sender,
                "ts": target_ts as i64,
            }));
            if !last_id.is_empty() {
                self.last_read_msg_id.insert(thread.to_string(), last_id);
            }
        }
        if let Some(t) = self.store.threads.get_mut(thread) {
            let mut dirty = false;
            if t.unread {
                t.unread = false;
                dirty = true;
            }
            if target_ts > t.read_up_to {
                t.read_up_to = target_ts;
                dirty = true;
            }
            if sent_to_server && target_ts > t.synced_read_up_to {
                t.synced_read_up_to = target_ts;
                dirty = true;
            }
            if dirty {
                if let Some(db) = &self.store.db {
                    db.save_thread(t);
                }
            }
        }
    }

    /// Old attachment messages saved before pictures were tracked have no picture. Ask the
    /// helper to send that stretch of history again (once per thread and session).
    fn repair_media(&mut self, th: &str) {
        if self.repaired.contains(th) || !self.store.ready {
            return;
        }
        let encrypted = th.starts_with("e:");
        if encrypted && self.store.history_state.is_empty() {
            return; // state not known yet; try again later
        }
        let Some(list) = self.store.messages.get(th) else { return };
        let idx = list.iter().rposition(|m| is_placeholder(&m.text) && m.media_ref.is_none() && !m.pending);
        let cmd = idx.and_then(|i| {
            if encrypted {
                (self.store.history_state == "restored").then(|| json!({"cmd": "loadEncryptedHistory", "thread": th, "before": list[i].ts as i64 + 1}))
            } else {
                // history is fetched "older than" a reference, so anchor on the next newer message
                list.get(i + 1).map(|n| json!({"cmd": "loadEarlier", "thread": th, "oldestId": n.id, "oldestTs": n.ts, "fromMe": n.sender == self.store.self_id}))
            }
        });
        self.repaired.insert(th.to_string());
        if let Some(c) = cmd {
            self.send_cmd(c);
        }
    }

    fn do_send(&mut self) {
        let text = self.input.trim().to_string();
        let Some(th) = self.selected.clone() else { return };
        if text.is_empty() {
            return;
        }
        if let Some(id) = self.editing.take() {
            self.send_cmd(json!({"cmd": "edit", "thread": th, "id": id, "text": text}));
            self.input.clear();
            return;
        }
        self.tag += 1;
        let tag = format!("t{}-{}", now_ms(), self.tag);
        let mut cmd = json!({"cmd": "send", "thread": th, "text": text, "clientTag": tag});
        if let Some((id, _, sender)) = self.reply.take() {
            cmd["replyId"] = json!(id);
            cmd["replySender"] = json!(sender);
        }
        self.send_cmd(cmd);
        if self.sent_texts.len() > 100 {
            self.sent_texts.clear();
        }
        self.sent_texts.insert(tag.clone(), text.clone());
        if th.starts_with("e:") {
            self.store.add_local_echo(&th, &tag, &text);
        }
        self.input.clear();
        self.scroll_bottom = true;
    }

    fn handle(&mut self, a: Action) {
        match a {
            Action::Select(raw_id) => {
                let id = self.store.canonical_thread(&raw_id);
                if self.unread_only && self.tab == Tab::Chats {
                    self.keep_in_unread = Some(id.clone());
                }
                self.store.ensure_loaded(&id);
                self.seen_len = self.store.messages.get(&id).map(|l| l.len()).unwrap_or(0);
                self.selected = Some(id.clone());
                self.reply = None;
                self.editing = None;
                self.input.clear();
                self.focus_input = true;
                self.request_refresh(&id);
                self.mark_read(&id);
                self.repair_media(&id);
            }
            Action::Send => self.do_send(),
            Action::Attach => {
                if let Some(th) = self.selected.clone() {
                    let tx = self.pick_tx.clone();
                    let ctx = self.ctx.clone();
                    std::thread::spawn(move || {
                        if let Some(p) = rfd::FileDialog::new().set_title("Chọn tệp để gửi").pick_file() {
                            let _ = tx.send((th, p));
                            ctx.request_repaint();
                        }
                    });
                }
            }
            Action::React { thread, id, emoji, from_me } => {
                self.send_cmd(json!({"cmd": "react", "thread": thread, "id": id, "emoji": emoji, "fromMe": from_me}));
            }
            Action::Unsend { thread, id } => self.send_cmd(json!({"cmd": "unsend", "thread": thread, "id": id})),
            Action::StartEdit { id, text } => {
                self.editing = Some(id);
                self.reply = None;
                self.input = text;
                self.focus_input = true;
            }
            Action::StartReply { id, text, sender } => {
                self.reply = Some((id, text, sender));
                self.editing = None;
                self.focus_input = true;
            }
            Action::CancelCompose => {
                if self.editing.take().is_some() {
                    self.input.clear();
                }
                self.reply = None;
            }
            Action::Mute { thread, muted } => {
                self.send_cmd(json!({"cmd": "mute", "thread": thread, "muted": muted}));
                if let Some(t) = self.store.threads.get_mut(&thread) {
                    t.muted = muted;
                    if let Some(db) = &self.store.db {
                        db.save_thread(t);
                    }
                }
            }
            Action::DeleteThread(th) => {
                self.send_cmd(json!({"cmd": "deleteThread", "thread": th}));
                self.store.threads.remove(&th);
                self.store.messages.remove(&th);
                if let Some(db) = &self.store.db {
                    db.delete_thread(&th);
                }
                if self.selected.as_deref() == Some(&th) {
                    self.selected = None;
                }
            }
            Action::LoadEarlier => {
                if let Some(th) = self.selected.clone() {
                    // stored history first; the server is asked only when that is used up
                    if self.store.load_older_from_db(&th) > 0 {
                        return;
                    }
                    let oldest = self.store.messages.get(&th).and_then(|l| l.iter().find(|m| !m.pending).cloned());
                    if let Some(m) = oldest {
                        let base_id = m.id.split('#').next().unwrap_or(&m.id).to_string();
                        let from_me = m.sender == self.store.self_id;
                        if th.starts_with("e:") && self.store.history_state == "restored" {
                            // encrypted archive: pages are keyed by timestamp (+999ms avoids skipping same-second WA msgs)
                            self.store.history_busy = true;
                            self.send_cmd(json!({"cmd": "loadEncryptedHistory", "thread": th, "before": m.ts as i64 + 999}));
                        } else {
                            self.send_cmd(json!({"cmd": "loadEarlier", "thread": th, "oldestId": base_id, "oldestTs": m.ts as i64, "fromMe": from_me}));
                        }
                    } else if th.starts_with("e:") && self.store.history_state == "restored" {
                        self.store.history_busy = true;
                        self.send_cmd(json!({"cmd": "loadEncryptedHistory", "thread": th, "before": 0}));
                    } else {
                        self.request_refresh(&th);
                    }
                }
            }
            Action::SendSticker { thread, id } => {
                self.send_cmd(json!({"cmd": "sendSticker", "thread": thread, "stickerId": id}));
                self.scroll_bottom = true;
            }
            Action::MoreThreads => self.ask_more_threads(),
            Action::OpenUrl(t) => notify::open_url(&t),
            Action::SaveFile(src) => {
                // never run a received file: offer to save it instead
                let name = std::path::Path::new(&src).file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "tệp".into());
                std::thread::spawn(move || {
                    if let Some(dest) = rfd::FileDialog::new().set_title("Lưu tệp").set_file_name(&name).save_file() {
                        let _ = cache::save_to(&src, &dest);
                    }
                });
            }
            Action::Settings => {
                self.settings_open = true;
                self.cache_used = cache::usage();
                self.cache_checked = Instant::now();
            }
            Action::Play(t) => notify::play(&cache::video_target(&t)),
            Action::Call { thread, video } => {
                let id = thread.trim_start_matches("e:");
                let url = format!("https://www.facebook.com/messages/t/{id}");
                let ok = self.cookies.as_deref().map(|c| notify::call(&url, video, c)).unwrap_or(false);
                if !ok {
                    self.store.last_error = Some("Không mở được cửa sổ gọi (thiếu russenger-call.exe hoặc chưa đăng nhập).".into());
                }
            }
            Action::ViewImage { thread, id } => {
                let items: Vec<(String, String)> = self
                    .store
                    .messages
                    .get(&thread)
                    .map(|l| {
                        l.iter()
                            .filter(|m| matches!(m.media_kind.as_deref(), Some("photo" | "image" | "gif")))
                            .filter_map(|m| m.media_ref.as_ref().map(|r| (m.id.clone(), media_uri(r))))
                            .collect()
                    })
                    .unwrap_or_default();
                if let Some(index) = items.iter().position(|(mid, _)| *mid == id) {
                    self.viewer = Some(Viewer { items, index, zoom: 1.0, offset: Vec2::ZERO });
                }
            }
            Action::Copy(t) => self.ctx.copy_text(t),
            Action::Logout => self.logout(),
            Action::Retry => self.start_backend(),
            Action::RestoreHistory => {
                let pin = std::mem::take(&mut self.pin);
                self.store.history_busy = true;
                self.store.history_error = None;
                self.send_cmd(json!({"cmd": "restoreHistory", "pin": pin}));
            }
            Action::ToggleTheme => {
                self.dark = !self.dark;
                theme::apply(&self.ctx, self.dark);
                if let Some(db) = &self.store.db {
                    db.set_setting("theme", if self.dark { "dark" } else { "light" });
                }
            }
            Action::ScrollBottom => self.scroll_bottom = true,
        }
    }

    // ---------- UI ----------

    /// A picture on the clipboard (and no text) becomes a file waiting for "send?".
    fn paste_clipboard_image(&mut self) {
        let Ok(mut cb) = arboard::Clipboard::new() else { return };
        if cb.get_text().map(|t| !t.is_empty()).unwrap_or(false) {
            return; // text paste is handled by the text box
        }
        let Ok(img) = cb.get_image() else { return };
        let Some(rgba) = image::RgbaImage::from_raw(img.width as u32, img.height as u32, img.bytes.into_owned()) else { return };
        let path = std::env::temp_dir().join(format!("russenger-paste-{}.png", now_ms()));
        if rgba.save(&path).is_ok() {
            self.outgoing.push((path, true));
        }
    }

    fn draw_outgoing(&mut self, ctx: &egui::Context) {
        let (mut send, mut cancel) = (false, false);
        egui::Window::new("Gửi tệp").collapsible(false).resizable(false).anchor(Align2::CENTER_CENTER, Vec2::ZERO).show(ctx, |ui| {
            for (path, _) in &self.outgoing {
                let name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
                let ext = path.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
                if matches!(ext.as_str(), "png" | "jpg" | "jpeg" | "gif" | "webp") {
                    ui.add(egui::Image::new(format!("file://{}", path.to_string_lossy().replace('\\', "/"))).max_size(Vec2::new(340.0, 240.0)).corner_radius(CornerRadius::same(8)));
                }
                ui.label(name);
            }
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if ui.button("Gửi").clicked() {
                    send = true;
                }
                if ui.button("Hủy").clicked() {
                    cancel = true;
                }
            });
        });
        if !(send || cancel) {
            return;
        }
        let files = std::mem::take(&mut self.outgoing);
        let th = self.selected.clone();
        for (path, temp) in files {
            if send {
                if let Some(th) = &th {
                    self.send_cmd(json!({"cmd": "sendMedia", "thread": th, "path": path.to_string_lossy(), "caption": ""}));
                }
            }
            if temp {
                // the helper reads the file while sending; give it time before removing the copy
                std::thread::spawn(move || {
                    std::thread::sleep(Duration::from_secs(if send { 180 } else { 0 }));
                    let _ = std::fs::remove_file(path);
                });
            }
        }
    }

    fn draw_settings(&mut self, ctx: &egui::Context) {
        let p = theme::palette(self.dark);
        if self.cache_checked.elapsed() > Duration::from_secs(2) {
            self.cache_used = cache::usage();
            self.cache_checked = Instant::now();
        }
        ctx.request_repaint_after(Duration::from_secs(2));
        let mut open = true;
        let mut limit_changed = false;
        egui::Window::new("Cài đặt").open(&mut open).collapsible(false).resizable(false).anchor(Align2::CENTER_CENTER, Vec2::ZERO).show(ctx, |ui| {
            ui.set_width(360.0);
            ui.label(RichText::new("Bộ nhớ đệm ảnh và video").font(theme::semibold(14.5)));
            ui.label(
                RichText::new("Ảnh và video đã xem được giữ lại trên máy đến mức này; vượt mức thì xóa cái cũ nhất. Cần lại sẽ tải từ Facebook (riêng chat mã hóa đầu cuối thì không tải lại được).")
                    .size(12.0)
                    .color(p.secondary),
            );
            ui.add_space(4.0);
            ui.label(format!("Đang dùng: {}", fmt_bytes(self.cache_used)));
            ui.horizontal(|ui| {
                ui.label("Giới hạn:");
                egui::ComboBox::from_id_salt("cache_limit").selected_text(fmt_bytes(self.cache_mb << 20)).show_ui(ui, |ui| {
                    for mb in [256u64, 512, 1024, 2048, 5120, 10240, 20480] {
                        if ui.selectable_value(&mut self.cache_mb, mb, fmt_bytes(mb << 20)).clicked() {
                            limit_changed = true;
                        }
                    }
                });
                if ui.button("Xóa bộ nhớ đệm").clicked() {
                    cache::clear();
                    self.cache_used = cache::usage();
                }
            });
            ui.separator();
            if ui.checkbox(&mut self.autostart, "Khởi động cùng Windows (chạy ẩn ở khay hệ thống)").changed() {
                notify::set_autostart(self.autostart);
            }
            if ui.checkbox(&mut self.tray_on, "Đóng cửa sổ thì chạy nền ở khay hệ thống").changed() {
                if let Some(db) = &self.store.db {
                    db.set_setting("tray", if self.tray_on { "on" } else { "off" });
                }
            }
        });
        if limit_changed {
            cache::set_limit_mb(self.cache_mb);
            if let Some(db) = &self.store.db {
                db.set_setting("cache_mb", &self.cache_mb.to_string());
            }
            std::thread::spawn(cache::trim);
        }
        if !open {
            self.settings_open = false;
        }
    }

    fn draw_login(&mut self, ctx: &egui::Context) {
        let p = theme::palette(self.dark);
        egui::CentralPanel::default().frame(egui::Frame::new().fill(p.chat_bg)).show(ctx, |ui| {
            ui.vertical_centered(|ui| {
                ui.add_space((ui.available_height() * 0.16).max(30.0));
                egui::Frame::new().fill(p.sidebar).corner_radius(CornerRadius::same(14)).inner_margin(Margin::same(28)).show(ui, |ui| {
                    ui.set_width(340.0);
                    ui.vertical_centered(|ui| {
                        ui.add(egui::Image::from_bytes("bytes://russenger-icon.png", include_bytes!("../../assets/icon-512.png").as_slice()).fit_to_exact_size(Vec2::splat(84.0)));
                        ui.add_space(10.0);
                        ui.label(RichText::new("Russenger").font(theme::semibold(26.0)).color(p.text));
                        ui.label(RichText::new("Messenger gốc, chạy native trên Windows").color(p.secondary).size(13.0));
                        ui.add_space(18.0);
                        let waiting = self.login_rx.is_some();
                        let btn = egui::Button::new(RichText::new("Đăng nhập bằng Facebook").font(theme::semibold(15.0)).color(Color32::WHITE))
                            .fill(p.accent)
                            .corner_radius(CornerRadius::same(10))
                            .min_size(Vec2::new(340.0, 42.0));
                        if ui.add_enabled(!waiting, btn).clicked() {
                            self.spawn_login_window();
                        }
                        if !self.login_status.is_empty() {
                            ui.add_space(8.0);
                            ui.label(RichText::new(&self.login_status).color(p.secondary).size(12.5));
                        }
                        ui.add_space(8.0);
                        egui::CollapsingHeader::new(RichText::new("Nâng cao: dán cookie").color(p.secondary).size(12.5)).show(ui, |ui| {
                            ui.add(egui::TextEdit::multiline(&mut self.paste).desired_rows(3).desired_width(330.0).password(true).hint_text("datr=…; c_user=…; xs=…"));
                            if ui.add_enabled(!self.paste.trim().is_empty(), egui::Button::new("Dùng cookie này")).clicked() {
                                let c = self.paste.trim().to_string();
                                self.finish_login(c);
                            }
                        });
                        ui.add_space(12.0);
                        ui.label(RichText::new("Client không chính thức (giao thức dịch ngược). Tài khoản có thể bị Meta giới hạn.").size(11.0).color(p.secondary));
                    });
                });
            });
        });
    }

    fn status_text(&self) -> String {
        if let Some(e) = &self.store.last_error {
            return format!("⚠ {e}");
        }
        if self.store.ready {
            "● Đã kết nối".into()
        } else if self.backend.is_some() {
            "○ Đang kết nối…".into()
        } else {
            "○ Chưa kết nối".into()
        }
    }

    fn tab_title(&self) -> String {
        match &self.tab {
            Tab::Chats => "Chat".into(),
            Tab::Marketplace => "Marketplace".into(),
            Tab::Requests => "Tin nhắn chờ".into(),
            Tab::Archive => "Lưu trữ".into(),
            Tab::Community(id) => self.store.communities.get(id).map(|c| c.name.clone()).unwrap_or_else(|| "Cộng đồng".into()),
        }
    }

    /// The far-left rail: Chats / Marketplace / Requests / Archive, then one entry per community.
    fn draw_rail(&mut self, ctx: &egui::Context, p: &'static Palette) {
        // unread (non-muted) threads per tab
        let mut unread: std::collections::HashMap<Tab, usize> = std::collections::HashMap::new();
        let mut community_activity: std::collections::HashMap<String, f64> = std::collections::HashMap::new();
        for t in self.store.threads.values() {
            if let Some(tab) = tab_of(t) {
                if let Tab::Community(id) = &tab {
                    let e = community_activity.entry(id.clone()).or_insert(0.0);
                    *e = e.max(t.last_activity);
                }
                if t.unread && !t.muted {
                    *unread.entry(tab).or_insert(0) += 1;
                }
            }
        }
        let mut communities: Vec<(&String, &crate::model::Community)> =
            self.store.communities.iter().filter(|(id, _)| community_activity.contains_key(*id)).collect();
        communities.sort_by(|a, b| community_activity[b.0].total_cmp(&community_activity[a.0]));

        let mut picked: Option<Tab> = None;
        egui::SidePanel::left("rail").exact_width(74.0).resizable(false).frame(egui::Frame::new().fill(p.rail)).show(ctx, |ui| {
            ui.add_space(8.0);
            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 2.0;
                let core: [(Tab, &str, &str); 4] = [
                    (Tab::Chats, "💬", "Chat"),
                    (Tab::Marketplace, "🛍", "Chợ"),
                    (Tab::Requests, "✉", "Chờ"),
                    (Tab::Archive, "🗃", "Lưu trữ"),
                ];
                for (tab, glyph, label) in core {
                    let (rect, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 58.0), Sense::click());
                    let selected = self.tab == tab;
                    rail_item_bg(ui, rect, &resp, selected, p);
                    let col = if selected { p.accent } else { p.secondary };
                    ui.painter().text(egui::pos2(rect.center().x, rect.top() + 23.0), Align2::CENTER_CENTER, glyph, theme::body(22.0), col);
                    ui.painter().text(egui::pos2(rect.center().x, rect.top() + 45.0), Align2::CENTER_CENTER, label, if selected { theme::semibold(11.0) } else { theme::body(11.0) }, col);
                    rail_badge(ui, rect, unread.get(&tab).copied().unwrap_or(0), p);
                    if resp.clicked() {
                        picked = Some(tab);
                    }
                }
                if !communities.is_empty() {
                    ui.add_space(6.0);
                    let r = ui.allocate_space(Vec2::new(ui.available_width(), 1.0)).1;
                    ui.painter().rect_filled(r.shrink2(Vec2::new(14.0, 0.0)), 0.0, p.separator);
                    ui.add_space(4.0);
                    for (id, c) in communities {
                        let tab = Tab::Community(id.clone());
                        let (rect, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 66.0), Sense::click());
                        let selected = self.tab == tab;
                        rail_item_bg(ui, rect, &resp, selected, p);
                        let av = egui::Rect::from_center_size(egui::pos2(rect.center().x, rect.top() + 25.0), Vec2::splat(34.0));
                        let mut child = ui.new_child(egui::UiBuilder::new().max_rect(av));
                        avatar(&mut child, if c.picture.is_empty() { None } else { Some(c.picture.as_str()) }, &c.name, id, 34.0);
                        let short = fit_text(ui, &c.name, &theme::body(10.5), rect.width() - 8.0);
                        ui.painter().text(egui::pos2(rect.center().x, rect.top() + 51.0), Align2::CENTER_CENTER, short, theme::body(10.5), if selected { p.accent } else { p.secondary });
                        rail_badge(ui, rect, unread.get(&tab).copied().unwrap_or(0), p);
                        resp.clone().on_hover_text(&c.name);
                        if resp.clicked() {
                            picked = Some(tab);
                        }
                    }
                }
            });
        });
        if let Some(t) = picked {
            self.tab = t;
        }
    }

    fn draw_thread_list(&mut self, ctx: &egui::Context, p: &'static Palette, actions: &mut Vec<Action>) {
        let panel = egui::SidePanel::left("threads")
            .resizable(false)
            .exact_width(self.sidebar_w)
            .show_separator_line(true)
            .frame(egui::Frame::new().fill(p.sidebar))
            .show(ctx, |ui| {
                // header: menu + search pill (Telegram-style balanced margins)
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    ui.add_space(10.0);
                    ui.menu_button(RichText::new("☰").size(18.0).color(p.secondary), |ui| {
                        let label = if self.dark { "☀ Giao diện sáng" } else { "🌙 Giao diện tối" };
                        if ui.button(label).clicked() {
                            actions.push(Action::ToggleTheme);
                            ui.close_menu();
                        }
                        if ui.button("⚙ Cài đặt").clicked() {
                            actions.push(Action::Settings);
                            ui.close_menu();
                        }
                        if ui.button("Đăng xuất").clicked() {
                            actions.push(Action::Logout);
                            ui.close_menu();
                        }
                    });
                    let pill_w = (ui.available_width() - 34.0).max(80.0);
                    egui::Frame::new().fill(p.input).corner_radius(CornerRadius::same(18)).inner_margin(Margin::symmetric(12, 6)).show(ui, |ui| {
                        ui.add(
                            egui::TextEdit::singleline(&mut self.search)
                                .frame(false)
                                .hint_text(RichText::new("🔍 Tìm kiếm").color(p.secondary))
                                .desired_width(pill_w),
                        );
                    });
                });
                ui.add_space(6.0);

                // tab title / unread filter chips (for Chat tab)
                let chats_unread_n = if self.tab == Tab::Chats {
                    self.store.threads.values().filter(|t| tab_of(t) == Some(Tab::Chats) && t.unread).count()
                } else {
                    0
                };
                ui.horizontal(|ui| {
                    ui.add_space(12.0);
                    if self.tab == Tab::Chats {
                        ui.spacing_mut().item_spacing.x = 6.0;
                        let draw_chip = |ui: &mut egui::Ui, label: &str, active: bool| -> bool {
                            let font = if active { theme::semibold(12.5) } else { theme::body(12.5) };
                            let fg = if active { Color32::WHITE } else { p.secondary };
                            let g = ui.painter().layout_no_wrap(label.to_string(), font, fg);
                            let (rect, resp) = ui.allocate_exact_size(Vec2::new(g.size().x + 20.0, 24.0), Sense::click());
                            if resp.hovered() {
                                ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                            }
                            let fill = if active {
                                p.row_selected
                            } else if resp.hovered() {
                                p.row_hover
                            } else {
                                p.input
                            };
                            ui.painter().rect_filled(rect, CornerRadius::same(12), fill);
                            ui.painter().galley(egui::pos2(rect.center().x - g.size().x * 0.5, rect.center().y - g.size().y * 0.5), g, fg);
                            resp.clicked()
                        };
                        if draw_chip(ui, "Tất cả", !self.unread_only) {
                            self.unread_only = false;
                            self.keep_in_unread = None;
                        }
                        let unread_lbl = if chats_unread_n > 0 {
                            format!("Chưa đọc · {chats_unread_n}")
                        } else {
                            "Chưa đọc".into()
                        };
                        if draw_chip(ui, &unread_lbl, self.unread_only) {
                            self.unread_only = !self.unread_only;
                            self.keep_in_unread = None;
                        }
                    } else {
                        // truncated: a long community name must not force the panel wider than the user set
                        ui.add(egui::Label::new(RichText::new(self.tab_title()).font(theme::semibold(13.5)).color(p.secondary)).truncate());
                    }
                });
                ui.add_space(4.0);

                let q = self.search.to_lowercase();
                let filter_unread = q.is_empty() && self.tab == Tab::Chats && self.unread_only;
                // searching looks through every tab; otherwise only the selected one
                let mut list: Vec<&Thread> = self
                    .store
                    .threads
                    .values()
                    .filter(|t| if q.is_empty() { tab_of(t).as_ref() == Some(&self.tab) } else { tab_of(t).is_some() })
                    .filter(|t| !filter_unread || t.unread || self.keep_in_unread.as_deref() == Some(t.id.as_str()))
                    .filter(|t| q.is_empty() || self.store.display_name(t).to_lowercase().contains(&q))
                    .collect();
                list.sort_by(|a, b| b.last_activity.total_cmp(&a.last_activity));

                let footer_h = 24.0;
                let area_h = (ui.available_height() - footer_h).max(60.0);
                // message search: needs two letters; results are listed under the chats
                let q_raw = self.search.trim().to_string();
                if q_raw.chars().count() >= 2 {
                    if self.search_for != q_raw {
                        self.search_hits = self.store.db.as_ref().map(|d| d.search_messages(&q_raw, 40)).unwrap_or_default();
                        self.search_for = q_raw.clone();
                    }
                } else if !self.search_for.is_empty() {
                    self.search_hits.clear();
                    self.search_for.clear();
                }
                let hits_h = if self.search_hits.is_empty() { 0.0 } else { (area_h * 0.5).max(140.0) };
                if list.is_empty() && filter_unread {
                    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), (area_h - hits_h).max(60.0)), Sense::hover());
                    ui.painter().text(
                        egui::pos2(rect.center().x, rect.top() + 48.0),
                        Align2::CENTER_CENTER,
                        "Không có cuộc trò chuyện chưa đọc",
                        theme::body(13.0),
                        p.secondary,
                    );
                } else {
                egui::ScrollArea::vertical().auto_shrink([false, false]).drag_to_scroll(false).max_height((area_h - hits_h).max(60.0)).show_rows(ui, ROW_H, list.len(), |ui, range| {
                    ui.spacing_mut().item_spacing.y = 0.0;
                    for t in &list[range] {
                        let selected = self.selected.as_deref() == Some(&t.id);
                        let (rect, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), ROW_H), Sense::click());
                        let hover_t = ui.ctx().animate_bool_with_time(resp.id, resp.hovered(), 0.1);
                        let fill = if selected { p.row_selected } else { p.row_hover.gamma_multiply(hover_t) };
                        // Telegram-style full-bleed row background edge-to-edge up to the divider
                        ui.painter().rect_filled(rect, CornerRadius::ZERO, fill);

                        let name = self.store.display_name(t);
                        let (fg, sub) = if selected { (Color32::WHITE, Color32::from_white_alpha(205)) } else { (p.text, p.secondary) };
                        let av = egui::Rect::from_min_size(egui::pos2(rect.left() + 10.0, rect.center().y - 24.0), Vec2::splat(48.0));
                        let mut child = ui.new_child(egui::UiBuilder::new().max_rect(av));
                        avatar(&mut child, self.store.thread_avatar(t).as_deref(), &name, &t.id, 48.0);

                        let tx = av.right() + 10.0;
                        let right = rect.right() - 13.0;
                        let time = list_time(t.last_activity);
                        let time_g = ui.painter().layout_no_wrap(time, theme::body(12.0), if selected { sub } else { p.secondary });
                        let time_w = time_g.size().x;
                        let name_w = (right - tx - time_w - 8.0).max(30.0);
                        let name_fit = fit_text(ui, &name, &theme::semibold(14.5), name_w);
                        ui.painter().text(egui::pos2(tx, rect.top() + 10.0), Align2::LEFT_TOP, name_fit, theme::semibold(14.5), fg);
                        ui.painter().galley(egui::pos2(right - time_w, rect.top() + 12.0), time_g, Color32::PLACEHOLDER);

                        let badge_w = if t.unread || t.muted { 22.0 } else { 0.0 };
                        let sn_w = (right - tx - badge_w).max(30.0);
                        let sn_fit = fit_text(ui, &t.snippet, &theme::body(13.5), sn_w);
                        ui.painter().text(egui::pos2(tx, rect.top() + 33.0), Align2::LEFT_TOP, sn_fit, theme::body(13.5), sub);
                        if t.unread {
                            let col = if selected { Color32::WHITE } else if t.muted { p.unread_muted } else { p.unread };
                            ui.painter().circle_filled(egui::pos2(right - 5.0, rect.top() + 42.0), 5.0, col);
                        } else if t.muted {
                            ui.painter().text(egui::pos2(right - 1.0, rect.top() + 42.0), Align2::RIGHT_CENTER, "🔕", theme::body(12.0), sub);
                        }

                        if resp.clicked() {
                            actions.push(Action::Select(t.id.clone()));
                        }
                        resp.context_menu(|ui| {
                            let label = if t.muted { "Bật thông báo" } else { "Tắt thông báo" };
                            if ui.button(label).clicked() {
                                actions.push(Action::Mute { thread: t.id.clone(), muted: !t.muted });
                                ui.close_menu();
                            }
                            if ui.button(RichText::new("Xóa cuộc trò chuyện").color(p.danger)).clicked() {
                                actions.push(Action::DeleteThread(t.id.clone()));
                                ui.close_menu();
                            }
                        });
                    }
                });
                }
                if !self.search_hits.is_empty() {
                    ui.horizontal(|ui| {
                        ui.add_space(12.0);
                        ui.label(RichText::new("Tin nhắn").font(theme::semibold(13.5)).color(p.secondary));
                    });
                    egui::ScrollArea::vertical().id_salt("search_hits").auto_shrink([false, false]).max_height((hits_h - 26.0).max(40.0)).show(ui, |ui| {
                        ui.spacing_mut().item_spacing.y = 0.0;
                        for (th, text, ts) in &self.search_hits {
                            let name = self.store.threads.get(th).map(|t| self.store.display_name(t)).unwrap_or_default();
                            let (rect, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 50.0), Sense::click());
                            if resp.hovered() {
                                ui.painter().rect_filled(rect, CornerRadius::ZERO, p.row_hover);
                            }
                            let (tx, right) = (rect.left() + 12.0, rect.right() - 13.0);
                            let time_g = ui.painter().layout_no_wrap(list_time(*ts), theme::body(12.0), p.secondary);
                            let time_w = time_g.size().x;
                            ui.painter().galley(egui::pos2(right - time_w, rect.top() + 10.0), time_g, Color32::PLACEHOLDER);
                            let name_fit = fit_text(ui, &name, &theme::semibold(14.0), (right - tx - time_w - 8.0).max(30.0));
                            ui.painter().text(egui::pos2(tx, rect.top() + 8.0), Align2::LEFT_TOP, name_fit, theme::semibold(14.0), p.text);
                            let snip = fit_text(ui, &around(text, &self.search_for), &theme::body(13.0), (right - tx).max(30.0));
                            ui.painter().text(egui::pos2(tx, rect.top() + 28.0), Align2::LEFT_TOP, snip, theme::body(13.0), p.secondary);
                            if resp.clicked() {
                                actions.push(Action::Select(th.clone()));
                            }
                        }
                    });
                }
                ui.horizontal(|ui| {
                    ui.add_space(10.0);
                    ui.label(RichText::new(self.status_text()).size(11.5).color(p.secondary));
                    if self.store.ready && self.search.trim().is_empty() && ui.small_button("⟳ Chat cũ hơn").on_hover_text("Tải thêm các cuộc trò chuyện cũ hơn").clicked() {
                        actions.push(Action::MoreThreads);
                    }
                    if !self.store.ready && self.backend.is_some() && self.store.exited && ui.small_button("Thử lại").clicked() {
                        actions.push(Action::Retry);
                    }
                });
            });

        // Asymmetric resize handle shifted to the right of the 1px divider ([div_x - 1, div_x + 7])
        // so it NEVER overlaps the sidebar's floating scrollbar ([div_x - 10.5, div_x - 3.5]).
        let pr = panel.response.rect;
        let div_x = pr.right();
        let mut save_w = false;
        egui::Area::new(egui::Id::new("sidebar_splitter"))
            .order(egui::Order::Foreground)
            .fixed_pos(egui::pos2(div_x - 1.0, pr.top()))
            .show(ctx, |ui| {
                let (_, resp) = ui.allocate_exact_size(Vec2::new(8.0, pr.height()), Sense::drag());
                if resp.hovered() || resp.dragged() {
                    ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
                }
                if resp.dragged() {
                    if let Some(pos) = ui.ctx().pointer_interact_pos() {
                        let max_w = (ui.ctx().screen_rect().width() - pr.left() - 260.0).clamp(260.0, 560.0);
                        self.sidebar_w = (pos.x - pr.left()).clamp(260.0, max_w);
                    }
                }
                if resp.drag_stopped() {
                    save_w = true;
                }
            });
        if save_w {
            if let Some(db) = &self.store.db {
                db.set_setting("sidebar_w", &format!("{:.0}", self.sidebar_w));
            }
        }
    }

    fn draw_pin_card(&mut self, ui: &mut egui::Ui, p: &'static Palette, actions: &mut Vec<Action>) {
        egui::Frame::new().fill(p.sidebar).corner_radius(CornerRadius::same(12)).inner_margin(Margin::same(12)).show(ui, |ui| {
            ui.label(RichText::new("🔒 Khôi phục lịch sử chat mã hóa").font(theme::semibold(14.5)).color(p.text));
            ui.label(RichText::new("Nhập mã PIN 6 số của Messenger (Lưu trữ an toàn). PIN chỉ gửi đi để khôi phục, không được lưu lại.").size(12.0).color(p.secondary));
            if let Some(e) = &self.store.history_error {
                let mut t = e.clone();
                if let Some(n) = self.store.history_attempts {
                    t.push_str(&format!(" · còn {n} lần thử"));
                }
                ui.label(RichText::new(t).size(12.0).color(p.danger));
            }
            ui.horizontal(|ui| {
                let r = ui.add(egui::TextEdit::singleline(&mut self.pin).password(true).desired_width(110.0).hint_text("6 chữ số"));
                self.pin.retain(|c| c.is_ascii_digit());
                self.pin.truncate(6);
                let ok = self.pin.len() == 6 && !self.store.history_busy;
                let submit = ui.add_enabled(ok, egui::Button::new("Khôi phục").fill(p.accent)).clicked() || (ok && r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)));
                if submit {
                    actions.push(Action::RestoreHistory);
                }
                if self.store.history_busy {
                    ui.spinner();
                }
            });
        });
        ui.add_space(6.0);
    }

    fn draw_main(&mut self, ctx: &egui::Context) {
        let p = theme::palette(self.dark);
        let mut actions: Vec<Action> = Vec::new();
        // a community that disappeared (or never had channels) falls back to Chats
        if let Tab::Community(id) = &self.tab {
            if !self.store.threads.values().any(|t| tab_of(t) == Some(Tab::Community(id.clone()))) {
                self.tab = Tab::Chats;
            }
        }
        self.draw_rail(ctx, p);
        self.draw_thread_list(ctx, p, &mut actions);

        egui::CentralPanel::default().frame(egui::Frame::new().fill(p.chat_bg)).show(ctx, |ui| {
            if self.store.history_state == "needPIN" {
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    ui.add_space(10.0);
                    ui.vertical(|ui| self.draw_pin_card(ui, p, &mut actions));
                });
            }
            let Some(th_id) = self.selected.clone() else {
                ui.centered_and_justified(|ui| {
                    egui::Frame::new().fill(p.chip).corner_radius(CornerRadius::same(14)).inner_margin(Margin::symmetric(14, 6)).show(ui, |ui| {
                        ui.label(RichText::new("Chọn một cuộc trò chuyện để bắt đầu").size(14.0).color(Color32::WHITE));
                    });
                });
                return;
            };
            let Some(thread) = self.store.threads.get(&th_id).cloned() else { return };
            let name = self.store.display_name(&thread);
            let key = thread.contact.clone().unwrap_or_else(|| th_id.trim_start_matches("e:").to_string());
            let is_e2ee = th_id.starts_with("e:");

            // ----- header -----
            egui::TopBottomPanel::top("chat_header")
                .exact_height(56.0)
                .frame(egui::Frame::new().fill(p.sidebar).inner_margin(Margin::symmetric(14, 0)))
                .show_inside(ui, |ui| {
                    ui.horizontal_centered(|ui| {
                        avatar(ui, self.store.thread_avatar(&thread).as_deref(), &name, &th_id, 38.0);
                        ui.vertical(|ui| {
                            ui.spacing_mut().item_spacing.y = 1.0;
                            ui.add_space(8.0);
                            ui.label(RichText::new(&name).font(theme::semibold(15.0)).color(p.text));
                            if self.store.typing.get(&th_id).copied().unwrap_or(false) {
                                ui.label(RichText::new("đang nhập…").size(12.5).color(p.accent));
                            } else if self.store.presence.get(&key).copied().unwrap_or(false) {
                                ui.label(RichText::new("đang hoạt động").size(12.5).color(p.accent));
                            } else if is_e2ee {
                                ui.label(RichText::new("🔒 mã hóa đầu cuối").size(12.5).color(p.secondary));
                            } else if thread.contact.is_none() {
                                ui.label(RichText::new("nhóm").size(12.5).color(p.secondary));
                            }
                        });
                        // calls run in Facebook's web client (see russenger-call)
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            let can = self.cookies.is_some();
                            let video = ui.add_enabled(can, egui::Button::new(RichText::new("📹").size(19.0).color(p.secondary)).frame(false)).on_hover_text("Gọi video");
                            if video.clicked() {
                                actions.push(Action::Call { thread: th_id.clone(), video: true });
                            }
                            let audio = ui.add_enabled(can, egui::Button::new(RichText::new("📞").size(18.0).color(p.secondary)).frame(false)).on_hover_text("Gọi thoại");
                            if audio.clicked() {
                                actions.push(Action::Call { thread: th_id.clone(), video: false });
                            }
                        });
                    });
                    let r = ui.max_rect();
                    ui.painter().line_segment([r.left_bottom(), r.right_bottom()], egui::Stroke::new(1.0, p.separator));
                });

            // ----- composer -----
            egui::TopBottomPanel::bottom("composer")
                .frame(egui::Frame::new().fill(p.sidebar).inner_margin(Margin::symmetric(12, 8)))
                .show_inside(ui, |ui| {
                    if let Some((_, text, sender)) = &self.reply {
                        let t: String = text.chars().take(90).collect();
                        let who = self.store.sender_name(sender);
                        self.compose_bar(ui, p, &format!("Trả lời {who}"), &t, &mut actions);
                    } else if self.editing.is_some() {
                        self.compose_bar(ui, p, "Sửa tin nhắn", &self.input.clone().chars().take(90).collect::<String>(), &mut actions);
                    }
                    ui.horizontal(|ui| {
                        if ui.add(egui::Button::new(RichText::new("📎").size(18.0).color(p.secondary)).frame(false)).on_hover_text("Gửi ảnh/tệp").clicked() {
                            actions.push(Action::Attach);
                        }
                        // emoji picker (colour pictures; clicking one appends it to the message)
                        let eb = ui.add(egui::Button::image(egui::Image::new(crate::emoji::uri("🙂")).fit_to_exact_size(Vec2::splat(22.0))).frame(false)).on_hover_text("Emoji");
                        if eb.clicked() {
                            self.emoji_open = !self.emoji_open;
                        }
                        if self.emoji_open {
                            let area = egui::Area::new(egui::Id::new("emoji_picker"))
                                .order(egui::Order::Foreground)
                                .fixed_pos(eb.rect.left_top() - Vec2::new(0.0, 8.0))
                                .pivot(Align2::LEFT_BOTTOM)
                                .show(ui.ctx(), |ui| {
                                    egui::Frame::popup(ui.style()).show(ui, |ui| {
                                        ui.set_width(340.0);
                                        egui::ScrollArea::vertical().max_height(260.0).show(ui, |ui| {
                                            ui.horizontal_wrapped(|ui| {
                                                ui.spacing_mut().item_spacing = Vec2::splat(2.0);
                                                for e in crate::emoji::PICKER.split_whitespace() {
                                                    let pic = egui::Image::new(crate::emoji::uri(e)).fit_to_exact_size(Vec2::splat(26.0)).alt_text(e.to_string());
                                                    if ui.add(egui::Button::image(pic).frame(false)).clicked() {
                                                        self.input.push_str(e);
                                                        self.focus_input = true;
                                                    }
                                                }
                                            });
                                        });
                                    });
                                });
                            // a click anywhere else closes the picker
                            if ui.ctx().input(|i| i.pointer.primary_clicked()) {
                                if let Some(pt) = ui.ctx().pointer_interact_pos() {
                                    if !area.response.rect.contains(pt) && !eb.rect.contains(pt) {
                                        self.emoji_open = false;
                                    }
                                }
                            }
                        }
                        // stickers seen in chats can be sent again (regular chats only)
                        let sb = ui.add(egui::Button::new(RichText::new("Sticker").size(12.5).color(p.secondary)).frame(false)).on_hover_text("Sticker gần đây");
                        if sb.clicked() {
                            self.sticker_open = !self.sticker_open;
                            self.emoji_open = false;
                            if self.sticker_open && self.store.stickers.is_empty() {
                                // ask for this chat's recent messages again: stickers in them get noted
                                actions.push(Action::Select(th_id.clone()));
                            }
                        }
                        if self.sticker_open {
                            let area = egui::Area::new(egui::Id::new("sticker_picker"))
                                .order(egui::Order::Foreground)
                                .fixed_pos(sb.rect.left_top() - Vec2::new(0.0, 8.0))
                                .pivot(Align2::LEFT_BOTTOM)
                                .show(ui.ctx(), |ui| {
                                    egui::Frame::popup(ui.style()).show(ui, |ui| {
                                        ui.set_width(360.0);
                                        if is_e2ee {
                                            ui.label(RichText::new("Chưa gửi được sticker trong chat mã hóa đầu cuối.").size(12.5).color(p.secondary));
                                        } else if self.store.stickers.is_empty() {
                                            ui.label(RichText::new("Chưa có sticker nào. Sticker bạn nhận được trong các cuộc trò chuyện sẽ hiện ở đây để gửi lại.").size(12.5).color(p.secondary));
                                        } else {
                                            egui::ScrollArea::vertical().max_height(300.0).show(ui, |ui| {
                                                ui.horizontal_wrapped(|ui| {
                                                    ui.spacing_mut().item_spacing = Vec2::splat(4.0);
                                                    for (sid, url) in &self.store.stickers {
                                                        let pic = egui::Image::new(media_uri(url)).fit_to_exact_size(Vec2::splat(76.0));
                                                        if ui.add(egui::Button::image(pic).frame(false)).clicked() {
                                                            actions.push(Action::SendSticker { thread: th_id.clone(), id: sid.clone() });
                                                            self.sticker_open = false;
                                                        }
                                                    }
                                                });
                                            });
                                        }
                                    });
                                });
                            if ui.ctx().input(|i| i.pointer.primary_clicked()) {
                                if let Some(pt) = ui.ctx().pointer_interact_pos() {
                                    if !area.response.rect.contains(pt) && !sb.rect.contains(pt) {
                                        self.sticker_open = false;
                                    }
                                }
                            }
                        }
                        let id = egui::Id::new("composer");
                        let enter = ui.memory(|m| m.has_focus(id)) && ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Enter));
                        let w = ui.available_width() - 52.0;
                        egui::Frame::new().fill(p.input).corner_radius(CornerRadius::same(18)).inner_margin(Margin::symmetric(14, 8)).show(ui, |ui| {
                            ui.set_width(w - 28.0);
                            egui::ScrollArea::vertical().max_height(110.0).min_scrolled_height(22.0).show(ui, |ui| {
                                let r = ui.add(
                                    egui::TextEdit::multiline(&mut self.input)
                                        .id(id)
                                        .frame(false)
                                        .desired_rows(1)
                                        .desired_width(f32::INFINITY)
                                        .hint_text(RichText::new("Nhập tin nhắn…").color(p.secondary)),
                                );
                                if self.focus_input {
                                    r.request_focus();
                                    self.focus_input = false;
                                }
                                if r.has_focus() && ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                                    actions.push(Action::CancelCompose);
                                }
                            });
                        });
                        let has_text = !self.input.trim().is_empty();
                        let t = ui.ctx().animate_bool_with_time(egui::Id::new("send_t"), has_text, 0.15);
                        let (rect, resp) = ui.allocate_exact_size(Vec2::splat(38.0), Sense::click());
                        let col = p.secondary.lerp_to_gamma(p.accent, t);
                        ui.painter().circle_filled(rect.center(), 19.0, col);
                        ui.painter().text(rect.center() + Vec2::new(1.0, 0.0), Align2::CENTER_CENTER, "➤", theme::body(16.0), Color32::WHITE);
                        if (enter || resp.clicked()) && has_text {
                            actions.push(Action::Send);
                        }
                    });
                });

            // ----- messages -----
            let empty: Vec<Message> = Vec::new();
            let msgs = self.store.messages.get(&th_id).unwrap_or(&empty);
            let metas = row_metas(msgs);
            let is_group = !is_e2ee && thread.contact.is_none();
            let max_w = (ui.available_width() * 0.68).clamp(220.0, 560.0);
            let stuck = *self.stuck_bottom.get(&th_id).unwrap_or(&true);
            let user_scrolled = ctx.input(|i| i.smooth_scroll_delta.y != 0.0 || i.raw_scroll_delta.y != 0.0);
            // a wheel tick must be able to leave the bottom, so no re-alignment in that frame
            let want_bottom = std::mem::take(&mut self.scroll_bottom) || (stuck && !user_scrolled);
            let mut heights = std::mem::take(&mut self.row_heights);
            if heights.len() > 20_000 {
                heights.clear();
            }
            // "seen" goes under my newest message that the other person has read (1:1 chats only)
            let seen_ts = if is_group { 0.0 } else { self.store.seen.get(&th_id).copied().unwrap_or(0.0) };
            let seen_id: Option<String> = if seen_ts > 0.0 {
                msgs.iter().rev().find(|m| m.sender == self.store.self_id && !m.pending && m.ts <= seen_ts + 1000.0).map(|m| m.id.clone())
            } else {
                None
            };
            let out = egui::ScrollArea::vertical().id_salt(&th_id).stick_to_bottom(true).auto_shrink([false, false]).show(ui, |ui| {
                ui.add_space(8.0);
                ui.vertical_centered(|ui| {
                    let busy = self.store.history_busy;
                    let done = self.store.history_done.contains(&th_id);
                    let label = if busy { "Đang tải…" } else if done { "Đã hết tin nhắn cũ" } else { "⟳ Tải tin nhắn cũ hơn" };
                    let b = egui::Button::new(RichText::new(label).size(12.0).color(Color32::WHITE)).fill(p.chip).corner_radius(CornerRadius::same(12));
                    if ui.add_enabled(!busy && !done, b).clicked() {
                        actions.push(Action::LoadEarlier);
                    }
                });
                ui.add_space(4.0);
                let clip = ui.clip_rect();
                let gap = ui.spacing().item_spacing.y;
                for (m, meta) in msgs.iter().zip(metas.iter()) {
                    let is_seen = seen_id.as_deref() == Some(m.id.as_str());
                    let imageish = matches!(m.media_kind.as_deref(), Some("photo" | "image" | "sticker" | "gif")) && m.media_ref.is_some();
                    let sig: u64 = ((max_w as u64) << 32)
                        ^ ((m.text.len() as u64) << 16)
                        ^ ((m.reactions.len() as u64) << 8)
                        ^ (u64::from(m.media_ref.is_some()))
                        ^ (u64::from(m.link.is_some()) << 1)
                        ^ (u64::from(meta.first) << 2)
                        ^ (u64::from(meta.last) << 3)
                        ^ (u64::from(meta.new_day) << 4)
                        ^ (u64::from(is_seen) << 5)
                        ^ (u64::from(m.pending) << 6);
                    let top = ui.cursor().top();
                    if let Some(&(old_sig, h)) = heights.get(&m.id) {
                        let img_unloaded = imageish && crate::cache::exists(m.media_ref.as_deref().unwrap_or("")) && h < 64.0;
                        // far above or below the visible part: keep the space, skip the drawing
                        if old_sig == sig && !img_unloaded && (top + h < clip.top() - 400.0 || top > clip.bottom() + 400.0) {
                            ui.allocate_exact_size(Vec2::new(1.0, (h - gap).max(0.0)), Sense::hover());
                            continue;
                        }
                    }
                    if meta.new_day {
                        ui.add_space(6.0);
                        chip(ui, &day_label(m.ts), p.chip);
                        ui.add_space(6.0);
                    }
                    self.draw_message(ui, p, &th_id, m, meta, max_w, is_group, &mut actions);
                    if is_seen {
                        ui.with_layout(egui::Layout::right_to_left(Align::Min), |ui| {
                            ui.add_space(8.0);
                            ui.label(RichText::new("✓✓ Đã xem").size(11.0).color(p.secondary));
                        });
                    }
                    ui.add_space(if meta.last { 8.0 } else { 2.0 });
                    heights.insert(m.id.clone(), (sig, ui.cursor().top() - top));
                }
                ui.add_space(6.0);
                // Pictures and link cards change height after they were laid out. While the view is
                // at the bottom, re-align it to the end of the content in this same frame.
                if want_bottom {
                    ui.scroll_to_cursor(Some(Align::BOTTOM));
                }
            });

            self.row_heights = heights;

            // scroll-to-bottom floating button
            let max_off = (out.content_size.y - out.inner_rect.height()).max(0.0);
            let away = max_off - out.state.offset.y;
            let st = self.stuck_bottom.entry(th_id.clone()).or_insert(true);
            *st = if user_scrolled { away < 40.0 } else { *st || away < 40.0 };
            if away < 40.0 {
                self.seen_len = msgs.len();
            } else if away > 220.0 {
                let pos = out.inner_rect.right_bottom() - Vec2::new(60.0, 60.0);
                let unread_new = msgs.len().saturating_sub(self.seen_len);
                egui::Area::new(egui::Id::new("to_bottom")).order(egui::Order::Foreground).fixed_pos(pos).show(ui.ctx(), |ui| {
                    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(42.0), Sense::click());
                    ui.painter().circle_filled(rect.center(), 21.0, p.sidebar);
                    ui.painter().circle_stroke(rect.center(), 21.0, egui::Stroke::new(1.0, p.separator));
                    ui.painter().text(rect.center(), Align2::CENTER_CENTER, "⌄", theme::semibold(20.0), p.secondary);
                    if unread_new > 0 {
                        let c = rect.right_top() + Vec2::new(-4.0, 4.0);
                        ui.painter().circle_filled(c, 9.0, p.accent);
                        ui.painter().text(c, Align2::CENTER_CENTER, unread_new.min(99).to_string(), theme::semibold(10.0), Color32::WHITE);
                    }
                    if resp.clicked() {
                        actions.push(Action::ScrollBottom);
                    }
                });
            }
        });

        for a in actions {
            self.handle(a);
        }
    }

    /// Full-window picture viewer: wheel/pinch zoom about the cursor, drag to pan,
    /// double-click to toggle zoom, arrows for previous/next, Esc or click outside to close.
    fn draw_viewer(&mut self, ctx: &egui::Context) {
        let Some(v) = self.viewer.as_mut() else { return };
        let screen = ctx.screen_rect();
        let mut close = false;
        let n = v.items.len();

        ctx.input_mut(|i| {
            if i.consume_key(egui::Modifiers::NONE, egui::Key::Escape) {
                close = true;
            }
            if i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowRight) && v.index + 1 < n {
                v.index += 1;
                v.zoom = 1.0;
                v.offset = Vec2::ZERO;
            }
            if i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowLeft) && v.index > 0 {
                v.index -= 1;
                v.zoom = 1.0;
                v.offset = Vec2::ZERO;
            }
            if i.consume_key(egui::Modifiers::NONE, egui::Key::Plus) || i.consume_key(egui::Modifiers::NONE, egui::Key::Equals) {
                v.zoom = (v.zoom * 1.25).min(24.0);
            }
            if i.consume_key(egui::Modifiers::NONE, egui::Key::Minus) {
                v.zoom = (v.zoom / 1.25).max(0.2);
            }
            if i.consume_key(egui::Modifiers::NONE, egui::Key::Num0) {
                v.zoom = 1.0;
                v.offset = Vec2::ZERO;
            }
        });

        egui::Area::new(egui::Id::new("viewer")).order(egui::Order::Foreground).fixed_pos(screen.min).show(ctx, |ui| {
            let bg = ui.allocate_rect(screen, Sense::click_and_drag());
            ui.painter().rect_filled(screen, 0.0, Color32::from_black_alpha(238));

            let uri = v.items[v.index].1.clone();
            let mut img_rect = egui::Rect::NOTHING;
            match ctx.try_load_texture(&uri, egui::TextureOptions::default(), egui::SizeHint::default()) {
                Ok(egui::load::TexturePoll::Ready { texture }) => {
                    // "fit" size: whole picture inside the window (small pictures up to 2x)
                    let avail = (screen.size() - Vec2::new(60.0, 130.0)).max(Vec2::splat(50.0));
                    let fit = (avail.x / texture.size.x).min(avail.y / texture.size.y).min(2.0);
                    let base = texture.size * fit;

                    let (scroll, pinch) = ctx.input(|i| (i.smooth_scroll_delta.y, i.zoom_delta()));
                    let factor = (scroll * 0.0025).exp() * pinch;
                    if (factor - 1.0).abs() > 1e-4 {
                        if let Some(p) = ctx.pointer_hover_pos() {
                            // keep the point under the cursor fixed while zooming
                            let center = screen.center() + v.offset;
                            let nz = (v.zoom * factor).clamp(0.2, 24.0);
                            let new_center = p + (center - p) * (nz / v.zoom);
                            v.offset = new_center - screen.center();
                            v.zoom = nz;
                        }
                    }
                    if bg.dragged() {
                        v.offset += bg.drag_delta();
                    }
                    if bg.double_clicked() {
                        if v.zoom > 1.05 {
                            v.zoom = 1.0;
                            v.offset = Vec2::ZERO;
                        } else if let Some(p) = ctx.pointer_hover_pos() {
                            let center = screen.center() + v.offset;
                            v.zoom = 2.5;
                            v.offset = (p + (center - p) * 2.5) - screen.center();
                        }
                    }
                    img_rect = egui::Rect::from_center_size(screen.center() + v.offset, base * v.zoom);
                    ui.painter().image(texture.id, img_rect, egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)), Color32::WHITE);
                }
                Ok(egui::load::TexturePoll::Pending { .. }) => {
                    ui.painter().text(screen.center(), Align2::CENTER_CENTER, "Đang tải ảnh…", theme::body(15.0), Color32::from_white_alpha(200));
                    ctx.request_repaint_after(Duration::from_millis(100));
                }
                Err(_) => {
                    ui.painter().text(screen.center(), Align2::CENTER_CENTER, "Không tải được ảnh (link có thể đã hết hạn)", theme::body(15.0), Color32::from_white_alpha(200));
                }
            }
            if bg.clicked() && !img_rect.is_negative() && !ctx.pointer_interact_pos().map(|p| img_rect.contains(p)).unwrap_or(false) {
                close = true;
            }

            // top bar: counter on the left, zoom controls and close on the right
            ui.painter().text(egui::pos2(screen.left() + 20.0, screen.top() + 30.0), Align2::LEFT_CENTER, format!("{} / {}", v.index + 1, n), theme::semibold(14.0), Color32::from_white_alpha(210));
            let bar = egui::Rect::from_min_size(egui::pos2(screen.right() - 345.0, screen.top() + 12.0), Vec2::new(333.0, 36.0));
            let mut child = ui.new_child(egui::UiBuilder::new().max_rect(bar).layout(Layout::right_to_left(Align::Center)));
            if child.add(egui::Button::new(RichText::new("✕").size(16.0).color(Color32::WHITE)).fill(Color32::from_white_alpha(28)).min_size(Vec2::new(36.0, 34.0))).on_hover_text("Đóng (Esc)").clicked() {
                close = true;
            }
            if child.add(egui::Button::new(RichText::new(format!("{:.0}%", v.zoom * 100.0)).color(Color32::WHITE)).fill(Color32::from_white_alpha(28)).min_size(Vec2::new(64.0, 34.0))).on_hover_text("Về kích thước vừa khung (0)").clicked() {
                v.zoom = 1.0;
                v.offset = Vec2::ZERO;
            }
            if child.add(egui::Button::new(RichText::new("+").size(18.0).color(Color32::WHITE)).fill(Color32::from_white_alpha(28)).min_size(Vec2::new(36.0, 34.0))).on_hover_text("Phóng to (+)").clicked() {
                v.zoom = (v.zoom * 1.25).min(24.0);
            }
            if child.add(egui::Button::new(RichText::new("−").size(18.0).color(Color32::WHITE)).fill(Color32::from_white_alpha(28)).min_size(Vec2::new(36.0, 34.0))).on_hover_text("Thu nhỏ (-)").clicked() {
                v.zoom = (v.zoom / 1.25).max(0.2);
            }
            if child.add(egui::Button::new(RichText::new("Lưu").color(Color32::WHITE)).fill(Color32::from_white_alpha(28)).min_size(Vec2::new(56.0, 34.0))).on_hover_text("Lưu ảnh về máy").clicked() {
                // local file or remote url; saved from a worker thread so the window never blocks
                let uri = v.items[v.index].1.clone();
                let source = uri.strip_prefix("file://").map(|p| p.to_string()).unwrap_or(uri);
                let ext = std::path::Path::new(source.split(['?', '#']).next().unwrap_or("")).extension().and_then(|e| e.to_str()).unwrap_or("jpg").to_string();
                std::thread::spawn(move || {
                    if let Some(dest) = rfd::FileDialog::new().set_title("Lưu ảnh").set_file_name(format!("russenger.{ext}")).save_file() {
                        let _ = crate::cache::save_to(&source, &dest);
                    }
                });
            }

            // previous / next
            if n > 1 {
                for (dir, glyph, x) in [(-1i32, "‹", screen.left() + 16.0), (1, "›", screen.right() - 60.0)] {
                    let enabled = if dir < 0 { v.index > 0 } else { v.index + 1 < n };
                    if !enabled {
                        continue;
                    }
                    let r = egui::Rect::from_min_size(egui::pos2(x, screen.center().y - 22.0), Vec2::new(44.0, 44.0));
                    let resp = ui.allocate_rect(r, Sense::click());
                    let a = if resp.hovered() { 70 } else { 38 };
                    ui.painter().circle_filled(r.center(), 22.0, Color32::from_white_alpha(a));
                    ui.painter().text(r.center() - Vec2::new(0.0, 2.0), Align2::CENTER_CENTER, glyph, theme::semibold(26.0), Color32::WHITE);
                    if resp.clicked() {
                        v.index = (v.index as i32 + dir) as usize;
                        v.zoom = 1.0;
                        v.offset = Vec2::ZERO;
                    }
                }
            }
            ui.painter().text(egui::pos2(screen.center().x, screen.bottom() - 22.0), Align2::CENTER_CENTER, "Cuộn để phóng to · kéo để di chuyển · nhấp đúp để phóng to nhanh · ← → chuyển ảnh", theme::body(12.0), Color32::from_white_alpha(120));
        });

        if close {
            self.viewer = None;
        }
    }

    fn compose_bar(&self, ui: &mut egui::Ui, p: &'static Palette, title: &str, preview: &str, actions: &mut Vec<Action>) {
        ui.horizontal(|ui| {
            let (bar, _) = ui.allocate_exact_size(Vec2::new(3.0, 34.0), Sense::hover());
            ui.painter().rect_filled(bar, CornerRadius::same(2), p.accent);
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                ui.label(RichText::new(title).font(theme::semibold(12.5)).color(p.accent));
                ui.label(RichText::new(preview).size(12.5).color(p.secondary));
            });
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.add(egui::Button::new(RichText::new("✕").color(p.secondary)).frame(false)).clicked() {
                    actions.push(Action::CancelCompose);
                }
            });
        });
        ui.add_space(6.0);
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_message(&self, ui: &mut egui::Ui, p: &'static Palette, th: &str, m: &Message, meta: &RowMeta, max_w: f32, is_group: bool, actions: &mut Vec<Action>) {
        if m.system {
            chip(ui, &m.text, p.chip);
            return;
        }
        let mine = m.sender == self.store.self_id;
        let is_sticker = m.media_kind.as_deref() == Some("sticker") && m.media_ref.is_some();
        // up to three emoji and nothing else are shown big, without a bubble
        let emoji_big = m.media_ref.is_none() && m.link.is_none() && crate::emoji::only_emoji(&m.text).map(|n| n <= 3).unwrap_or(false);
        let bare = is_sticker || emoji_big;
        let fill = if bare { Color32::TRANSPARENT } else if mine { p.bubble_out } else { p.bubble_in };
        let layout = if mine { Layout::right_to_left(Align::Min) } else { Layout::left_to_right(Align::Min) };
        let side_pad = 14.0;
        let avatar_col = if !mine && is_group { 38.0 } else { 0.0 };

        ui.with_layout(layout, |ui| {
            ui.add_space(side_pad);
            if avatar_col > 0.0 {
                ui.add_space(avatar_col);
            }
            let group_rect_x = ui.cursor().left();
            let _ = group_rect_x;
            let scope = ui.scope(|ui| {
                ui.set_max_width(max_w);
                // own messages hug the right edge of the scope, others the left
                ui.with_layout(Layout::top_down(if mine { Align::Max } else { Align::Min }), |ui| {
                    ui.spacing_mut().item_spacing.y = 3.0;
                    // per-corner radii: tight corners where neighbours stack
                    let big = 16u8;
                    let small = 5u8;
                    let tail = 3u8;
                    let top_adj = if meta.first { big } else { small };
                    let bot_adj = if meta.last { tail } else { small };
                    let cr = if mine {
                        CornerRadius { nw: big, ne: top_adj, sw: big, se: bot_adj }
                    } else {
                        CornerRadius { nw: top_adj, ne: big, sw: bot_adj, se: big }
                    };

                    let imageish = matches!(m.media_kind.as_deref(), Some("photo" | "image" | "sticker" | "gif")) && m.media_ref.is_some();
                    // the "📷 Photo" / "🔗 Link" style label is only a stand-in until the picture or card is available
                    let has_text = !m.text.is_empty() && !((m.media_ref.is_some() || m.link.is_some()) && is_placeholder(&m.text));
                    let time = hhmm(m.ts);
                    let tick = if mine { if m.pending { " ○" } else { " ✓" } } else { "" };
                    let meta_str = format!("{time}{tick}");
                    let time_col = if mine && !bare { p.time_out } else { p.secondary };

                    // clickable areas inside the bubble (link card, picture); the bubble itself also
                    // senses clicks, so these are resolved from its click position afterwards
                    let mut hits: Vec<(egui::Rect, Hit)> = Vec::new();
                    let frame_resp = egui::Frame::new().fill(fill).corner_radius(cr).inner_margin(Margin::symmetric(10, 6)).show(ui, |ui| {
                        if !mine && is_group && meta.first {
                            ui.label(RichText::new(self.store.sender_name(&m.sender)).font(theme::semibold(13.0)).color(theme::avatar_color(&m.sender)));
                        }
                        if let Some(q) = &m.reply_text {
                            let t: String = q.chars().take(120).collect();
                            let r = egui::Frame::new().fill(p.accent.gamma_multiply(0.18)).corner_radius(CornerRadius::same(4)).inner_margin(Margin { left: 9, right: 6, top: 3, bottom: 3 }).show(ui, |ui| {
                                ui.label(RichText::new(t).size(12.5).color(if mine { p.text_out } else { p.text }));
                            });
                            let rr = r.response.rect;
                            let bar = if mine { p.text_out } else { p.accent };
                            ui.painter().rect_filled(egui::Rect::from_min_size(rr.left_top(), Vec2::new(3.0, rr.height())), CornerRadius::same(2), bar);
                        }
                        // shared link / reel / post: a card with picture, source, title and the link itself
                        if let Some(l) = &m.link {
                            let fg = if mine { p.text_out } else { p.text };
                            let sub = if mine { p.time_out } else { p.secondary };
                            let card = egui::Frame::new().fill(Color32::from_black_alpha(55)).corner_radius(CornerRadius::same(10)).show(ui, |ui| {
                                ui.set_max_width(280.0);
                                if !l.image.is_empty() {
                                    ui.add(egui::Image::new(media_uri(&l.image)).fit_to_original_size(1.0).max_size(Vec2::new(280.0, 170.0)).corner_radius(CornerRadius { nw: 10, ne: 10, sw: 0, se: 0 }));
                                }
                                egui::Frame::new().inner_margin(Margin::symmetric(9, 6)).show(ui, |ui| {
                                    ui.spacing_mut().item_spacing.y = 2.0;
                                    let domain = l.domain();
                                    let src = if !l.source.is_empty() { l.source.clone() } else { domain.clone() };
                                    if !src.is_empty() {
                                        ui.label(RichText::new(src.to_uppercase()).size(10.5).color(sub));
                                    }
                                    if !l.title.is_empty() {
                                        ui.label(RichText::new(&l.title).font(theme::semibold(13.5)).color(fg));
                                    }
                                    let desc = if !l.description.is_empty() { &l.description } else { &l.subtitle };
                                    if !desc.is_empty() {
                                        let d: String = desc.chars().take(160).collect();
                                        ui.label(RichText::new(d).size(12.5).color(sub));
                                    }
                                    if !l.url.is_empty() {
                                        ui.add(egui::Label::new(RichText::new(&l.url).size(11.5).color(p.link).underline()).truncate());
                                    }
                                });
                            });
                            if !l.url.is_empty() {
                                hits.push((card.response.rect, Hit::Url(l.url.clone())));
                            }
                        }
                        if let (Some(_), Some(r)) = (&m.media_kind, &m.media_ref) {
                            if imageish && !crate::cache::exists(r) {
                                ui.label(RichText::new("🖼 Ảnh đã bị xóa khỏi bộ nhớ đệm").size(12.5).color(p.secondary));
                            } else if imageish {
                                let resp = ui.add(egui::Image::new(media_uri(r)).fit_to_original_size(1.0).max_size(if is_sticker { Vec2::splat(120.0) } else { Vec2::new(280.0, 320.0) }).corner_radius(CornerRadius::same(if is_sticker { 0 } else { 10 })).sense(Sense::click()));
                                if !has_text && !is_sticker {
                                    let g = ui.painter().layout_no_wrap(meta_str.clone(), theme::body(11.0), Color32::WHITE);
                                    let pill = egui::Rect::from_min_size(resp.rect.right_bottom() - g.size() - Vec2::new(14.0, 8.0), g.size() + Vec2::new(10.0, 4.0));
                                    ui.painter().rect_filled(pill, CornerRadius::same(9), Color32::from_black_alpha(120));
                                    ui.painter().galley(pill.min + Vec2::new(5.0, 2.0), g, Color32::WHITE);
                                }
                                if !is_sticker {
                                    hits.push((resp.rect, Hit::Image(m.id.clone())));
                                }
                            } else if let Some(kind) = &m.media_kind {
                                let label = match kind.as_str() {
                                    "video" => "▶ Phát video",
                                    "audio" => "▶ Phát âm thanh",
                                    _ => "💾 Lưu tệp…",
                                };
                                if !crate::cache::exists(r) {
                                    ui.label(RichText::new("📎 Tệp đã bị xóa khỏi bộ nhớ đệm").size(12.5).color(p.secondary));
                                } else {
                                    let b = ui.button(label);
                                    hits.push((b.rect, if matches!(kind.as_str(), "video" | "audio") { Hit::Play(r.clone()) } else { Hit::File(r.clone()) }));
                                }
                            }
                        }
                        if has_text && crate::emoji::contains_emoji(&m.text) {
                            // text mixed with emoji pictures; a spacer reserves room for the time
                            let size = if emoji_big { 40.0 } else { 19.0 };
                            let fg = if mine && !bare { p.text_out } else { p.text };
                            let time_w = ui.painter().layout_no_wrap(meta_str.clone(), theme::body(11.0), Color32::WHITE).size().x + 8.0;
                            ui.horizontal_wrapped(|ui| {
                                ui.spacing_mut().item_spacing = Vec2::new(0.0, 2.0);
                                for seg in crate::emoji::split(&m.text) {
                                    match seg {
                                        crate::emoji::Seg::Text(t) => {
                                            ui.label(RichText::new(t).font(theme::body(14.5)).color(fg));
                                        }
                                        crate::emoji::Seg::Emoji(e) => {
                                            ui.add(egui::Image::new(crate::emoji::uri(e)).fit_to_exact_size(Vec2::splat(size)).alt_text(e.to_string()));
                                        }
                                    }
                                }
                                if !emoji_big {
                                    ui.add_space(time_w);
                                }
                            });
                            if emoji_big {
                                ui.label(RichText::new(&meta_str).size(11.0).color(time_col));
                            }
                        } else if has_text {
                            // invisible spacer reserves room for the time painted at the bottom-right
                            let mut job = LayoutJob::default();
                            job.append(&m.text, 0.0, TextFormat { font_id: theme::body(14.5), color: if mine { p.text_out } else { p.text }, ..Default::default() });
                            job.append(&format!("   {meta_str}"), 0.0, TextFormat { font_id: theme::body(11.0), color: Color32::TRANSPARENT, ..Default::default() });
                            ui.add(egui::Label::new(job).wrap());
                        } else if !imageish || is_sticker {
                            // card / file / sticker only: no text line to hang the time on
                            ui.label(RichText::new(&meta_str).size(11.0).color(time_col));
                        }
                    });
                    let rect = frame_resp.response.rect;
                    if has_text && !emoji_big {
                        ui.painter().text(rect.right_bottom() - Vec2::new(10.0, 5.0), Align2::RIGHT_BOTTOM, meta_str, theme::body(11.0), time_col);
                    }

                    // reactions as pills
                    if !m.reactions.is_empty() {
                        let mut counts: Vec<(String, usize, bool)> = Vec::new();
                        for (actor, e) in &m.reactions {
                            let me = *actor == self.store.self_id;
                            match counts.iter_mut().find(|(x, _, _)| x == e) {
                                Some(c) => {
                                    c.1 += 1;
                                    c.2 |= me;
                                }
                                None => counts.push((e.clone(), 1, me)),
                            }
                        }
                        ui.horizontal_wrapped(|ui| {
                            ui.spacing_mut().item_spacing.x = 4.0;
                            for (e, n, me) in counts {
                                let col = if me { p.accent } else { p.input };
                                let pic = egui::Image::new(crate::emoji::uri(&e)).fit_to_exact_size(Vec2::splat(16.0)).alt_text(e.clone());
                                let b = if n > 1 {
                                    egui::Button::image_and_text(pic, RichText::new(n.to_string()).size(12.5).color(if me { Color32::WHITE } else { p.text }))
                                } else {
                                    egui::Button::image(pic)
                                }
                                .fill(col)
                                .corner_radius(CornerRadius::same(12))
                                .small();
                                if ui.add(b).clicked() {
                                    actions.push(Action::React { thread: th.to_string(), id: m.id.clone(), emoji: if me { String::new() } else { e }, from_me: mine });
                                }
                            }
                        });
                    }

                    let resp = frame_resp.response.interact(Sense::click());
                    if let Some(pos) = resp.interact_pointer_pos() {
                        if resp.hovered() && hits.iter().any(|(r, _)| r.contains(pos)) {
                            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                        }
                        if resp.clicked() {
                            if let Some((_, hit)) = hits.iter().find(|(r, _)| r.contains(pos)) {
                                actions.push(match hit {
                                    Hit::Url(u) => Action::OpenUrl(u.clone()),
                                    Hit::File(u) => Action::SaveFile(u.clone()),
                                    Hit::Image(id) => Action::ViewImage { thread: th.to_string(), id: id.clone() },
                                    Hit::Play(r) => Action::Play(r.clone()),
                                });
                            }
                        }
                    } else if resp.hovered() {
                        if let Some(pos) = ui.ctx().pointer_hover_pos() {
                            if hits.iter().any(|(r, _)| r.contains(pos)) {
                                ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                            }
                        }
                    }
                    if resp.double_clicked() {
                        actions.push(Action::StartReply { id: m.id.clone(), text: m.text.clone(), sender: m.sender.clone() });
                    }
                    resp.context_menu(|ui| {
                        ui.horizontal(|ui| {
                            for e in REACTIONS {
                                let pic = egui::Image::new(crate::emoji::uri(e)).fit_to_exact_size(Vec2::splat(24.0)).alt_text(e.to_string());
                                if ui.add(egui::Button::image(pic).frame(false)).clicked() {
                                    actions.push(Action::React { thread: th.to_string(), id: m.id.clone(), emoji: e.to_string(), from_me: mine });
                                    ui.close_menu();
                                }
                            }
                        });
                        if ui.button("↩ Trả lời").clicked() {
                            actions.push(Action::StartReply { id: m.id.clone(), text: m.text.clone(), sender: m.sender.clone() });
                            ui.close_menu();
                        }
                        if !m.text.is_empty() && ui.button("Sao chép").clicked() {
                            actions.push(Action::Copy(m.text.clone()));
                            ui.close_menu();
                        }
                        if mine && !m.pending {
                            if !m.text.is_empty() && ui.button("✎ Sửa").clicked() {
                                actions.push(Action::StartEdit { id: m.id.clone(), text: m.text.clone() });
                                ui.close_menu();
                            }
                            if ui.button(RichText::new("Thu hồi").color(p.danger)).clicked() {
                                actions.push(Action::Unsend { thread: th.to_string(), id: m.id.clone() });
                                ui.close_menu();
                            }
                        }
                    });
                });
            });

            // group avatar next to the last bubble of an incoming run
            if avatar_col > 0.0 && meta.last {
                let r = scope.response.rect;
                let av_rect = egui::Rect::from_min_size(egui::pos2(r.left() - 34.0, r.bottom() - 30.0), Vec2::splat(30.0));
                let mut child = ui.new_child(egui::UiBuilder::new().max_rect(av_rect));
                let name = self.store.sender_name(&m.sender);
                avatar(&mut child, self.store.avatars.get(&m.sender).map(|s| s.as_str()), &name, &m.sender, 30.0);
            }
        });
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // a click on a toast: bring the window forward and open that chat
        if let Some(th) = notify::take_open_request() {
            match self.hwnd {
                Some(h) if crate::tray::PARKED.load(std::sync::atomic::Ordering::Relaxed) => crate::tray::show(h),
                _ => {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
                    ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                }
            }
            if !th.is_empty() && th != "summary" && self.screen == Screen::Main && self.store.threads.contains_key(&th) {
                self.handle(Action::Select(th));
            }
        }
        self.pump_events();
        self.pump_login();

        // resolve names/avatars for people we only know by id
        if self.store.ready && self.backend.is_some() {
            let unknown: Vec<String> = self
                .store
                .threads
                .values()
                .filter(|t| t.name.is_empty())
                .map(|t| t.contact.clone().unwrap_or_else(|| t.id.trim_start_matches("e:").to_string()))
                .filter(|k| !self.store.contacts.contains_key(k) && !self.requested.contains(k))
                .take(5)
                .collect();
            for k in unknown {
                self.requested.insert(k.clone());
                if let Ok(n) = k.parse::<i64>() {
                    self.send_cmd(json!({"cmd": "fetchContact", "id": n}));
                }
            }
        }

        // the first sync only has the newest chats: ask for older pages once, after the list is in
        if self.store.ready && !self.more_threads_asked && self.backend.is_some() && self.backend_started.elapsed() > Duration::from_secs(6) {
            self.more_threads_asked = true;
            self.ask_more_threads();
        }

        // ask whether encrypted history can be restored, once the encrypted channel had time to connect
        if self.store.ready && !self.history_asked && self.backend.is_some() && self.backend_started.elapsed() > Duration::from_secs(8) {
            self.history_asked = true;
            self.send_cmd(json!({"cmd": "historyStatus"}));
        }

        // encrypted threads can only be repaired once the history state is known
        if let Some(th) = self.selected.clone() {
            if th.starts_with("e:") && !self.repaired.contains(&th) {
                self.repair_media(&th);
            }
        }

        // a message that failed to send goes back into the box so nothing typed is lost
        for tag in std::mem::take(&mut self.store.failed_sends) {
            if let Some(text) = self.sent_texts.remove(&tag) {
                if self.input.trim().is_empty() {
                    self.input = text;
                    self.focus_input = true;
                }
            }
        }

        // the close button hides the window into the tray instead of quitting
        if self.tray_on && self.tray.is_some() && ctx.input(|i| i.viewport().close_requested()) {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            if let Some(h) = self.hwnd {
                crate::tray::hide(h);
            }
        }

        // files dragged onto the window, and Ctrl+V with a picture on the clipboard
        if self.screen == Screen::Main && self.selected.is_some() && self.backend.is_some() && self.outgoing.is_empty() {
            for f in ctx.input(|i| i.raw.dropped_files.clone()) {
                if let Some(p) = f.path {
                    self.outgoing.push((p, false));
                }
            }
            let down = ctx.input(|i| i.focused) && unsafe {
                use windows_sys::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
                GetAsyncKeyState(0x11) < 0 && GetAsyncKeyState(0x56) < 0
            };
            if down && !self.paste_down {
                self.paste_clipboard_image();
            }
            self.paste_down = down;
        }

        // keep the open thread read while the window is focused (incl. after refreshThread brings in new messages)
        let focused = ctx.input(|i| i.focused);
        if focused {
            if let Some(th) = self.selected.clone() {
                let latest_in = self.latest_incoming_id(&th);
                let new_in_msg = !latest_in.is_empty() && self.last_read_msg_id.get(&th) != Some(&latest_in);
                let needs_read = self
                    .store
                    .threads
                    .get(&th)
                    .map(|t| {
                        t.unread
                            || (self.store.ready && (t.read_up_to > t.synced_read_up_to + 999.0 || t.last_activity > t.synced_read_up_to + 999.0 || new_in_msg))
                    })
                    .unwrap_or(false);
                if needs_read {
                    self.mark_read(&th);
                }
            }
        }

        // Sync any threads that were already read locally while offline/connecting (or before closing the app)
        // so Facebook's server watermark catches up and never resurrects them as unread on next start.
        if self.store.ready && self.backend.is_some() && self.backend_started.elapsed() > Duration::from_secs(5) {
            let unsynced: Vec<String> = self
                .store
                .threads
                .values()
                .filter(|t| !t.unread && t.read_up_to > 0.0 && t.read_up_to > t.synced_read_up_to + 999.0)
                .map(|t| t.id.clone())
                .take(5)
                .collect();
            for th in unsynced {
                self.mark_read(&th);
            }
        }

        // Toasts for live messages while the window is not focused. Messages arriving close
        // together are collected and shown as ONE toast (per chat, or a summary over several).
        if focused {
            self.store.notifications.clear();
            self.pending_since = None;
        } else if !self.store.notifications.is_empty() {
            let since = *self.pending_since.get_or_insert_with(Instant::now);
            let ready_at = (since + Duration::from_millis(1500)).max(self.last_toast + Duration::from_secs(4));
            if Instant::now() >= ready_at {
                let batch = std::mem::take(&mut self.store.notifications);
                self.pending_since = None;
                let (t, b, tag) = summarize_notifications(&batch);
                notify::toast_for(&t, &b, &tag, &tag);
                self.last_toast = Instant::now();
            } else {
                ctx.request_repaint_after(ready_at - Instant::now());
            }
        }

        let unread = self.store.threads.values().filter(|t| t.unread && !t.muted).count();
        let title = if unread > 0 { format!("Russenger ({unread})") } else { "Russenger".to_string() };
        if title != self.last_title {
            // sending a viewport command repaints the window, so only do it when the title changes
            ctx.send_viewport_cmd(egui::ViewportCommand::Title(title.clone()));
            self.last_title = title;
        }

        match self.screen {
            Screen::Login => self.draw_login(ctx),
            Screen::Main => {
                self.draw_main(ctx);
                if self.settings_open {
                    self.draw_settings(ctx);
                }
                if !self.outgoing.is_empty() {
                    self.draw_outgoing(ctx);
                }
                if self.viewer.is_some() {
                    self.draw_viewer(ctx);
                }
            }
        }
    }
}
