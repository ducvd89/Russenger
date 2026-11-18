use crate::db::Db;
use serde_json::Value;
use std::collections::HashMap;

#[derive(Default, Clone)]
pub struct Thread {
    pub id: String,
    pub name: String,
    pub snippet: String,
    pub picture: String,
    pub last_activity: f64,
    pub unread: bool,
    pub muted: bool,
    pub contact: Option<String>,
    /// Messenger thread kind (5 = marketplace, 17-21 = community folder / channels)
    pub ttype: i64,
    /// id of the community a channel belongs to ("" / "0" / "-1" = none)
    pub parent: String,
    /// "inbox" | "requests" | "archived" | "spam" | "hidden" ("" = unknown, treated as inbox)
    pub folder: String,
    /// Local watermark (ms) up to which the user has read this conversation.
    pub read_up_to: f64,
    /// Watermark (ms) already sent to / confirmed by the server.
    pub synced_read_up_to: f64,
}

#[derive(Default, Clone)]
pub struct Community {
    pub name: String,
    pub description: String,
    pub picture: String,
    pub members: i64,
}

/// A shared link / reel / post card (Messenger "XMA" attachment) — not a picture the user sent.
#[derive(Clone, Default)]
pub struct LinkPreview {
    pub title: String,
    pub subtitle: String,
    pub description: String,
    pub source: String,
    pub url: String,
    pub image: String,
}

impl LinkPreview {
    pub fn to_json(&self) -> String {
        serde_json::json!({"t": self.title, "s": self.subtitle, "d": self.description, "src": self.source, "u": self.url, "i": self.image}).to_string()
    }
    pub fn from_json(j: &str) -> Option<Self> {
        let v: Value = serde_json::from_str(j).ok()?;
        let g = |k: &str| v.get(k).and_then(Value::as_str).unwrap_or("").to_string();
        Some(Self { title: g("t"), subtitle: g("s"), description: g("d"), source: g("src"), url: g("u"), image: g("i") })
    }
    /// Host name of the link, for display.
    pub fn domain(&self) -> String {
        let u = self.url.trim_start_matches("https://").trim_start_matches("http://");
        u.split('/').next().unwrap_or("").trim_start_matches("www.").to_string()
    }
}

#[derive(Clone, Default)]
pub struct Message {
    pub id: String,
    pub sender: String,
    pub text: String,
    pub ts: f64,
    pub system: bool,
    pub reply_text: Option<String>,
    pub media_kind: Option<String>,
    /// http(s) URL or a local file path
    pub media_ref: Option<String>,
    pub reactions: Vec<(String, String)>,
    pub pending: bool,
    pub link: Option<LinkPreview>,
}

pub const PAGE: usize = 200;

/// Placeholder texts the helper uses for attachment-only messages.
pub fn is_placeholder(text: &str) -> bool {
    matches!(text, "📷 Photo" | "💟 Sticker" | "🎬 Video" | "🎤 Voice message" | "📎 File" | "📎 Attachment" | "🔗 Link" | "[Tệp đính kèm]")
}

#[derive(Default)]
pub struct Store {
    pub self_id: String,
    pub self_name: String,
    pub ready: bool,
    pub need_login: bool,
    pub exited: bool,
    pub last_error: Option<String>,
    pub contacts: HashMap<String, String>,
    pub avatars: HashMap<String, String>,
    pub threads: HashMap<String, Thread>,
    pub messages: HashMap<String, Vec<Message>>,
    pub typing: HashMap<String, bool>,
    pub presence: HashMap<String, bool>,
    /// (title, body) pairs produced by live incoming messages, drained by the UI for toasts
    pub notifications: Vec<(String, String, String)>,
    /// client tags of messages that failed to send (drained by the UI)
    pub failed_sends: Vec<String>,
    /// stickers seen in chats, newest first: (sticker id, picture address)
    pub stickers: Vec<(String, String)>,
    /// per thread: time up to which the other person has read our messages
    pub seen: std::collections::HashMap<String, f64>,
    pub db: Option<Db>,
    /// "" (unknown) | "none" | "needPIN" | "restored" | "error"
    pub history_state: String,
    pub history_error: Option<String>,
    pub history_attempts: Option<i64>,
    pub history_busy: bool,
    /// threads whose encrypted archive has no older messages left
    pub history_done: std::collections::HashSet<String>,
    /// message id -> (kind, url or local path); media events can precede their message
    pub media: HashMap<String, (String, String)>,
    /// message id -> link card (events can precede their message, like media)
    pub links: HashMap<String, LinkPreview>,
    pub communities: HashMap<String, Community>,
    pub first_names: HashMap<String, String>,
    /// Max message timestamp per thread stored before this session synced, used to forward-backfill gaps.
    pub pre_sync_max_ts: HashMap<String, f64>,
    /// Redirect map when an unprefixed / hybrid thread id merges into its canonical "e:<id>" thread.
    pub merged_threads: HashMap<String, String>,
}

