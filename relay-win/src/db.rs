//! SQLite persistence. The helper keeps no history and E2EE messages are
//! delivered only once, so everything the UI sees is written here.
use crate::model::{Message, Thread};
use rusqlite::{params, Connection};
use std::collections::HashMap;

pub struct Db {
    conn: Connection,
}

impl Db {
    pub fn open() -> rusqlite::Result<Self> {
        let conn = Connection::open(crate::vault::data_dir().join("relay.db"))?;
        // lower-casing that also folds Vietnamese capitals (SQLite's own lower() is ASCII only)
        conn.create_scalar_function(
            "ulower",
            1,
            rusqlite::functions::FunctionFlags::SQLITE_UTF8 | rusqlite::functions::FunctionFlags::SQLITE_DETERMINISTIC,
            |ctx| Ok(ctx.get::<Option<String>>(0)?.unwrap_or_default().to_lowercase()),
        )?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;
             CREATE TABLE IF NOT EXISTS messages(
               id TEXT PRIMARY KEY, thread TEXT NOT NULL, sender TEXT, text TEXT, ts REAL,
               system INTEGER, reply_text TEXT, media_kind TEXT, media_ref TEXT);
             CREATE INDEX IF NOT EXISTS msg_thread ON messages(thread, ts);
             CREATE TABLE IF NOT EXISTS threads(
               id TEXT PRIMARY KEY, name TEXT, snippet TEXT, picture TEXT, last_activity REAL,
               unread INTEGER, muted INTEGER, contact TEXT);
             CREATE TABLE IF NOT EXISTS contacts(id TEXT PRIMARY KEY, name TEXT, avatar TEXT);
             CREATE TABLE IF NOT EXISTS settings(key TEXT PRIMARY KEY, value TEXT);
             CREATE TABLE IF NOT EXISTS media(id TEXT PRIMARY KEY, kind TEXT, ref TEXT);
             CREATE TABLE IF NOT EXISTS links(id TEXT PRIMARY KEY, json TEXT);
             CREATE TABLE IF NOT EXISTS stickers(id TEXT PRIMARY KEY, url TEXT, seen REAL);
             CREATE TABLE IF NOT EXISTS reactions(msg TEXT, actor TEXT, emoji TEXT, PRIMARY KEY(msg, actor));
             CREATE TABLE IF NOT EXISTS communities(id TEXT PRIMARY KEY, name TEXT, description TEXT, picture TEXT, members INTEGER);",
        )?;
        // pictures are no longer kept on disk: forget rows that pointed into the old cache so the
        // messages fetch a fresh picture link from Facebook when they are opened again
        let _ = conn.execute("DELETE FROM media WHERE ref LIKE '%media-cache%'", []);
        // columns added later (errors = column already there)
        for sql in [
            "ALTER TABLE threads ADD COLUMN ttype INTEGER",
            "ALTER TABLE threads ADD COLUMN parent TEXT",
            "ALTER TABLE threads ADD COLUMN folder TEXT",
        ] {
            let _ = conn.execute(sql, []);
        }
        Ok(Self { conn })
    }

    /// Newest messages (thread, text, ts) whose text contains `q`, any letter case.
    pub fn search_messages(&self, q: &str, limit: usize) -> Vec<(String, String, f64)> {
        let like = format!("%{}%", q.to_lowercase().replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_"));
        let mut out = Vec::new();
        if let Ok(mut st) = self.conn.prepare("SELECT thread,text,ts FROM messages WHERE system=0 AND ulower(text) LIKE ?1 ESCAPE '\\' ORDER BY ts DESC LIMIT ?2") {
            if let Ok(rows) = st.query_map(params![like, limit as i64], |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?.unwrap_or_default(), r.get::<_, Option<f64>>(2)?.unwrap_or(0.0)))) {
                out.extend(rows.flatten());
            }
        }
        out
    }

    pub fn save_sticker(&self, id: &str, url: &str, seen: f64) {
        let _ = self.conn.execute("INSERT OR REPLACE INTO stickers(id,url,seen) VALUES(?1,?2,?3)", params![id, url, seen]);
    }
    /// Stickers seen in chats, newest first: (sticker id, picture address).
    pub fn load_stickers(&self, limit: usize) -> Vec<(String, String)> {
        let mut out = Vec::new();
        if let Ok(mut st) = self.conn.prepare("SELECT id,url FROM stickers ORDER BY seen DESC LIMIT ?1") {
            if let Ok(rows) = st.query_map(params![limit as i64], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))) {
                out.extend(rows.flatten());
            }
        }
        out
    }

    pub fn get_setting(&self, key: &str) -> Option<String> {
        self.conn.query_row("SELECT value FROM settings WHERE key=?1", params![key], |r| r.get(0)).ok()
    }
    pub fn set_setting(&self, key: &str, value: &str) {
        let _ = self.conn.execute("INSERT OR REPLACE INTO settings(key,value) VALUES(?1,?2)", params![key, value]);
    }

    pub fn save_message(&self, thread: &str, m: &Message) {
        let _ = self.conn.execute(
            "INSERT INTO messages(id,thread,sender,text,ts,system,reply_text,media_kind,media_ref)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)
             ON CONFLICT(id) DO UPDATE SET text=excluded.text, ts=excluded.ts,
               reply_text=excluded.reply_text,
               media_kind=COALESCE(excluded.media_kind, media_kind),
               media_ref=COALESCE(excluded.media_ref, media_ref)",
            params![m.id, thread, m.sender, m.text, m.ts, m.system, m.reply_text, m.media_kind, m.media_ref],
        );
    }
    /// Media can arrive before its message row exists, so it lives in its own table.
    pub fn set_media(&self, id: &str, kind: &str, r: &str) {
        let _ = self.conn.execute("INSERT OR REPLACE INTO media(id,kind,ref) VALUES(?1,?2,?3)", params![id, kind, r]);
    }
    pub fn delete_media(&self, id: &str) {
        let _ = self.conn.execute("DELETE FROM media WHERE id=?1", params![id]);
    }
    pub fn set_link(&self, id: &str, json: &str) {
        let _ = self.conn.execute("INSERT OR REPLACE INTO links(id,json) VALUES(?1,?2)", params![id, json]);
    }
    pub fn load_links(&self) -> Vec<(String, String)> {
        let mut out = Vec::new();
        if let Ok(mut st) = self.conn.prepare("SELECT id,json FROM links") {
            if let Ok(rows) = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))) {
                out.extend(rows.flatten());
            }
        }
        out
    }
    pub fn load_media(&self) -> Vec<(String, String, String)> {
        let mut out = Vec::new();
        if let Ok(mut st) = self.conn.prepare("SELECT id,kind,ref FROM media") {
            if let Ok(rows) = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?))) {
                out.extend(rows.flatten());
            }
        }
        out
    }
    pub fn edit_message(&self, id: &str, text: &str) {
        let _ = self.conn.execute("UPDATE messages SET text=?2 WHERE id=?1", params![id, text]);
    }
    pub fn delete_message(&self, id: &str) {
        let _ = self.conn.execute("DELETE FROM messages WHERE id=?1", params![id]);
        let _ = self.conn.execute("DELETE FROM media WHERE id=?1", params![id]);
        let _ = self.conn.execute("DELETE FROM reactions WHERE msg=?1", params![id]);
    }
    /// Remember (or, with an empty emoji, forget) one person's reaction to a message.
    pub fn set_reaction(&self, msg: &str, actor: &str, emoji: &str) {
        if emoji.is_empty() {
            let _ = self.conn.execute("DELETE FROM reactions WHERE msg=?1 AND actor=?2", params![msg, actor]);
        } else {
            let _ = self.conn.execute("INSERT OR REPLACE INTO reactions(msg,actor,emoji) VALUES(?1,?2,?3)", params![msg, actor, emoji]);
        }
    }
    /// (message id, actor, emoji) of every stored reaction in a thread.
    pub fn load_reactions(&self, thread: &str) -> Vec<(String, String, String)> {
        let mut out = Vec::new();
        if let Ok(mut st) = self.conn.prepare("SELECT r.msg,r.actor,r.emoji FROM reactions r JOIN messages m ON m.id=r.msg WHERE m.thread=?1") {
            if let Ok(rows) = st.query_map(params![thread], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?))) {
                out.extend(rows.flatten());
            }
        }
        out
    }
    pub fn save_thread(&self, t: &Thread) {
        let _ = self.conn.execute(
            "INSERT OR REPLACE INTO threads(id,name,snippet,picture,last_activity,unread,muted,contact,ttype,parent,folder)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
            params![t.id, t.name, t.snippet, t.picture, t.last_activity, t.unread, t.muted, t.contact, t.ttype, t.parent, t.folder],
        );
    }
    pub fn delete_thread(&self, id: &str) {
        let _ = self.conn.execute("DELETE FROM threads WHERE id=?1", params![id]);
        let _ = self.conn.execute("DELETE FROM reactions WHERE msg IN (SELECT id FROM messages WHERE thread=?1)", params![id]);
        let _ = self.conn.execute("DELETE FROM messages WHERE thread=?1", params![id]);
    }
    pub fn save_contact(&self, id: &str, name: &str, avatar: &str) {
        let _ = self.conn.execute(
            "INSERT OR REPLACE INTO contacts(id,name,avatar) VALUES(?1,?2,?3)",
            params![id, name, avatar],
        );
    }
    pub fn begin(&self) {
        let _ = self.conn.execute_batch("BEGIN");
    }
    pub fn commit(&self) {
        let _ = self.conn.execute_batch("COMMIT");
    }

    pub fn load_threads(&self) -> Vec<Thread> {
        let mut st = match self.conn.prepare(
            "SELECT id,name,snippet,picture,last_activity,unread,muted,contact,ttype,parent,folder FROM threads",
        ) {
            Ok(s) => s,
            Err(_) => return vec![],
        };
        st.query_map([], |r| {
            Ok(Thread {
                id: r.get(0)?,
                name: r.get::<_, Option<String>>(1)?.unwrap_or_default(),
                snippet: r.get::<_, Option<String>>(2)?.unwrap_or_default(),
                picture: r.get::<_, Option<String>>(3)?.unwrap_or_default(),
                last_activity: r.get::<_, Option<f64>>(4)?.unwrap_or(0.0),
                unread: r.get::<_, Option<bool>>(5)?.unwrap_or(false),
                muted: r.get::<_, Option<bool>>(6)?.unwrap_or(false),
                contact: r.get(7)?,
                ttype: r.get::<_, Option<i64>>(8)?.unwrap_or(0),
                parent: r.get::<_, Option<String>>(9)?.unwrap_or_default(),
                folder: r.get::<_, Option<String>>(10)?.unwrap_or_default(),
            })
        })
        .map(|it| it.filter_map(Result::ok).collect())
        .unwrap_or_default()
    }

    pub fn save_community(&self, id: &str, c: &crate::model::Community) {
        let _ = self.conn.execute(
            "INSERT OR REPLACE INTO communities(id,name,description,picture,members) VALUES(?1,?2,?3,?4,?5)",
            params![id, c.name, c.description, c.picture, c.members],
        );
    }
    pub fn load_communities(&self) -> Vec<(String, crate::model::Community)> {
        let mut out = Vec::new();
        if let Ok(mut st) = self.conn.prepare("SELECT id,name,description,picture,members FROM communities") {
            if let Ok(rows) = st.query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    crate::model::Community {
                        name: r.get::<_, Option<String>>(1)?.unwrap_or_default(),
                        description: r.get::<_, Option<String>>(2)?.unwrap_or_default(),
                        picture: r.get::<_, Option<String>>(3)?.unwrap_or_default(),
                        members: r.get::<_, Option<i64>>(4)?.unwrap_or(0),
                    },
                ))
            }) {
                out.extend(rows.flatten());
            }
        }
        out
    }

    pub fn load_contacts(&self) -> HashMap<String, (String, String)> {
        let mut out = HashMap::new();
        if let Ok(mut st) = self.conn.prepare("SELECT id,name,avatar FROM contacts") {
            if let Ok(rows) = st.query_map([], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?.unwrap_or_default(), r.get::<_, Option<String>>(2)?.unwrap_or_default()))
            }) {
                for (id, n, a) in rows.flatten() {
                    out.insert(id, (n, a));
                }
            }
        }
        out
    }

    /// Latest `limit` messages of a thread, oldest first.
    pub fn load_messages(&self, thread: &str, limit: usize) -> Vec<Message> {
        self.load_messages_before(thread, f64::MAX, limit)
    }

    /// The `limit` messages of a thread just older than `before`, oldest first.
    pub fn load_messages_before(&self, thread: &str, before: f64, limit: usize) -> Vec<Message> {
        let mut st = match self.conn.prepare(
            "SELECT id,sender,text,ts,system,reply_text,media_kind,media_ref FROM messages
             WHERE thread=?1 AND ts<?3 ORDER BY ts DESC LIMIT ?2",
        ) {
            Ok(s) => s,
            Err(_) => return vec![],
        };
        let mut v: Vec<Message> = st
            .query_map(params![thread, limit as i64, before], |r| {
                Ok(Message {
                    id: r.get(0)?,
                    sender: r.get::<_, Option<String>>(1)?.unwrap_or_default(),
                    text: r.get::<_, Option<String>>(2)?.unwrap_or_default(),
                    ts: r.get::<_, Option<f64>>(3)?.unwrap_or(0.0),
                    system: r.get::<_, Option<bool>>(4)?.unwrap_or(false),
                    reply_text: r.get(5)?,
                    media_kind: r.get(6)?,
                    media_ref: r.get(7)?,
                    reactions: Vec::new(),
                    pending: false,
                    link: None,
                })
            })
            .map(|it| it.filter_map(Result::ok).collect())
            .unwrap_or_default();
        v.reverse();
        v
    }
}