fn s(v: &Value, k: &str) -> String {
    match v.get(k) {
        Some(Value::String(x)) => x.clone(),
        Some(Value::Number(n)) => n.to_string(),
        _ => String::new(),
    }
}
fn opt(v: &Value, k: &str) -> Option<String> {
    let x = s(v, k);
    if x.is_empty() { None } else { Some(x) }
}
fn f(v: &Value, k: &str) -> f64 {
    v.get(k).and_then(Value::as_f64).unwrap_or(0.0)
}
fn b(v: &Value, k: &str) -> bool {
    v.get(k).and_then(Value::as_bool).unwrap_or(false)
}

impl Store {
    pub fn with_db(db: Option<Db>) -> Self {
        let mut st = Store::default();
        if let Some(db) = &db {
            st.pre_sync_max_ts = db.max_message_ts_by_thread();
            for t in db.load_threads() {
                st.threads.insert(t.id.clone(), t);
            }
            for (id, kind, r) in db.load_media() {
                st.media.insert(id, (kind, r));
            }
            for (id, c) in db.load_communities() {
                st.communities.insert(id, c);
            }
            for (id, j) in db.load_links() {
                if let Some(l) = LinkPreview::from_json(&j) {
                    st.links.insert(id, l);
                }
            }
            for (id, (name, avatar)) in db.load_contacts() {
                if !name.is_empty() {
                    st.contacts.insert(id.clone(), name);
                }
                if !avatar.is_empty() {
                    st.avatars.insert(id, avatar);
                }
            }
        }
        st.stickers = db.as_ref().map(|d| d.load_stickers(60)).unwrap_or_default();
        st.db = db;
        // Heal any split threads where an encrypted thread "e:<id>" also left rows under plain "<id>".
        let e2ee_ids: Vec<String> = st.threads.keys().filter(|k| k.starts_with("e:")).cloned().collect();
        for eid in e2ee_ids {
            let raw = eid.trim_start_matches("e:").to_string();
            st.merge_thread(&raw, &eid);
        }
        st
    }

    pub fn canonical_thread(&self, th: &str) -> String {
        if let Some(to) = self.merged_threads.get(th) {
            return to.clone();
        }
        if !th.is_empty() && !th.starts_with("e:") {
            let eid = format!("e:{th}");
            if self.threads.contains_key(&eid) {
                return eid;
            }
        }
        th.to_string()
    }

    /// Merge a duplicate plain/hybrid thread into its canonical encrypted thread, preserving messages and metadata.
    pub fn merge_thread(&mut self, from: &str, to: &str) {
        if from.is_empty() || to.is_empty() || from == to {
            return;
        }
        self.merged_threads.insert(from.to_string(), to.to_string());
        if let Some(ts) = self.pre_sync_max_ts.remove(from) {
            let e = self.pre_sync_max_ts.entry(to.to_string()).or_insert(0.0);
            if ts > *e {
                *e = ts;
            }
        }
        if let Some(old_t) = self.threads.remove(from) {
            let t = self.threads.entry(to.to_string()).or_default();
            if t.id.is_empty() {
                t.id = to.to_string();
            }
            if t.name.is_empty() && !old_t.name.is_empty() {
                t.name = old_t.name;
            }
            if t.picture.is_empty() && !old_t.picture.is_empty() {
                t.picture = old_t.picture;
            }
            if t.contact.is_none() {
                t.contact = old_t.contact;
            }
            if t.folder.is_empty() && !old_t.folder.is_empty() {
                t.folder = old_t.folder;
                t.ttype = old_t.ttype;
                t.parent = old_t.parent;
            }
            t.muted |= old_t.muted;
            t.read_up_to = t.read_up_to.max(old_t.read_up_to);
            t.synced_read_up_to = t.synced_read_up_to.max(old_t.synced_read_up_to);
            if old_t.last_activity > t.last_activity {
                t.last_activity = old_t.last_activity;
                if !old_t.snippet.is_empty() {
                    t.snippet = old_t.snippet;
                }
            }
            if t.read_up_to > 0.0 && t.last_activity <= t.read_up_to + 999.0 {
                t.unread = false;
            } else if old_t.unread && old_t.last_activity > t.read_up_to + 999.0 {
                t.unread = true;
            }
        }
        if let Some(old_msgs) = self.messages.remove(from) {
            if let Some(list) = self.messages.get_mut(to) {
                for m in old_msgs {
                    if !list.iter().any(|x| x.id == m.id) {
                        list.push(m);
                    }
                }
                list.sort_by(|a, b| a.ts.total_cmp(&b.ts).then_with(|| a.id.cmp(&b.id)));
            }
        }
        if let Some(db) = &self.db {
            db.merge_threads(from, to);
            if let Some(t) = self.threads.get(to) {
                db.save_thread(t);
            }
        }
    }

    fn resolve_reply_text(&self, thread: &str, reply_id: &str) -> Option<String> {
        if reply_id.is_empty() {
            return None;
        }
        if let Some(list) = self.messages.get(thread) {
            if let Some(m) = list.iter().find(|m| m.id == reply_id) {
                if !m.text.is_empty() {
                    return Some(m.text.clone());
                }
            }
        }
        self.db.as_ref().and_then(|d| d.find_message_text(reply_id))
    }

    /// Lazily load a thread's stored history the first time it is opened.
    pub fn ensure_loaded(&mut self, thread: &str) {
        if self.messages.contains_key(thread) {
            return;
        }
        let list = self.db.as_ref().map(|d| d.load_messages(thread, PAGE)).unwrap_or_default();
        let list = self.decorate(thread, list);
        self.messages.insert(thread.to_string(), list);
    }

    /// Attach pictures, link cards and reactions kept outside the message rows.
    fn decorate(&self, thread: &str, mut list: Vec<Message>) -> Vec<Message> {
        let reactions = self.db.as_ref().map(|d| d.load_reactions(thread)).unwrap_or_default();
        for m in list.iter_mut() {
            if let Some((k, r)) = self.media.get(&m.id) {
                m.media_kind = Some(k.clone());
                m.media_ref = Some(r.clone());
            }
            m.link = self.links.get(&m.id).cloned();
            m.reactions = reactions.iter().filter(|(id, _, _)| *id == m.id).map(|(_, a, e)| (a.clone(), e.clone())).collect();
        }
        list
    }

    /// Show one more page of already stored older messages. Returns how many were added
    /// (0 = the stored history is used up and the server has to be asked).
    pub fn load_older_from_db(&mut self, thread: &str) -> usize {
        let Some(oldest) = self.messages.get(thread).and_then(|l| l.iter().filter(|m| !m.pending).map(|m| m.ts).reduce(f64::min)) else { return 0 };
        let older = self.db.as_ref().map(|d| d.load_messages_before(thread, oldest, PAGE)).unwrap_or_default();
        let older = self.decorate(thread, older);
        let list = self.messages.entry(thread.to_string()).or_default();
        let mut added = 0;
        for m in older {
            if !list.iter().any(|x| x.id == m.id) {
                list.push(m);
                added += 1;
            }
        }
        if added > 0 {
            list.sort_by(|a, b| a.ts.total_cmp(&b.ts).then_with(|| a.id.cmp(&b.id)));
        }
        added
    }

    pub fn apply(&mut self, ev: &Value) {
        match s(ev, "type").as_str() {
            "ready" => {
                self.ready = true;
                self.need_login = false;
                if self.self_id.is_empty() {
                    self.self_id = s(ev, "self");
                }
            }
            "self" => {
                self.self_id = s(ev, "id");
                self.self_name = s(ev, "name");
            }
            "needLogin" => self.need_login = true,
            "stickerSeen" => {
                let (id, url) = (s(ev, "stickerId"), s(ev, "url"));
                if !id.is_empty() && !url.is_empty() {
                    self.stickers.retain(|(i, _)| *i != id);
                    self.stickers.insert(0, (id.clone(), url.clone()));
                    self.stickers.truncate(60);
                    if let Some(db) = &self.db {
                        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as f64).unwrap_or(0.0);
                        db.save_sticker(&id, &url, now);
                    }
                }
            }
            "seen" => {
                let th = self.canonical_thread(&s(ev, "thread"));
                let ts = f(ev, "ts");
                let e = self.seen.entry(th).or_insert(0.0);
                if ts > *e {
                    *e = ts;
                }
            }
            "read" => {
                let th = self.canonical_thread(&s(ev, "thread"));
                let rts = f(ev, "readUpTo");
                if rts > 0.0 {
                    if let Some(t) = self.threads.get_mut(&th) {
                        t.read_up_to = t.read_up_to.max(rts);
                        t.synced_read_up_to = t.synced_read_up_to.max(rts);
                        if t.last_activity <= t.read_up_to + 999.0 {
                            t.unread = false;
                        }
                        if let Some(db) = &self.db {
                            db.save_thread(t);
                        }
                    }
                }
            }
            "mergeThread" => {
                let (from, to) = (s(ev, "from"), s(ev, "to"));
                self.merge_thread(&from, &to);
            }
            "deleteThread" => {
                let id = self.canonical_thread(&s(ev, "thread"));
                self.threads.remove(&id);
                self.messages.remove(&id);
                if let Some(db) = &self.db {
                    db.delete_thread(&id);
                }
            }
            "exited" => self.exited = true,
            "error" => self.last_error = Some(s(ev, "msg")),
            "sendFailed" => {
                // the message never left: drop its pending bubble and let the UI give the text back
                let tag = s(ev, "clientTag");
                let th = self.canonical_thread(&s(ev, "thread"));
                self.last_error = Some(s(ev, "msg"));
                if let Some(list) = self.messages.get_mut(&th) {
                    list.retain(|m| m.id != tag);
                }
                self.failed_sends.push(tag);
            }
            "contact" => {
                let (id, name, avatar) = (s(ev, "id"), s(ev, "name"), s(ev, "avatar"));
                if !name.is_empty() {
                    self.contacts.insert(id.clone(), name.clone());
                }
                if !avatar.is_empty() {
                    self.avatars.insert(id.clone(), avatar.clone());
                }
                let first = s(ev, "firstName");
                if !first.is_empty() {
                    self.first_names.insert(id.clone(), first);
                }
                if let Some(db) = &self.db {
                    db.save_contact(&id, &name, &avatar);
                }
            }
            "historyStatus" => {
                self.history_state = s(ev, "state");
                self.history_error = opt(ev, "error");
                self.history_attempts = ev.get("attemptsRemaining").and_then(Value::as_i64);
                self.history_busy = false;
            }
            "historyPage" => {
                self.history_busy = false;
                if !b(ev, "hasMore") {
                    self.history_done.insert(self.canonical_thread(&s(ev, "thread")));
                }
            }
            "presence" => {
                self.presence.insert(s(ev, "id"), b(ev, "online"));
            }
            "thread" => {
                let raw_id = s(ev, "id");
                let id = if raw_id.starts_with("e:") {
                    let plain = raw_id.trim_start_matches("e:").to_string();
                    self.merge_thread(&plain, &raw_id);
                    if let Some(mf) = opt(ev, "mergeFrom") {
                        self.merge_thread(&mf, &raw_id);
                    }
                    raw_id
                } else {
                    self.canonical_thread(&raw_id)
                };
                let is_partial = b(ev, "partial");
                let from_lightspeed = ev.get("folder").is_some();
                let t = self.threads.entry(id.clone()).or_default();
                t.id = id;
                if is_partial {
                    if ev.get("name").is_some() {
                        let n = s(ev, "name");
                        if !n.is_empty() {
                            t.name = n;
                        }
                    }
                    if ev.get("picture").is_some() {
                        let p = s(ev, "picture");
                        if !p.is_empty() {
                            t.picture = p;
                        }
                    }
                    if ev.get("muted").is_some() {
                        t.muted = b(ev, "muted");
                    }
                    if ev.get("folder").is_some() {
                        t.folder = s(ev, "folder");
                    }
                    if ev.get("parent").is_some() {
                        t.parent = s(ev, "parent");
                    }
                    let pts = f(ev, "lastActivity");
                    if pts > t.last_activity {
                        t.last_activity = pts;
                        let psnip = s(ev, "snippet");
                        if !psnip.is_empty() {
                            t.snippet = psnip;
                        }
                    }
                    if ev.get("unread").is_some() && !b(ev, "unread") {
                        t.unread = false;
                        if pts > t.read_up_to {
                            t.read_up_to = pts;
                        }
                    }
                    if let Some(db) = &self.db {
                        db.save_thread(t);
                    }
                    return;
                }
                let name = s(ev, "name");
                if !name.is_empty() {
                    t.name = name;
                }
                let pic = s(ev, "picture");
                if !pic.is_empty() {
                    t.picture = pic;
                }
                let ev_ts = f(ev, "lastActivity");
                let snip = s(ev, "snippet");
                if ev_ts >= t.last_activity || t.last_activity == 0.0 {
                    if ev_ts > 0.0 {
                        t.last_activity = ev_ts;
                    }
                    if !snip.is_empty() {
                        t.snippet = snip;
                    }
                }
                if ev.get("muted").is_some() {
                    t.muted = b(ev, "muted");
                }
                if let Some(c) = opt(ev, "contact") {
                    t.contact = Some(c);
                }
                // encrypted-thread events from WhatsApp carry no folder/type: keep what we had
                if from_lightspeed {
                    t.folder = s(ev, "folder");
                    t.ttype = ev.get("ttype").and_then(Value::as_i64).unwrap_or(t.ttype);
                    t.parent = s(ev, "parent");
                }
                let ev_unread = b(ev, "unread");
                let ev_read_up_to = f(ev, "readUpTo");
                if ev_read_up_to > t.synced_read_up_to {
                    t.synced_read_up_to = ev_read_up_to;
                }
                if ev_read_up_to > t.read_up_to {
                    t.read_up_to = ev_read_up_to;
                }
                if !ev_unread {
                    if ev_ts > 0.0 {
                        t.read_up_to = t.read_up_to.max(ev_ts);
                        if from_lightspeed {
                            t.synced_read_up_to = t.synced_read_up_to.max(ev_ts);
                        }
                    }
                    if t.last_activity <= t.read_up_to + 999.0 {
                        t.unread = false;
                    }
                } else {
                    let check_ts = if ev_ts > 0.0 { ev_ts } else { t.last_activity };
                    if t.read_up_to > 0.0 && check_ts <= t.read_up_to + 999.0 {
                        // Already read locally; keep unread=false and mark server out-of-sync so App re-sends markReadServer.
                        t.unread = false;
                        if from_lightspeed && ev_read_up_to < t.read_up_to {
                            t.synced_read_up_to = ev_read_up_to;
                        }
                    } else {
                        t.unread = true;
                    }
                }
                if let Some(db) = &self.db {
                    db.save_thread(t);
                }
            }
            "community" => {
                let id = s(ev, "id");
                let c = Community {
                    name: s(ev, "name"),
                    description: s(ev, "description"),
                    picture: s(ev, "picture"),
                    members: ev.get("members").and_then(Value::as_i64).unwrap_or(0),
                };
                if !id.is_empty() && !c.name.is_empty() {
                    if let Some(db) = &self.db {
                        db.save_community(&id, &c);
                    }
                    self.communities.insert(id, c);
                }
            }
            "participant" => {
                // 1:1 threads: remember the other person so the thread can borrow name/avatar
                let th = self.canonical_thread(&s(ev, "thread"));
                let c = s(ev, "contact");
                if c != self.self_id && !c.is_empty() {
                    if let Some(t) = self.threads.get_mut(&th) {
                        let is_group = matches!(t.ttype, 2 | 8 | 11 | 16) || (t.ttype >= 17 && t.ttype <= 26);
                        if t.contact.is_none() && !is_group {
                            t.contact = Some(c);
                        }
                    }
                }
            }
            "message" => {
                let thread = self.canonical_thread(&s(ev, "thread"));
                let reply_text = opt(ev, "replyToText")
                    .or_else(|| opt(ev, "replyToId").and_then(|rid| self.resolve_reply_text(&thread, &rid)));
                let mut m = Message {
                    id: s(ev, "id"),
                    sender: s(ev, "sender"),
                    text: s(ev, "text"),
                    ts: f(ev, "ts"),
                    system: b(ev, "system"),
                    reply_text,
                    ..Default::default()
                };
                // the media event usually arrives before its message
                if let Some((k, r)) = self.media.get(&m.id) {
                    m.media_kind = Some(k.clone());
                    m.media_ref = Some(r.clone());
                }
                m.link = self.links.get(&m.id).cloned();
                let from_other = !self.self_id.is_empty() && m.sender != self.self_id;
                if let Some(db) = &self.db {
                    db.save_message(&thread, &m);
                }
                let (live, text, sender, msg_ts) = (b(ev, "live"), m.text.clone(), m.sender.clone(), m.ts);
                // Own message (from this app or our phone) implies we have read up to msg_ts.
                if !self.self_id.is_empty() && m.sender == self.self_id && msg_ts > 0.0 {
                    if let Some(t) = self.threads.get_mut(&thread) {
                        t.read_up_to = t.read_up_to.max(msg_ts);
                        if t.last_activity <= t.read_up_to + 999.0 {
                            t.unread = false;
                        }
                        if let Some(db) = &self.db {
                            db.save_thread(t);
                        }
                    }
                }
                // the chat list preview and ordering follow the newest message
                self.bump_preview(&thread, &sender, &text, msg_ts);
                // Not opened yet: the DB copy is enough, history is loaded on open.
                if let Some(list) = self.messages.get_mut(&thread) {
                    if let Some(old) = list.iter_mut().find(|x| x.id == m.id) {
                        let (r, mk, mr) = (std::mem::take(&mut old.reactions), old.media_kind.take(), old.media_ref.take());
                        let (nk, nr, nl) = (m.media_kind.clone(), m.media_ref.clone(), m.link.clone());
                        let old_link = old.link.take();
                        let old_reply = old.reply_text.take();
                        let old_text = std::mem::take(&mut old.text);
                        *old = m;
                        if old.text.is_empty() && !old_text.is_empty() {
                            old.text = old_text;
                        }
                        old.reply_text = old.reply_text.take().or(old_reply);
                        old.reactions = r;
                        old.media_kind = nk.or(mk);
                        old.media_ref = nr.or(mr);
                        old.link = nl.or(old_link);
                    } else {
                        list.push(m);
                        list.sort_by(|a, b| a.ts.total_cmp(&b.ts).then_with(|| a.id.cmp(&b.id)));
                    }
                }
                if live && from_other {
                    let already_read = self
                        .threads
                        .get(&thread)
                        .map(|t| t.read_up_to > 0.0 && msg_ts > 0.0 && msg_ts <= t.read_up_to + 999.0)
                        .unwrap_or(false);
                    if !already_read {
                        let muted = self.threads.get(&thread).map(|t| t.muted).unwrap_or(false);
                        if let Some(t) = self.threads.get_mut(&thread) {
                            t.unread = true;
                            if let Some(db) = &self.db {
                                db.save_thread(t);
                            }
                        }
                        if !muted && !text.is_empty() {
                            let title = self
                                .threads
                                .get(&thread)
                                .map(|t| self.display_name(t))
                                .unwrap_or_else(|| self.sender_name(&sender));
                            self.notifications.push((thread.clone(), title, text));
                        }
                    }
                }
            }
            "linkurl" => {
                // just the url of a link card (from its CTA row)
                let (id, url) = (s(ev, "id"), s(ev, "url"));
                if !url.is_empty() {
                    let l = self.links.entry(id.clone()).or_default();
                    l.url = url.clone();
                    let json = l.to_json();
                    if let Some(db) = &self.db {
                        db.set_link(&id, &json);
                    }
                    let snapshot = l.clone();
                    for list in self.messages.values_mut() {
                        if let Some(m) = list.iter_mut().find(|m| m.id == id) {
                            m.link = Some(snapshot.clone());
                        }
                    }
                }
            }
            "link" => {
                let id = s(ev, "id");
                let l = LinkPreview {
                    title: s(ev, "title"),
                    subtitle: s(ev, "subtitle"),
                    description: s(ev, "description"),
                    source: s(ev, "source"),
                    url: s(ev, "url"),
                    image: s(ev, "image"),
                };
                let mut l = l;
                // the CTA row that carries the url may have arrived first
                if l.url.is_empty() {
                    if let Some(old) = self.links.get(&id) {
                        l.url = old.url.clone();
                    }
                }
                // earlier versions stored a link's preview picture as a plain image: drop that
                if self.media.remove(&id).is_some() {
                    if let Some(db) = &self.db {
                        db.delete_media(&id);
                    }
                }
                if let Some(db) = &self.db {
                    db.set_link(&id, &l.to_json());
                }
                for list in self.messages.values_mut() {
                    if let Some(m) = list.iter_mut().find(|m| m.id == id) {
                        m.media_kind = None;
                        m.media_ref = None;
                        m.link = Some(l.clone());
                    }
                }
                self.links.insert(id, l);
            }
            "media" => {
                let (id, kind) = (s(ev, "id"), s(ev, "kind"));
                let r = opt(ev, "url").or_else(|| opt(ev, "path"));
                if let Some(r) = r {
                    self.media.insert(id.clone(), (kind.clone(), r.clone()));
                    if let Some(db) = &self.db {
                        db.set_media(&id, &kind, &r);
                    }
                    for list in self.messages.values_mut() {
                        if let Some(m) = list.iter_mut().find(|m| m.id == id) {
                            m.media_kind = Some(kind.clone());
                            m.media_ref = Some(r.clone());
                        }
                    }
                }
            }
            "sent" => {
                // E2EE send reconciliation: swap the local-echo tag for the real id
                let (tag, id) = (s(ev, "clientTag"), s(ev, "id"));
                let th = self.canonical_thread(&s(ev, "thread"));
                if let Some(list) = self.messages.get_mut(&th) {
                    let real_exists = list.iter().any(|m| m.id == id);
                    if real_exists {
                        list.retain(|m| m.id != tag);
                    } else if let Some(m) = list.iter_mut().find(|m| m.id == tag) {
                        m.id = id.clone();
                        m.pending = false;
                        if let Some(db) = &self.db {
                            db.save_message(&th, m);
                        }
                    }
                }
            }
            "reaction" => {
                let (id, emoji, actor) = (s(ev, "id"), s(ev, "emoji"), s(ev, "actor"));
                if let Some(db) = &self.db {
                    db.set_reaction(&id, &actor, &emoji);
                }
                for list in self.messages.values_mut() {
                    if let Some(m) = list.iter_mut().find(|m| m.id == id) {
                        m.reactions.retain(|(a, _)| *a != actor);
                        if !emoji.is_empty() {
                            m.reactions.push((actor.clone(), emoji.clone()));
                        }
                    }
                }
            }
            "edit" => {
                let (id, text) = (s(ev, "id"), s(ev, "text"));
                if let Some(db) = &self.db {
                    db.edit_message(&id, &text);
                }
                for list in self.messages.values_mut() {
                    if let Some(m) = list.iter_mut().find(|m| m.id == id) {
                        m.text = text.clone();
                    }
                }
            }
            "delete" => {
                // by id across all threads: the thread key can differ between normal and E2EE forms
                let id = s(ev, "id");
                let prefix = format!("{id}#");
                if let Some(db) = &self.db {
                    db.delete_message(&id);
                }
                self.media.retain(|k, _| k != &id && !k.starts_with(&prefix));
                for list in self.messages.values_mut() {
                    list.retain(|m| m.id != id && !m.id.starts_with(&prefix));
                }
            }
            "typing" => {
                let th = self.canonical_thread(&s(ev, "thread"));
                self.typing.insert(th, b(ev, "composing"));
            }
            _ => {}
        }
    }

    /// Update a thread's preview line and activity time when a newer message arrives
    /// (the server only re-sends the thread row occasionally).
    pub fn bump_preview(&mut self, thread: &str, sender: &str, text: &str, ts: f64) {
        if text.is_empty() {
            return;
        }
        let Some(t) = self.threads.get(thread) else { return };
        if ts <= t.last_activity {
            return;
        }
        let group = t.contact.is_none();
        let snippet = if sender == self.self_id {
            format!("Bạn: {text}")
        } else if group {
            let who = self
                .first_names
                .get(sender)
                .cloned()
                .or_else(|| self.contacts.get(sender).map(|n| n.split_whitespace().next().unwrap_or("").to_string()))
                .unwrap_or_default();
            if who.is_empty() { text.to_string() } else { format!("{who}: {text}") }
        } else {
            text.to_string()
        };
        if let Some(t) = self.threads.get_mut(thread) {
            t.last_activity = ts;
            t.snippet = snippet;
            if let Some(db) = &self.db {
                db.save_thread(t);
            }
        }
    }

    pub fn add_local_echo(&mut self, thread: &str, tag: &str, text: &str) {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as f64)
            .unwrap_or(0.0);
        self.ensure_loaded(thread);
        let me = self.self_id.clone();
        self.bump_preview(thread, &me, text, ts);
        self.messages.entry(thread.to_string()).or_default().push(Message {
            id: tag.to_string(),
            sender: self.self_id.clone(),
            text: text.to_string(),
            ts,
            pending: true,
            ..Default::default()
        });
    }

    pub fn display_name(&self, t: &Thread) -> String {
        if !t.name.is_empty() {
            return t.name.clone();
        }
        let key = t.contact.clone().unwrap_or_else(|| t.id.trim_start_matches("e:").to_string());
        self.contacts.get(&key).cloned().unwrap_or(key)
    }

    pub fn thread_avatar(&self, t: &Thread) -> Option<String> {
        if !t.picture.is_empty() {
            return Some(t.picture.clone());
        }
        let key = t.contact.clone().unwrap_or_else(|| t.id.trim_start_matches("e:").to_string());
        self.avatars.get(&key).cloned()
    }

    pub fn sender_name(&self, id: &str) -> String {
        if id == self.self_id {
            return "Bạn".into();
        }
        self.contacts.get(id).cloned().unwrap_or_else(|| id.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn read_watermark_survives_stale_server_reload_and_1s_wa_timestamps() {
        let db = Db::open_memory().unwrap();
        let mut st = Store::with_db(Some(db));
        st.self_id = "999".into();

        // Incoming unread E2EE thread from Lightspeed (ms precision)
        st.apply(&json!({
            "type": "thread", "id": "e:100", "name": "Alice",
            "snippet": "hello", "lastActivity": 1700000005432.0,
            "unread": true, "readUpTo": 1700000000000.0, "folder": "inbox"
        }));
        assert!(st.threads["e:100"].unread);

        // User reads up to 1700000005000.0 (1-second truncated WhatsApp timestamp)
        st.apply(&json!({"type": "read", "thread": "e:100", "readUpTo": 1700000005000.0}));
        assert!(!st.threads["e:100"].unread, "999ms WA tolerance must clear unread");

        // Server re-sends stale unread=true with old readUpTo=1700000000000 on restart
        st.apply(&json!({
            "type": "thread", "id": "e:100", "name": "Alice",
            "snippet": "hello", "lastActivity": 1700000005432.0,
            "unread": true, "readUpTo": 1700000000000.0, "folder": "inbox"
        }));
        let t = &st.threads["e:100"];
        assert!(!t.unread, "already-read thread must stay read after restart");
        assert!(t.read_up_to > t.synced_read_up_to + 999.0, "must flag server watermark as needing re-sync");
    }

    #[test]
    fn split_plain_and_e2ee_threads_merge_without_losing_messages() {
        let db = Db::open_memory().unwrap();
        let mut st = Store::with_db(Some(db));
        st.self_id = "999".into();

        // Plain thread arrives before E2EE mapping is known
        st.apply(&json!({
            "type": "thread", "id": "100", "name": "Bob",
            "snippet": "old", "lastActivity": 1000.0, "unread": false, "folder": "inbox"
        }));
        st.apply(&json!({
            "type": "message", "id": "m1", "thread": "100", "sender": "100",
            "text": "from plain", "ts": 1000.0, "live": false
        }));

        // E2EE thread arrives and merges plain "100" -> "e:100"
        st.apply(&json!({
            "type": "thread", "id": "e:100", "name": "",
            "snippet": "encrypted", "lastActivity": 2000.0, "unread": true, "e2ee": true
        }));
        st.apply(&json!({
            "type": "message", "id": "m2", "thread": "e:100", "sender": "100",
            "text": "from e2ee", "ts": 2000.0, "live": true
        }));

        assert!(!st.threads.contains_key("100"));
        assert_eq!(st.threads["e:100"].name, "Bob");
        st.ensure_loaded("e:100");
        let ids: Vec<&str> = st.messages["e:100"].iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, vec!["m1", "m2"]);
    }

    #[test]
    fn same_timestamp_pagination_and_multipart_delete() {
        let db = Db::open_memory().unwrap();
        let mut st = Store::with_db(Some(db));
        st.self_id = "999".into();

        for i in 0..5 {
            st.apply(&json!({
                "type": "message",
                "id": format!("m{i}"),
                "thread": "t1",
                "sender": "100",
                "text": format!("msg {i}"),
                "ts": 5000.0, // identical 1-second timestamp
                "live": false
            }));
        }
        // Load only 2 newest, then paginate older from DB at boundary ts=5000.0
        let initial = st.db.as_ref().unwrap().load_messages("t1", 2);
        st.messages.insert("t1".into(), initial);
        let added = st.load_older_from_db("t1");
        assert_eq!(added, 3, "messages sharing boundary timestamp must not be skipped");
        assert_eq!(st.messages["t1"].len(), 5);

        // Multi-part gallery message deletion removes all #k children
        st.apply(&json!({"type": "message", "id": "gal", "thread": "t1", "sender": "100", "text": "📷 Photo", "ts": 6000.0}));
        st.apply(&json!({"type": "message", "id": "gal#1", "thread": "t1", "sender": "100", "text": "📷 Photo", "ts": 6000.001}));
        st.apply(&json!({"type": "delete", "id": "gal"}));
        assert!(!st.messages["t1"].iter().any(|m| m.id.starts_with("gal")));
    }
}
