//! Local SQLite store for the News app. Everything lives on this machine —
//! the only outbound traffic is fetching the user's own feeds and the SenClaw
//! LLM bridge. Tables:
//!   * `sources`        — feed RSS/Atom, hoặc trang thường được quét link
//!                        (`kind` = feed|scrape), kèm ETag/Last-Modified + health
//!   * `articles`       — tin đã thu thập, dedup theo hash(url|guid)
//!   * `articles_fts`   — FTS5 mirror (title/description/content,
//!                        unicode61 remove_diacritics 2 → tìm không dấu được)
//!   * `topics`         — chủ đề do người dùng định nghĩa (keyword CSV)
//!   * `article_topics` — gán bài ↔ chủ đề (tự động theo keyword)
//!   * `stories`        — dòng sự kiện (chuỗi tin liên quan) + token profile
//!   * `analyses`       — cache kết quả AI đánh giá từng bài
//!   * `activity`, `settings`
//!
//! Story membership is DERIVED once at insert time (crate::cluster) and stored
//! — reclustering history on every read would make story ids unstable, and
//! stable ids is what the timeline UI + MCP tools need.

use crate::cluster::{self, StoryProfile};
use anyhow::{anyhow, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

pub struct Db {
    conn: Mutex<Connection>,
}

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS sources (
  id            INTEGER PRIMARY KEY AUTOINCREMENT,
  name          TEXT NOT NULL,
  url           TEXT NOT NULL UNIQUE,
  category      TEXT NOT NULL DEFAULT '',
  lang          TEXT NOT NULL DEFAULT '',
  -- 'feed'   — url is an RSS/Atom document (the default, cheapest, most exact)
  -- 'scrape' — url is an ordinary listing/category page whose article links are
  --            harvested from the HTML, for sites that publish no feed
  kind          TEXT NOT NULL DEFAULT 'feed',
  status        TEXT NOT NULL DEFAULT 'active',
  etag          TEXT NOT NULL DEFAULT '',
  last_modified TEXT NOT NULL DEFAULT '',
  last_fetch_at INTEGER NOT NULL DEFAULT 0,
  last_status   TEXT NOT NULL DEFAULT '',
  last_error    TEXT NOT NULL DEFAULT '',
  note          TEXT NOT NULL DEFAULT '',
  created_at    INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS articles (
  id           INTEGER PRIMARY KEY AUTOINCREMENT,
  source_id    INTEGER NOT NULL,
  hash         TEXT NOT NULL UNIQUE,
  guid         TEXT NOT NULL DEFAULT '',
  url          TEXT NOT NULL,
  title        TEXT NOT NULL,
  description  TEXT NOT NULL DEFAULT '',
  content      TEXT NOT NULL DEFAULT '',
  image_url    TEXT NOT NULL DEFAULT '',
  author       TEXT NOT NULL DEFAULT '',
  category     TEXT NOT NULL DEFAULT '',
  story_id     INTEGER,
  published_at INTEGER NOT NULL,
  fetched_at   INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_articles_pub    ON articles(published_at DESC);
CREATE INDEX IF NOT EXISTS idx_articles_source ON articles(source_id);
CREATE INDEX IF NOT EXISTS idx_articles_story  ON articles(story_id);
CREATE VIRTUAL TABLE IF NOT EXISTS articles_fts USING fts5(
  article_id UNINDEXED,
  title,
  description,
  content,
  tokenize='unicode61 remove_diacritics 2'
);
CREATE TABLE IF NOT EXISTS topics (
  id         INTEGER PRIMARY KEY AUTOINCREMENT,
  name       TEXT NOT NULL,
  keywords   TEXT NOT NULL DEFAULT '',
  color      TEXT NOT NULL DEFAULT '',
  created_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS article_topics (
  article_id INTEGER NOT NULL,
  topic_id   INTEGER NOT NULL,
  PRIMARY KEY (article_id, topic_id)
);
CREATE TABLE IF NOT EXISTS stories (
  id            INTEGER PRIMARY KEY AUTOINCREMENT,
  title         TEXT NOT NULL,
  profile       TEXT NOT NULL DEFAULT '{}',
  first_at      INTEGER NOT NULL,
  last_at       INTEGER NOT NULL,
  article_count INTEGER NOT NULL DEFAULT 0,
  summary       TEXT NOT NULL DEFAULT '',
  summary_model TEXT NOT NULL DEFAULT '',
  summary_at    INTEGER NOT NULL DEFAULT 0,
  facts         TEXT NOT NULL DEFAULT '{}'
);
CREATE INDEX IF NOT EXISTS idx_stories_last ON stories(last_at DESC);
-- Every AI brief ever run on a story, newest kept alive in stories.summary as
-- well. A story keeps developing, so re-summarising is normal and the older
-- readings stay worth going back to ("what did it look like on the 29th?").
CREATE TABLE IF NOT EXISTS story_summaries (
  id            INTEGER PRIMARY KEY AUTOINCREMENT,
  story_id      INTEGER NOT NULL,
  summary       TEXT NOT NULL,
  model         TEXT NOT NULL DEFAULT '',
  article_count INTEGER NOT NULL DEFAULT 0,
  last_at       INTEGER NOT NULL DEFAULT 0,
  created_at    INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_story_summaries ON story_summaries(story_id, created_at DESC);
-- How many headlines each phrase has appeared in, across every source and every
-- language. This is the clusterer's stopword list, measured instead of written:
-- whatever turns out to be everyday language for THESE feeds stops identifying
-- an event, whichever language it is in.
CREATE TABLE IF NOT EXISTS phrase_df (
  phrase TEXT PRIMARY KEY,
  df     INTEGER NOT NULL DEFAULT 0
);
-- Article text rendered into the reader's display language. Cached because a
-- translation costs an LLM call and the source text never changes; keyed by
-- language so switching back and forth is free after the first pass.
CREATE TABLE IF NOT EXISTS translations (
  article_id  INTEGER NOT NULL,
  lang        TEXT NOT NULL,
  title       TEXT NOT NULL DEFAULT '',
  description TEXT NOT NULL DEFAULT '',
  at          INTEGER NOT NULL,
  PRIMARY KEY (article_id, lang)
);
CREATE TABLE IF NOT EXISTS analyses (
  article_id  INTEGER PRIMARY KEY,
  summary     TEXT NOT NULL DEFAULT '',
  sentiment   TEXT NOT NULL DEFAULT '',
  importance  INTEGER NOT NULL DEFAULT 0,
  clickbait   INTEGER NOT NULL DEFAULT 0,
  reliability TEXT NOT NULL DEFAULT '',
  tags        TEXT NOT NULL DEFAULT '',
  model       TEXT NOT NULL DEFAULT '',
  created_at  INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS digests (
  id            INTEGER PRIMARY KEY AUTOINCREMENT,
  hours         INTEGER NOT NULL,
  focus         TEXT NOT NULL DEFAULT '',
  topic_id      INTEGER,
  topic_name    TEXT NOT NULL DEFAULT '',
  article_count INTEGER NOT NULL DEFAULT 0,
  text          TEXT NOT NULL,
  model         TEXT NOT NULL DEFAULT '',
  truncated     INTEGER NOT NULL DEFAULT 0,
  created_at    INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_digests_at ON digests(created_at DESC);
CREATE TABLE IF NOT EXISTS activity (
  id         INTEGER PRIMARY KEY AUTOINCREMENT,
  kind       TEXT NOT NULL,
  message    TEXT NOT NULL,
  ref_id     TEXT NOT NULL DEFAULT '',
  created_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS settings (
  key   TEXT PRIMARY KEY,
  value TEXT NOT NULL
);
-- Links a scrape source offered that turned out NOT to be article pages
-- (section indexes, policy pages…). Remembered so they are not re-opened every
-- single cycle: they stay on the listing page forever, and without this each
-- one costs a request per cycle and makes the source look permanently broken.
CREATE TABLE IF NOT EXISTS scrape_rejects (
  url       TEXT PRIMARY KEY,
  source_id INTEGER NOT NULL,
  at        INTEGER NOT NULL
);
"#;

pub fn now_ts() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub fn iso(ts: i64) -> String {
    chrono::DateTime::from_timestamp(ts, 0)
        .map(|d| d.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
        .unwrap_or_default()
}

/// Feeds seeded on first run — a starting kit the user can edit/delete freely.
///
/// Every URL here was validated against the live parser (2026-07-29); feeds
/// that 404, 403 the collector, or serve nothing were dropped rather than
/// shipped broken. Deliberately spread across Vietnam / world / Asia and across
/// domains, because the trend and story-graph features only work when several
/// independent outlets can cover the same event.
const SEED_SOURCES: &[(&str, &str, &str, &str)] = &[
    // Việt Nam
    (
        "VnExpress — Tin mới nhất",
        "https://vnexpress.net/rss/tin-moi-nhat.rss",
        "Tổng hợp",
        "vi",
    ),
    (
        "VnExpress — Thế giới",
        "https://vnexpress.net/rss/the-gioi.rss",
        "Thế giới",
        "vi",
    ),
    (
        "VnExpress — Kinh doanh",
        "https://vnexpress.net/rss/kinh-doanh.rss",
        "Kinh doanh",
        "vi",
    ),
    (
        "VnExpress — Số hóa",
        "https://vnexpress.net/rss/so-hoa.rss",
        "Công nghệ",
        "vi",
    ),
    (
        "Tuổi Trẻ — Tin mới",
        "https://tuoitre.vn/rss/tin-moi-nhat.rss",
        "Tổng hợp",
        "vi",
    ),
    (
        "Tuổi Trẻ — Thế giới",
        "https://tuoitre.vn/rss/the-gioi.rss",
        "Thế giới",
        "vi",
    ),
    (
        "Thanh Niên",
        "https://thanhnien.vn/rss/home.rss",
        "Tổng hợp",
        "vi",
    ),
    (
        "VietNamNet",
        "https://vietnamnet.vn/home.rss",
        "Tổng hợp",
        "vi",
    ),
    ("CafeF", "https://cafef.vn/home.rss", "Kinh doanh", "vi"),
    // Thế giới
    (
        "BBC — World",
        "https://feeds.bbci.co.uk/news/world/rss.xml",
        "Thế giới",
        "en",
    ),
    (
        "Al Jazeera",
        "https://www.aljazeera.com/xml/rss/all.xml",
        "Thế giới",
        "en",
    ),
    (
        "The Guardian — World",
        "https://www.theguardian.com/world/rss",
        "Thế giới",
        "en",
    ),
    (
        "NYT — World",
        "https://rss.nytimes.com/services/xml/rss/nyt/World.xml",
        "Thế giới",
        "en",
    ),
    (
        "NPR",
        "https://feeds.npr.org/1001/rss.xml",
        "Tổng hợp",
        "en",
    ),
    (
        "France 24",
        "https://www.france24.com/en/rss",
        "Thế giới",
        "en",
    ),
    (
        "Deutsche Welle",
        "https://rss.dw.com/rdf/rss-en-all",
        "Thế giới",
        "en",
    ),
    // Châu Á
    (
        "Channel NewsAsia",
        "https://www.channelnewsasia.com/api/v1/rss-outbound-feed?_format=xml",
        "Châu Á",
        "en",
    ),
    (
        "South China Morning Post",
        "https://www.scmp.com/rss/91/feed",
        "Châu Á",
        "en",
    ),
    (
        "The Japan Times",
        "https://www.japantimes.co.jp/feed/",
        "Châu Á",
        "en",
    ),
    // Kinh doanh / công nghệ / khoa học / thể thao
    (
        "CNBC",
        "https://search.cnbc.com/rs/search/combinedcms/view.xml?partnerId=wrss01&id=100003114",
        "Kinh doanh",
        "en",
    ),
    (
        "MarketWatch",
        "https://feeds.marketwatch.com/marketwatch/topstories/",
        "Kinh doanh",
        "en",
    ),
    (
        "The Verge",
        "https://www.theverge.com/rss/index.xml",
        "Công nghệ",
        "en",
    ),
    (
        "Ars Technica",
        "https://feeds.arstechnica.com/arstechnica/index",
        "Công nghệ",
        "en",
    ),
    (
        "TechCrunch",
        "https://techcrunch.com/feed/",
        "Công nghệ",
        "en",
    ),
    (
        "Hacker News",
        "https://hnrss.org/frontpage",
        "Công nghệ",
        "en",
    ),
    (
        "Science Daily",
        "https://www.sciencedaily.com/rss/all.xml",
        "Khoa học",
        "en",
    ),
    ("Phys.org", "https://phys.org/rss-feed/", "Khoa học", "en"),
    (
        "BBC — Sport",
        "https://feeds.bbci.co.uk/sport/rss.xml",
        "Thể thao",
        "en",
    ),
];

/// Example topics so the feature is discoverable; freely editable.
const SEED_TOPICS: &[(&str, &str, &str)] = &[
    (
        "Công nghệ & AI",
        "AI, trí tuệ nhân tạo, công nghệ, smartphone, chip, phần mềm, startup, robot",
        "blue",
    ),
    (
        "Kinh tế",
        "kinh tế, lạm phát, chứng khoán, giá vàng, tỷ giá, ngân hàng, xuất khẩu, GDP",
        "gold",
    ),
    (
        "Thể thao",
        "bóng đá, World Cup, SEA Games, V-League, tuyển Việt Nam, Olympic, tennis",
        "green",
    ),
];

/// Accept `""` (legacy callers) as `"feed"`; reject anything unknown so a typo
/// cannot create a source that silently never fetches.
pub fn normalize_kind(kind: &str) -> Result<&'static str> {
    match kind.trim() {
        "" | "feed" => Ok("feed"),
        "scrape" => Ok("scrape"),
        other => Err(anyhow!("kind phải là feed|scrape (nhận '{other}')")),
    }
}

/// Columns added after the first release. `CREATE TABLE IF NOT EXISTS` leaves
/// an existing table untouched, so every later column needs an explicit ALTER;
/// a duplicate-column error just means this DB already has it.
fn migrate(c: &Connection) {
    for sql in [
        "ALTER TABLE sources ADD COLUMN kind TEXT NOT NULL DEFAULT 'feed'",
        // Places / people / headline quantities a story has established. Old
        // rows get '{}' and are re-derived by the next regroup.
        "ALTER TABLE stories ADD COLUMN facts TEXT NOT NULL DEFAULT '{}'",
        // Chuyên mục do System One (Laya) chọn — xem laya.rs. Khác `category`,
        // vốn là chuyên mục do chính feed khai. `section_at` NULL = chưa phân loại.
        "ALTER TABLE articles ADD COLUMN section TEXT NOT NULL DEFAULT ''",
        "ALTER TABLE articles ADD COLUMN section_conf REAL NOT NULL DEFAULT 0",
        "ALTER TABLE articles ADD COLUMN section_at INTEGER",
        "CREATE INDEX IF NOT EXISTS idx_articles_section ON articles(section)",
    ] {
        let _ = c.execute(sql, []);
    }
}

impl Db {
    pub fn open(path: PathBuf) -> Result<Self> {
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.execute_batch(SCHEMA)?;
        migrate(&conn);
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    #[cfg(test)]
    pub fn open_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(SCHEMA)?;
        migrate(&conn);
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    pub fn open_default() -> Result<Self> {
        let dir = std::env::var("SENCLAW_DATA_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| {
                let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
                PathBuf::from(home)
                    .join(".senclaw")
                    .join("apps")
                    .join("news")
            });
        std::fs::create_dir_all(&dir).ok();
        let db = Self::open(dir.join("news.db"))?;
        db.seed_if_empty();
        let fixed = db.repair_entities();
        if fixed > 0 {
            println!("[news] sửa HTML entity còn sót trong {fixed} bài đã lưu");
        }
        Ok(db)
    }

    /// One-shot repair for rows stored before the entity table covered accented
    /// letters — those articles hold literal `&eacute;` in their text. Runs on
    /// every boot but only touches rows that still look encoded, so it is a
    /// no-op once clean. FTS rows are rewritten to match.
    pub fn repair_entities(&self) -> i64 {
        self.with(|c| {
            let mut st = c.prepare(
                "SELECT id,title,description,content FROM articles
                 WHERE title LIKE '%&%;%' OR description LIKE '%&%;%' OR content LIKE '%&%;%'",
            )?;
            let rows: Vec<(i64, String, String, String)> = st
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
                .flatten()
                .collect();
            drop(st);

            let mut n = 0i64;
            for (id, title, desc, content) in rows {
                let (nt, nd, nc) = (
                    crate::fetch::decode_entities(&title),
                    crate::fetch::decode_entities(&desc),
                    crate::fetch::decode_entities(&content),
                );
                if nt == title && nd == desc && nc == content {
                    continue;
                }
                c.execute(
                    "UPDATE articles SET title=?1,description=?2,content=?3 WHERE id=?4",
                    params![nt, nd, nc, id],
                )?;
                c.execute("DELETE FROM articles_fts WHERE article_id=?1", params![id])?;
                c.execute(
                    "INSERT INTO articles_fts(article_id,title,description,content)
                     SELECT id,title,description,content FROM articles WHERE id=?1",
                    params![id],
                )?;
                n += 1;
            }
            Ok(n)
        })
        .unwrap_or(0)
    }

    pub fn seed_if_empty(&self) {
        if self.list_sources(None).is_empty() {
            for (name, url, cat, lang) in SEED_SOURCES {
                let _ = self.add_source(name, url, cat, lang, "nguồn mặc định", "feed");
            }
        }
        if self.list_topics().is_empty() {
            for (name, keywords, color) in SEED_TOPICS {
                let _ = self.add_topic(name, keywords, color);
            }
        }
    }

    fn with<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        let conn = self.conn.lock().map_err(|_| anyhow!("db lock poisoned"))?;
        f(&conn)
    }

    /// Same guard, but hands out `&mut Connection` — needed for `transaction()`.
    fn with_mut<T>(&self, f: impl FnOnce(&mut Connection) -> Result<T>) -> Result<T> {
        let mut conn = self.conn.lock().map_err(|_| anyhow!("db lock poisoned"))?;
        f(&mut conn)
    }

    // ---- settings / activity ----

    pub fn setting(&self, key: &str, default: &str) -> String {
        self.with(|c| {
            Ok(c.query_row(
                "SELECT value FROM settings WHERE key=?1",
                params![key],
                |r| r.get::<_, String>(0),
            )
            .optional()?)
        })
        .ok()
        .flatten()
        .unwrap_or_else(|| default.to_string())
    }

    pub fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        self.with(|c| {
            c.execute(
                "INSERT INTO settings(key,value) VALUES(?1,?2)
                 ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                params![key, value],
            )?;
            Ok(())
        })
    }

    pub fn log(&self, kind: &str, message: &str, ref_id: &str) {
        let _ = self.with(|c| {
            c.execute(
                "INSERT INTO activity(kind,message,ref_id,created_at) VALUES(?1,?2,?3,?4)",
                params![kind, message, ref_id, now_ts()],
            )?;
            c.execute(
                "DELETE FROM activity WHERE id NOT IN (SELECT id FROM activity ORDER BY id DESC LIMIT 500)",
                [],
            )?;
            Ok(())
        });
    }

    // ---- digests (lịch sử điểm tin đã chạy) ----

    /// Store one finished digest. Keeps the newest `KEEP` rows — a digest is a
    /// snapshot of a moment, so an unbounded log would just grow forever.
    #[allow(clippy::too_many_arguments)]
    pub fn save_digest(
        &self,
        hours: i64,
        focus: &str,
        topic_id: Option<i64>,
        topic_name: &str,
        article_count: i64,
        text: &str,
        model: &str,
        truncated: bool,
    ) -> Result<i64> {
        const KEEP: i64 = 50;
        self.with(|c| {
            c.execute(
                "INSERT INTO digests(hours,focus,topic_id,topic_name,article_count,text,model,truncated,created_at)
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)",
                params![hours, focus.trim(), topic_id, topic_name.trim(), article_count, text, model, truncated as i64, now_ts()],
            )?;
            let id = c.last_insert_rowid();
            c.execute(
                "DELETE FROM digests WHERE id NOT IN (SELECT id FROM digests ORDER BY id DESC LIMIT ?1)",
                params![KEEP],
            )?;
            Ok(id)
        })
    }

    /// Newest first. `text` is replaced by a short preview — the list view
    /// never needs the whole report, and shipping 50 of them would be wasteful.
    pub fn list_digests(&self, limit: i64) -> Vec<Value> {
        self.with(|c| {
            let mut st = c.prepare(
                "SELECT id,hours,focus,topic_id,topic_name,article_count,model,truncated,created_at,
                        substr(text,1,300)
                 FROM digests ORDER BY id DESC LIMIT ?1",
            )?;
            let rows = st
                .query_map(params![limit.clamp(1, 50)], |r| {
                    Ok(json!({
                        "id": r.get::<_, i64>(0)?,
                        "hours": r.get::<_, i64>(1)?,
                        "focus": r.get::<_, String>(2)?,
                        "topic_id": r.get::<_, Option<i64>>(3)?,
                        "topic_name": r.get::<_, String>(4)?,
                        "article_count": r.get::<_, i64>(5)?,
                        "model": r.get::<_, String>(6)?,
                        "truncated": r.get::<_, i64>(7)? != 0,
                        "created_at": iso(r.get::<_, i64>(8)?),
                        "preview": crate::fetch::clip(&crate::fetch::strip_html(&r.get::<_, String>(9)?), 160),
                    }))
                })?
                .flatten()
                .collect();
            Ok(rows)
        })
        .unwrap_or_default()
    }

    pub fn get_digest(&self, id: i64) -> Option<Value> {
        self.with(|c| {
            Ok(c.query_row(
                "SELECT id,hours,focus,topic_id,topic_name,article_count,model,truncated,created_at,text
                 FROM digests WHERE id=?1",
                params![id],
                |r| {
                    Ok(json!({
                        "id": r.get::<_, i64>(0)?,
                        "hours": r.get::<_, i64>(1)?,
                        "focus": r.get::<_, String>(2)?,
                        "topic_id": r.get::<_, Option<i64>>(3)?,
                        "topic_name": r.get::<_, String>(4)?,
                        "article_count": r.get::<_, i64>(5)?,
                        "model": r.get::<_, String>(6)?,
                        "truncated": r.get::<_, i64>(7)? != 0,
                        "created_at": iso(r.get::<_, i64>(8)?),
                        "text": r.get::<_, String>(9)?,
                    }))
                },
            )
            .optional()?)
        })
        .ok()
        .flatten()
    }

    pub fn delete_digest(&self, id: i64) -> Result<()> {
        self.with(|c| {
            let n = c.execute("DELETE FROM digests WHERE id=?1", params![id])?;
            if n == 0 {
                return Err(anyhow!("bản điểm tin #{id} không tồn tại"));
            }
            Ok(())
        })
    }

    pub fn recent_activity(&self, limit: i64) -> Vec<Value> {
        self.with(|c| {
            let mut st = c.prepare(
                "SELECT kind,message,ref_id,created_at FROM activity ORDER BY id DESC LIMIT ?1",
            )?;
            let rows = st
                .query_map(params![limit], |r| {
                    Ok(json!({
                        "kind": r.get::<_, String>(0)?,
                        "message": r.get::<_, String>(1)?,
                        "ref_id": r.get::<_, String>(2)?,
                        "at": iso(r.get::<_, i64>(3)?),
                    }))
                })?
                .flatten()
                .collect();
            Ok(rows)
        })
        .unwrap_or_default()
    }

    // ---- sources ----

    /// `kind` is `"feed"` (RSS/Atom) or `"scrape"` (harvest links out of an
    /// ordinary page); an empty string means "feed".
    pub fn add_source(
        &self,
        name: &str,
        url: &str,
        category: &str,
        lang: &str,
        note: &str,
        kind: &str,
    ) -> Result<i64> {
        let url = url.trim();
        if url.is_empty() || !(url.starts_with("http://") || url.starts_with("https://")) {
            return Err(anyhow!("URL nguồn phải bắt đầu bằng http(s)://"));
        }
        let kind = normalize_kind(kind)?;
        self.with(|c| {
            let dup: Option<i64> = c
                .query_row("SELECT id FROM sources WHERE url=?1", params![url], |r| r.get(0))
                .optional()?;
            if let Some(id) = dup {
                return Err(anyhow!("nguồn này đã tồn tại (#{id})"));
            }
            c.execute(
                "INSERT INTO sources(name,url,category,lang,note,kind,created_at) VALUES(?1,?2,?3,?4,?5,?6,?7)",
                params![name.trim(), url, category.trim(), lang.trim(), note.trim(), kind, now_ts()],
            )?;
            Ok(c.last_insert_rowid())
        })
    }

    pub fn list_sources(&self, status: Option<&str>) -> Vec<Value> {
        self.with(|c| {
            let mut st = c.prepare(
                "SELECT s.id,s.name,s.url,s.category,s.lang,s.status,s.last_fetch_at,s.last_status,
                        s.last_error,s.note,
                        (SELECT COUNT(*) FROM articles a WHERE a.source_id=s.id) AS n,
                        s.kind
                 FROM sources s
                 WHERE (?1 IS NULL OR s.status=?1)
                 ORDER BY s.id",
            )?;
            let rows = st
                .query_map(params![status], |r| {
                    Ok(json!({
                        "id": r.get::<_, i64>(0)?,
                        "name": r.get::<_, String>(1)?,
                        "url": r.get::<_, String>(2)?,
                        "category": r.get::<_, String>(3)?,
                        "lang": r.get::<_, String>(4)?,
                        "status": r.get::<_, String>(5)?,
                        "last_fetch_at": iso(r.get::<_, i64>(6)?),
                        "last_status": r.get::<_, String>(7)?,
                        "last_error": r.get::<_, String>(8)?,
                        "note": r.get::<_, String>(9)?,
                        "article_count": r.get::<_, i64>(10)?,
                        "kind": r.get::<_, String>(11)?,
                    }))
                })?
                .flatten()
                .collect();
            Ok(rows)
        })
        .unwrap_or_default()
    }

    pub fn get_source(&self, id: i64) -> Option<Value> {
        self.list_sources(None).into_iter().find(|s| s["id"] == id)
    }

    /// Patch-style update: only provided fields change.
    pub fn update_source(&self, id: i64, patch: &Value) -> Result<()> {
        self.with(|c| {
            let exists: Option<i64> = c
                .query_row("SELECT id FROM sources WHERE id=?1", params![id], |r| {
                    r.get(0)
                })
                .optional()?;
            if exists.is_none() {
                return Err(anyhow!("nguồn #{id} không tồn tại"));
            }
            for (field, col) in [
                ("name", "name"),
                ("url", "url"),
                ("category", "category"),
                ("lang", "lang"),
                ("status", "status"),
                ("note", "note"),
                ("kind", "kind"),
            ] {
                if let Some(v) = patch.get(field).and_then(|x| x.as_str()) {
                    if field == "status" && !["active", "paused"].contains(&v) {
                        return Err(anyhow!("status phải là active|paused"));
                    }
                    let v = if field == "kind" {
                        normalize_kind(v)?
                    } else {
                        v.trim()
                    };
                    c.execute(
                        &format!("UPDATE sources SET {col}=?1 WHERE id=?2"),
                        params![v, id],
                    )?;
                }
            }
            Ok(())
        })
    }

    /// Delete a source AND its articles (fts rows, topic links, analyses;
    /// stories are trimmed via decrement + empty-story sweep).
    pub fn delete_source(&self, id: i64) -> Result<i64> {
        let _ = self.with(|c| {
            c.execute("DELETE FROM scrape_rejects WHERE source_id=?1", params![id])?;
            Ok(())
        });
        self.with(|c| {
            let exists: Option<i64> =
                c.query_row("SELECT id FROM sources WHERE id=?1", params![id], |r| r.get(0)).optional()?;
            if exists.is_none() {
                return Err(anyhow!("nguồn #{id} không tồn tại"));
            }
            c.execute(
                "DELETE FROM articles_fts WHERE article_id IN (SELECT id FROM articles WHERE source_id=?1)",
                params![id],
            )?;
            c.execute(
                "DELETE FROM article_topics WHERE article_id IN (SELECT id FROM articles WHERE source_id=?1)",
                params![id],
            )?;
            c.execute(
                "DELETE FROM analyses WHERE article_id IN (SELECT id FROM articles WHERE source_id=?1)",
                params![id],
            )?;
            let removed = c.execute("DELETE FROM articles WHERE source_id=?1", params![id])? as i64;
            c.execute("DELETE FROM sources WHERE id=?1", params![id])?;
            Self::sweep_stories(c)?;
            Ok(removed)
        })
    }

    pub fn mark_source_fetch(
        &self,
        id: i64,
        etag: &str,
        last_modified: &str,
        status: &str,
        error: &str,
    ) {
        let _ = self.with(|c| {
            c.execute(
                "UPDATE sources SET etag=?1,last_modified=?2,last_fetch_at=?3,last_status=?4,last_error=?5 WHERE id=?6",
                params![etag, last_modified, now_ts(), status, crate::fetch::clip(error, 500), id],
            )?;
            Ok(())
        });
    }

    /// Conditional-GET state of one source.
    pub fn source_fetch_meta(&self, id: i64) -> Option<(String, String)> {
        self.with(|c| {
            Ok(c.query_row(
                "SELECT etag,last_modified FROM sources WHERE id=?1",
                params![id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?)
        })
        .ok()
        .flatten()
    }

    /// URLs out of `urls` that are already stored. Lets the page scraper skip
    /// re-fetching article pages it has read before — the whole cost of a
    /// scrape source is those per-article requests.
    pub fn known_urls(&self, urls: &[String]) -> std::collections::HashSet<String> {
        if urls.is_empty() {
            return Default::default();
        }
        self.with(|c| {
            let marks = vec!["?"; urls.len()].join(",");
            let mut st = c.prepare(&format!("SELECT url FROM articles WHERE url IN ({marks})"))?;
            let rows = st
                .query_map(rusqlite::params_from_iter(urls.iter()), |r| {
                    r.get::<_, String>(0)
                })?
                .flatten()
                .collect();
            Ok(rows)
        })
        .unwrap_or_default()
    }

    /// URLs already judged "not an article page" — skip without re-fetching.
    pub fn rejected_urls(&self, urls: &[String]) -> std::collections::HashSet<String> {
        if urls.is_empty() {
            return Default::default();
        }
        self.with(|c| {
            let marks = vec!["?"; urls.len()].join(",");
            let mut st = c.prepare(&format!(
                "SELECT url FROM scrape_rejects WHERE url IN ({marks})"
            ))?;
            let rows = st
                .query_map(rusqlite::params_from_iter(urls.iter()), |r| {
                    r.get::<_, String>(0)
                })?
                .flatten()
                .collect();
            Ok(rows)
        })
        .unwrap_or_default()
    }

    pub fn mark_rejected(&self, source_id: i64, urls: &[String]) {
        let _ = self.with(|c| {
            let now = now_ts();
            for u in urls {
                c.execute(
                    "INSERT OR REPLACE INTO scrape_rejects(url,source_id,at) VALUES(?1,?2,?3)",
                    params![u, source_id, now],
                )?;
            }
            Ok(())
        });
    }

    /// (id, url, etag, last_modified) of every active source.
    pub fn sources_to_fetch(&self) -> Vec<(i64, String, String, String)> {
        self.with(|c| {
            let mut st = c.prepare(
                "SELECT id,url,etag,last_modified FROM sources WHERE status='active' ORDER BY id",
            )?;
            let rows = st
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
                .flatten()
                .collect();
            Ok(rows)
        })
        .unwrap_or_default()
    }

    // ---- articles ----

    /// Insert one fetched item. Returns Some(article_id) when new, None when
    /// the hash (url|guid) was already seen. Also mirrors into FTS.
    #[allow(clippy::too_many_arguments)]
    pub fn insert_article(
        &self,
        source_id: i64,
        guid: &str,
        url: &str,
        title: &str,
        description: &str,
        image_url: &str,
        author: &str,
        category: &str,
        published_at: i64,
    ) -> Result<Option<i64>> {
        use sha2::{Digest, Sha256};
        let key = if !url.trim().is_empty() {
            url.trim()
        } else {
            guid.trim()
        };
        let hash = hex::encode(Sha256::digest(key.as_bytes()));
        self.with(|c| {
            let dup: Option<i64> = c
                .query_row("SELECT id FROM articles WHERE hash=?1", params![hash], |r| r.get(0))
                .optional()?;
            if dup.is_some() {
                return Ok(None);
            }
            let now = now_ts();
            // Feeds sometimes carry future or absent dates; clamp into sanity.
            let pub_at = if published_at <= 0 || published_at > now + 3600 { now } else { published_at };
            c.execute(
                "INSERT INTO articles(source_id,hash,guid,url,title,description,image_url,author,category,published_at,fetched_at)
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
                params![source_id, hash, guid.trim(), url.trim(), title.trim(), description.trim(),
                        image_url.trim(), author.trim(), category.trim(), pub_at, now],
            )?;
            let id = c.last_insert_rowid();
            c.execute(
                "INSERT INTO articles_fts(article_id,title,description,content) VALUES(?1,?2,?3,'')",
                params![id, title.trim(), description.trim()],
            )?;
            Ok(Some(id))
        })
    }

    pub fn set_article_content(&self, id: i64, content: &str) -> Result<()> {
        self.with(|c| {
            let n = c.execute(
                "UPDATE articles SET content=?1 WHERE id=?2",
                params![content, id],
            )?;
            if n == 0 {
                return Err(anyhow!("bài #{id} không tồn tại"));
            }
            c.execute("DELETE FROM articles_fts WHERE article_id=?1", params![id])?;
            c.execute(
                "INSERT INTO articles_fts(article_id,title,description,content)
                 SELECT id,title,description,content FROM articles WHERE id=?1",
                params![id],
            )?;
            Ok(())
        })
    }

    fn article_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Value> {
        Ok(json!({
            "id": r.get::<_, i64>(0)?,
            "source_id": r.get::<_, i64>(1)?,
            "source_name": r.get::<_, String>(2)?,
            "url": r.get::<_, String>(3)?,
            "title": r.get::<_, String>(4)?,
            "description": r.get::<_, String>(5)?,
            "image_url": r.get::<_, String>(6)?,
            "author": r.get::<_, String>(7)?,
            "category": r.get::<_, String>(8)?,
            "story_id": r.get::<_, Option<i64>>(9)?,
            "published_at": iso(r.get::<_, i64>(10)?),
            "has_content": r.get::<_, i64>(11)? > 0,
            "story_size": r.get::<_, Option<i64>>(12)?.unwrap_or(0),
            "section": r.get::<_, String>(13)?,
            "section_conf": r.get::<_, f64>(14)?,
        }))
    }

    const ARTICLE_COLS: &'static str =
        "a.id, a.source_id, COALESCE(s.name,''), a.url, a.title, a.description, a.image_url,
         a.author, a.category, a.story_id, a.published_at, length(a.content),
         (SELECT article_count FROM stories st WHERE st.id=a.story_id),
         a.section, a.section_conf";

    /// Filtered listing, newest first. `q` uses FTS (diacritic-insensitive).
    #[allow(clippy::too_many_arguments)]
    pub fn list_articles(
        &self,
        q: Option<&str>,
        source_id: Option<i64>,
        topic_id: Option<i64>,
        story_id: Option<i64>,
        category: Option<&str>,
        section: Option<&str>,
        since_ts: Option<i64>,
        limit: i64,
        offset: i64,
    ) -> Vec<Value> {
        let limit = limit.clamp(1, 500);
        self.with(|c| {
            let mut sql = format!(
                "SELECT {} FROM articles a JOIN sources s ON s.id=a.source_id",
                Self::ARTICLE_COLS
            );
            let mut wheres: Vec<String> = Vec::new();
            let mut binds: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
            if let Some(q) = q.map(str::trim).filter(|s| !s.is_empty()) {
                sql.push_str(" JOIN articles_fts f ON f.article_id=a.id");
                wheres.push("articles_fts MATCH ?".into());
                binds.push(Box::new(fts_query(q)));
            }
            if let Some(sid) = source_id {
                wheres.push("a.source_id=?".into());
                binds.push(Box::new(sid));
            }
            if let Some(tid) = topic_id {
                wheres.push(
                    "a.id IN (SELECT article_id FROM article_topics WHERE topic_id=?)".into(),
                );
                binds.push(Box::new(tid));
            }
            if let Some(stid) = story_id {
                wheres.push("a.story_id=?".into());
                binds.push(Box::new(stid));
            }
            if let Some(cat) = category.map(str::trim).filter(|s| !s.is_empty()) {
                wheres.push("(a.category LIKE '%'||?||'%' OR s.category LIKE '%'||?||'%')".into());
                binds.push(Box::new(cat.to_string()));
                binds.push(Box::new(cat.to_string()));
            }
            if let Some(sec) = section.map(str::trim).filter(|s| !s.is_empty()) {
                wheres.push("a.section=?".into());
                binds.push(Box::new(sec.to_string()));
            }
            if let Some(ts) = since_ts {
                wheres.push("a.published_at>=?".into());
                binds.push(Box::new(ts));
            }
            if !wheres.is_empty() {
                sql.push_str(" WHERE ");
                sql.push_str(&wheres.join(" AND "));
            }
            sql.push_str(" ORDER BY a.published_at DESC LIMIT ? OFFSET ?");
            binds.push(Box::new(limit));
            binds.push(Box::new(offset.max(0)));

            let mut st = c.prepare(&sql)?;
            let params_ref: Vec<&dyn rusqlite::types::ToSql> =
                binds.iter().map(|b| b.as_ref()).collect();
            let rows = st
                .query_map(params_ref.as_slice(), Self::article_row)?
                .flatten()
                .collect();
            Ok(rows)
        })
        .unwrap_or_default()
    }

    pub fn get_article(&self, id: i64) -> Option<Value> {
        self.with(|c| {
            let sql = format!(
                "SELECT {}, a.content, a.fetched_at FROM articles a JOIN sources s ON s.id=a.source_id WHERE a.id=?1",
                Self::ARTICLE_COLS
            );
            let art = c
                .query_row(&sql, params![id], |r| {
                    let mut v = Self::article_row(r)?;
                    v["content"] = json!(r.get::<_, String>(15)?);
                    v["fetched_at"] = json!(iso(r.get::<_, i64>(16)?));
                    Ok(v)
                })
                .optional()?;
            let Some(mut art) = art else { return Ok(None) };
            // topics of this article
            let mut st = c.prepare(
                "SELECT t.id,t.name,t.color FROM topics t JOIN article_topics at ON at.topic_id=t.id WHERE at.article_id=?1",
            )?;
            let topics: Vec<Value> = st
                .query_map(params![id], |r| {
                    Ok(json!({"id": r.get::<_, i64>(0)?, "name": r.get::<_, String>(1)?, "color": r.get::<_, String>(2)?}))
                })?
                .flatten()
                .collect();
            art["topics"] = json!(topics);
            // cached AI analysis if any
            if let Some(an) = Self::analysis_row(c, id)? {
                art["analysis"] = an;
            }
            Ok(Some(art))
        })
        .unwrap_or(None)
    }

    /// Sibling articles of the same story, for "tin liên quan".
    pub fn related_articles(&self, article_id: i64, limit: i64) -> Vec<Value> {
        self.with(|c| {
            let story: Option<i64> = c
                .query_row(
                    "SELECT story_id FROM articles WHERE id=?1",
                    params![article_id],
                    |r| r.get(0),
                )
                .optional()?
                .flatten();
            let Some(sid) = story else {
                return Ok(Vec::new());
            };
            let sql = format!(
                "SELECT {} FROM articles a JOIN sources s ON s.id=a.source_id
                 WHERE a.story_id=?1 AND a.id<>?2 ORDER BY a.published_at DESC LIMIT ?3",
                Self::ARTICLE_COLS
            );
            let mut st = c.prepare(&sql)?;
            let rows = st
                .query_map(params![sid, article_id, limit], Self::article_row)?
                .flatten()
                .collect();
            Ok(rows)
        })
        .unwrap_or_default()
    }

    /// `(id, title)` rows in a published_at window — trend input.
    pub fn titles_between(&self, from_ts: i64, to_ts: i64) -> Vec<(i64, String)> {
        self.with(|c| {
            let mut st = c.prepare(
                "SELECT id,title FROM articles WHERE published_at>=?1 AND published_at<?2",
            )?;
            let rows = st
                .query_map(params![from_ts, to_ts], |r| Ok((r.get(0)?, r.get(1)?)))?
                .flatten()
                .collect();
            Ok(rows)
        })
        .unwrap_or_default()
    }

    /// Compact `{id,title,source,published_at}` for a set of ids (trend samples).
    pub fn brief_articles(&self, ids: &[i64], limit: usize) -> Vec<Value> {
        let ids: Vec<i64> = ids.iter().take(limit).copied().collect();
        if ids.is_empty() {
            return Vec::new();
        }
        self.with(|c| {
            let marks = vec!["?"; ids.len()].join(",");
            let sql = format!(
                "SELECT a.id,a.title,COALESCE(s.name,''),a.published_at,a.url FROM articles a
                 JOIN sources s ON s.id=a.source_id WHERE a.id IN ({marks}) ORDER BY a.published_at DESC"
            );
            let mut st = c.prepare(&sql)?;
            let params_ref: Vec<&dyn rusqlite::types::ToSql> =
                ids.iter().map(|i| i as &dyn rusqlite::types::ToSql).collect();
            let rows = st
                .query_map(params_ref.as_slice(), |r| {
                    Ok(json!({
                        "id": r.get::<_, i64>(0)?,
                        "title": r.get::<_, String>(1)?,
                        "source": r.get::<_, String>(2)?,
                        "published_at": iso(r.get::<_, i64>(3)?),
                        "url": r.get::<_, String>(4)?,
                    }))
                })?
                .flatten()
                .collect();
            Ok(rows)
        })
        .unwrap_or_default()
    }

    // ---- topics ----

    pub fn add_topic(&self, name: &str, keywords: &str, color: &str) -> Result<i64> {
        if name.trim().is_empty() {
            return Err(anyhow!("thiếu tên chủ đề"));
        }
        self.with(|c| {
            c.execute(
                "INSERT INTO topics(name,keywords,color,created_at) VALUES(?1,?2,?3,?4)",
                params![name.trim(), keywords.trim(), color.trim(), now_ts()],
            )?;
            Ok(c.last_insert_rowid())
        })
    }

    pub fn list_topics(&self) -> Vec<Value> {
        self.with(|c| {
            let mut st = c.prepare(
                "SELECT t.id,t.name,t.keywords,t.color,
                        (SELECT COUNT(*) FROM article_topics at WHERE at.topic_id=t.id) AS n
                 FROM topics t ORDER BY t.id",
            )?;
            let rows = st
                .query_map([], |r| {
                    Ok(json!({
                        "id": r.get::<_, i64>(0)?,
                        "name": r.get::<_, String>(1)?,
                        "keywords": r.get::<_, String>(2)?,
                        "color": r.get::<_, String>(3)?,
                        "article_count": r.get::<_, i64>(4)?,
                    }))
                })?
                .flatten()
                .collect();
            Ok(rows)
        })
        .unwrap_or_default()
    }

    pub fn update_topic(&self, id: i64, patch: &Value) -> Result<()> {
        self.with(|c| {
            let exists: Option<i64> = c
                .query_row("SELECT id FROM topics WHERE id=?1", params![id], |r| {
                    r.get(0)
                })
                .optional()?;
            if exists.is_none() {
                return Err(anyhow!("chủ đề #{id} không tồn tại"));
            }
            for field in ["name", "keywords", "color"] {
                if let Some(v) = patch.get(field).and_then(|x| x.as_str()) {
                    c.execute(
                        &format!("UPDATE topics SET {field}=?1 WHERE id=?2"),
                        params![v.trim(), id],
                    )?;
                }
            }
            Ok(())
        })
    }

    pub fn delete_topic(&self, id: i64) -> Result<()> {
        self.with(|c| {
            c.execute("DELETE FROM article_topics WHERE topic_id=?1", params![id])?;
            let n = c.execute("DELETE FROM topics WHERE id=?1", params![id])?;
            if n == 0 {
                return Err(anyhow!("chủ đề #{id} không tồn tại"));
            }
            Ok(())
        })
    }

    /// `(topic_id, [lowercased keywords])` for the matcher.
    pub fn topic_keywords(&self) -> Vec<(i64, Vec<String>)> {
        self.list_topics()
            .into_iter()
            .map(|t| {
                let kws = t["keywords"]
                    .as_str()
                    .unwrap_or("")
                    .split(',')
                    .map(|k| k.trim().to_lowercase())
                    .filter(|k| !k.is_empty())
                    .collect();
                (t["id"].as_i64().unwrap_or(0), kws)
            })
            .collect()
    }

    /// Keyword-match one article's title+description against every topic.
    pub fn assign_topics(&self, article_id: i64, title: &str, description: &str) {
        let hay = format!("{} {}", title, description).to_lowercase();
        for (tid, kws) in self.topic_keywords() {
            if kws.iter().any(|k| hay.contains(k.as_str())) {
                let _ = self.with(|c| {
                    c.execute(
                        "INSERT OR IGNORE INTO article_topics(article_id,topic_id) VALUES(?1,?2)",
                        params![article_id, tid],
                    )?;
                    Ok(())
                });
            }
        }
    }

    /// Re-run keyword matching for ONE topic over recent articles (after the
    /// user edits its keywords). Returns how many articles now match.
    pub fn reassign_topic(&self, topic_id: i64, since_ts: i64) -> Result<i64> {
        let kws: Vec<String> = self
            .topic_keywords()
            .into_iter()
            .find(|(id, _)| *id == topic_id)
            .map(|(_, k)| k)
            .ok_or_else(|| anyhow!("chủ đề #{topic_id} không tồn tại"))?;
        self.with(|c| {
            c.execute(
                "DELETE FROM article_topics WHERE topic_id=?1",
                params![topic_id],
            )?;
            let mut st =
                c.prepare("SELECT id,title,description FROM articles WHERE published_at>=?1")?;
            let arts: Vec<(i64, String, String)> = st
                .query_map(params![since_ts], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
                .flatten()
                .collect();
            let mut n = 0i64;
            for (aid, title, desc) in arts {
                let hay = format!("{} {}", title, desc).to_lowercase();
                if kws.iter().any(|k| hay.contains(k.as_str())) {
                    c.execute(
                        "INSERT OR IGNORE INTO article_topics(article_id,topic_id) VALUES(?1,?2)",
                        params![aid, topic_id],
                    )?;
                    n += 1;
                }
            }
            Ok(n)
        })
    }

    // ---- chuyên mục (System One / Laya) ----

    /// Chủ đề người dùng dưới dạng phương án cho model.
    pub fn topic_defs(&self) -> Vec<crate::laya::TopicDef> {
        self.list_topics()
            .into_iter()
            .map(|t| crate::laya::TopicDef {
                id: t["id"].as_i64().unwrap_or(0),
                name: t["name"].as_str().unwrap_or("").to_string(),
                keywords: t["keywords"].as_str().unwrap_or("").to_string(),
            })
            .collect()
    }

    /// Bài chưa phân loại từ `since_ts`, mới nhất trước: `(id, title, description)`.
    pub fn pending_sections(&self, since_ts: i64, limit: i64) -> Vec<(i64, String, String)> {
        self.with(|c| {
            let mut st = c.prepare(
                "SELECT id,title,description FROM articles
                 WHERE section_at IS NULL AND published_at>=?1
                 ORDER BY published_at DESC LIMIT ?2",
            )?;
            let rows = st
                .query_map(params![since_ts, limit], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
                .flatten()
                .collect();
            Ok(rows)
        })
        .unwrap_or_default()
    }

    /// Ghi kết luận cho một bài. `topics = Some(..)` thay toàn bộ chủ đề của bài
    /// (kể cả thành rỗng); `None` giữ chủ đề khớp từ khóa.
    pub fn apply_section(
        &self,
        article_id: i64,
        section: &str,
        conf: f64,
        topics: Option<&[i64]>,
    ) -> Result<()> {
        self.with_mut(|c| {
            let tx = c.transaction()?;
            let found = tx.execute(
                "UPDATE articles SET section=?1, section_conf=?2, section_at=?3 WHERE id=?4",
                params![section, conf, now_ts(), article_id],
            )?;
            // Retention may have purged the article while the model was
            // answering; tagging it now would leave orphan topic rows.
            if found == 0 {
                return Ok(());
            }
            if let Some(ids) = topics {
                tx.execute("DELETE FROM article_topics WHERE article_id=?1", params![article_id])?;
                for tid in ids {
                    tx.execute(
                        "INSERT OR IGNORE INTO article_topics(article_id,topic_id) VALUES(?1,?2)",
                        params![article_id, tid],
                    )?;
                }
            }
            tx.commit()?;
            Ok(())
        })
    }

    /// Đưa bài từ `since_ts` về hàng chờ phân loại — bộ chủ đề đổi thì bộ
    /// phương án của model cũng đổi, kết luận cũ không còn đúng.
    pub fn requeue_sections(&self, since_ts: i64) -> usize {
        self.with(|c| {
            Ok(c.execute(
                "UPDATE articles SET section_at=NULL WHERE published_at>=?1",
                params![since_ts],
            )?)
        })
        .unwrap_or(0)
    }

    /// `(đã phân loại, đang chờ)` trong cửa sổ từ `since_ts`.
    pub fn section_progress(&self, since_ts: i64) -> (i64, i64) {
        self.with(|c| {
            Ok(c.query_row(
                "SELECT COALESCE(SUM(section_at IS NOT NULL),0), COALESCE(SUM(section_at IS NULL),0)
                 FROM articles WHERE published_at>=?1",
                params![since_ts],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?)
        })
        .unwrap_or((0, 0))
    }

    /// Số bài theo chuyên mục từ `since_ts`, nhiều nhất trước.
    pub fn section_counts(&self, since_ts: i64) -> Vec<Value> {
        self.with(|c| {
            let mut st = c.prepare(
                "SELECT section, COUNT(*) AS n FROM articles
                 WHERE section<>'' AND published_at>=?1
                 GROUP BY section ORDER BY n DESC, section",
            )?;
            let rows = st
                .query_map(params![since_ts], |r| {
                    Ok(json!({"section": r.get::<_, String>(0)?, "count": r.get::<_, i64>(1)?}))
                })?
                .flatten()
                .collect();
            Ok(rows)
        })
        .unwrap_or_default()
    }

    // ---- stories (dòng sự kiện) ----

    /// Language every AI answer should be written in, and what article text is
    /// translated into for display. Empty = leave everything as published.
    pub fn display_language(&self) -> String {
        self.setting("display_language", "Tiếng Việt")
    }

    /// Cached translations for these articles, `article_id → {title, description}`.
    pub fn translations_for(&self, ids: &[i64], lang: &str) -> HashMap<i64, (String, String)> {
        if ids.is_empty() || lang.is_empty() {
            return HashMap::new();
        }
        let holes = vec!["?"; ids.len()].join(",");
        self.with(|c| {
            let mut st = c.prepare(&format!(
                "SELECT article_id,title,description FROM translations
                 WHERE lang=?1 AND article_id IN ({holes})"
            ))?;
            let mut args: Vec<&dyn rusqlite::ToSql> = vec![&lang];
            for id in ids {
                args.push(id);
            }
            let rows = st
                .query_map(args.as_slice(), |r| {
                    Ok((r.get(0)?, (r.get(1)?, r.get(2)?)))
                })?
                .flatten()
                .collect();
            Ok(rows)
        })
        .unwrap_or_default()
    }

    pub fn save_translation(
        &self,
        article_id: i64,
        lang: &str,
        title: &str,
        description: &str,
    ) -> Result<()> {
        self.with(|c| {
            c.execute(
                "INSERT INTO translations(article_id,lang,title,description,at)
                 VALUES(?1,?2,?3,?4,?5)
                 ON CONFLICT(article_id,lang) DO UPDATE SET title=?3,description=?4,at=?5",
                params![article_id, lang, title, description, now_ts()],
            )?;
            Ok(())
        })
    }

    /// Raw digest-marker setting (one marker per line). Empty = use the
    /// built-in defaults.
    pub fn digest_markers_setting(&self) -> String {
        self.setting("digest_markers", "")
    }

    /// Push the configured markers into the clusterer. Call at boot and after
    /// the setting changes.
    pub fn apply_digest_markers(&self) {
        let raw = self.digest_markers_setting();
        let list: Vec<String> = if raw.trim().is_empty() {
            cluster::DEFAULT_DIGEST_MARKERS
                .iter()
                .map(|s| s.to_string())
                .collect()
        } else {
            raw.lines()
                .flat_map(|l| l.split(','))
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        };
        cluster::set_digest_markers(list);
    }

    /// Count this headline's phrases into the archive-wide frequency table.
    /// Called once per article, so `df` is a document frequency.
    pub fn bump_phrase_df(&self, title: &str) {
        let phrases = cluster::key_phrases(title);
        if phrases.is_empty() {
            return;
        }
        let _ = self.with(|c| {
            let mut st = c.prepare(
                "INSERT INTO phrase_df(phrase,df) VALUES(?1,1)
                 ON CONFLICT(phrase) DO UPDATE SET df=df+1",
            )?;
            for p in &phrases {
                st.execute(params![p])?;
            }
            Ok(())
        });
    }

    /// Archive frequencies for just these phrases — the clusterer only ever
    /// asks about the ~10 phrases of the headline it is placing, so this stays
    /// a keyed lookup instead of loading the whole table.
    pub fn corpus_for(&self, phrases: &std::collections::BTreeSet<String>) -> cluster::Corpus {
        if phrases.is_empty() {
            return cluster::Corpus::default();
        }
        self.with(|c| {
            let total: i64 = c.query_row("SELECT COUNT(*) FROM articles", [], |r| r.get(0))?;
            let holes = vec!["?"; phrases.len()].join(",");
            let mut st =
                c.prepare(&format!("SELECT phrase,df FROM phrase_df WHERE phrase IN ({holes})"))?;
            let args: Vec<&dyn rusqlite::ToSql> =
                phrases.iter().map(|p| p as &dyn rusqlite::ToSql).collect();
            let df = st
                .query_map(args.as_slice(), |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?.max(0) as u32))
                })?
                .flatten()
                .collect();
            Ok(cluster::Corpus {
                total: total.max(0) as u32,
                df,
            })
        })
        .unwrap_or_default()
    }

    /// Recent story profiles for the clusterer (last `days`, newest first).
    pub fn recent_story_profiles(&self, days: i64, limit: i64) -> Vec<StoryProfile> {
        let since = now_ts() - days * 86400;
        self.with(|c| {
            let mut st = c.prepare(
                "SELECT id,profile,first_at,last_at,article_count,facts FROM stories
                 WHERE last_at>=?1 ORDER BY last_at DESC LIMIT ?2",
            )?;
            let rows = st
                .query_map(params![since, limit], |r| {
                    Ok(StoryProfile {
                        story_id: r.get(0)?,
                        profile: cluster::profile_from_json(&r.get::<_, String>(1)?),
                        first_at: r.get(2)?,
                        last_at: r.get(3)?,
                        article_count: r.get::<_, i64>(4)?.max(0) as u32,
                        facts: serde_json::from_str(&r.get::<_, String>(5)?).unwrap_or_default(),
                    })
                })?
                .flatten()
                .collect();
            Ok(rows)
        })
        .unwrap_or_default()
    }

    /// Attach an article to a story (updating profile/counters), or create a
    /// fresh story seeded from this article. Returns the story id, or `None`
    /// when the article stays outside the event streams (roundup pages, or a
    /// headline too short to fingerprint) — those still live in the feed and in
    /// search, they just don't seed a timeline.
    pub fn place_in_story(
        &self,
        article_id: i64,
        title: &str,
        published_at: i64,
        story_id: Option<i64>,
    ) -> Result<Option<i64>> {
        if story_id.is_none()
            && (cluster::is_digest_title(title) || cluster::key_phrases(title).len() < 2)
        {
            return Ok(None);
        }
        let facts = cluster::facts_of(title);
        self.with(|c| {
            let sid = match story_id {
                Some(sid) => {
                    let (profile_json, first_at, last_at, facts_json): (String, i64, i64, String) =
                        c.query_row(
                            "SELECT profile,first_at,last_at,facts FROM stories WHERE id=?1",
                            params![sid],
                            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
                        )?;
                    let mut profile = cluster::profile_from_json(&profile_json);
                    cluster::profile_merge(&mut profile, title);
                    let mut sf: cluster::StoryFacts =
                        serde_json::from_str(&facts_json).unwrap_or_default();
                    sf.absorb(&facts);
                    c.execute(
                        "UPDATE stories SET profile=?1, first_at=?2, last_at=?3,
                                article_count=article_count+1, summary_at=0, facts=?4 WHERE id=?5",
                        params![
                            cluster::profile_to_json(&profile),
                            first_at.min(published_at),
                            last_at.max(published_at),
                            serde_json::to_string(&sf).unwrap_or_else(|_| "{}".into()),
                            sid
                        ],
                    )?;
                    sid
                }
                None => {
                    let mut profile = std::collections::HashMap::new();
                    cluster::profile_merge(&mut profile, title);
                    c.execute(
                        "INSERT INTO stories(title,profile,first_at,last_at,article_count,facts)
                         VALUES(?1,?2,?3,?3,1,?4)",
                        params![
                            title.trim(),
                            cluster::profile_to_json(&profile),
                            published_at,
                            serde_json::to_string(&cluster::StoryFacts::seed(&facts))
                                .unwrap_or_else(|_| "{}".into())
                        ],
                    )?;
                    c.last_insert_rowid()
                }
            };
            c.execute("UPDATE articles SET story_id=?1 WHERE id=?2", params![sid, article_id])?;
            Ok(Some(sid))
        })
    }

    /// Stories active in a window, biggest/freshest first. `min_articles=2`
    /// hides the single-article "stories" that are just uncorrelated news.
    pub fn list_stories(&self, since_ts: i64, min_articles: i64, limit: i64) -> Vec<Value> {
        self.with(|c| {
            let mut st = c.prepare(
                "SELECT id,title,first_at,last_at,article_count,summary,summary_at
                 FROM stories WHERE last_at>=?1 AND article_count>=?2
                 ORDER BY article_count DESC, last_at DESC LIMIT ?3",
            )?;
            let rows = st
                .query_map(params![since_ts, min_articles, limit], |r| {
                    Ok(json!({
                        "id": r.get::<_, i64>(0)?,
                        "title": r.get::<_, String>(1)?,
                        "first_at": iso(r.get::<_, i64>(2)?),
                        "last_at": iso(r.get::<_, i64>(3)?),
                        "article_count": r.get::<_, i64>(4)?,
                        "has_summary": !r.get::<_, String>(5)?.is_empty() && r.get::<_, i64>(6)? > 0,
                    }))
                })?
                .flatten()
                .collect();
            Ok(rows)
        })
        .unwrap_or_default()
    }

    /// One story + its timeline (articles ascending by time = diễn biến).
    pub fn get_story(&self, id: i64) -> Option<Value> {
        self.with(|c| {
            let head = c
                .query_row(
                    "SELECT id,title,first_at,last_at,article_count,summary,summary_model,summary_at
                     FROM stories WHERE id=?1",
                    params![id],
                    |r| {
                        Ok(json!({
                            "id": r.get::<_, i64>(0)?,
                            "title": r.get::<_, String>(1)?,
                            "first_at": iso(r.get::<_, i64>(2)?),
                            "last_at": iso(r.get::<_, i64>(3)?),
                            "article_count": r.get::<_, i64>(4)?,
                            "summary": r.get::<_, String>(5)?,
                            "summary_model": r.get::<_, String>(6)?,
                            "summary_at": if r.get::<_, i64>(7)? > 0 { iso(r.get::<_, i64>(7)?) } else { String::new() },
                        }))
                    },
                )
                .optional()?;
            let Some(mut story) = head else { return Ok(None) };
            // Newest first — the timeline is read like a feed ("mới nhất trước").
            // `llm::story_prompt` re-orders ascending for the narrative summary.
            let sql = format!(
                "SELECT {} FROM articles a JOIN sources s ON s.id=a.source_id
                 WHERE a.story_id=?1 ORDER BY a.published_at DESC",
                Self::ARTICLE_COLS
            );
            let mut st = c.prepare(&sql)?;
            let mut timeline: Vec<Value> =
                st.query_map(params![id], Self::article_row)?.flatten().collect();
            drop(st);
            // Attach any translation already cached for the display language.
            // The original text is never replaced — the UI shows the reader's
            // language and keeps the published headline underneath.
            //
            // Read through `c`, NOT through the `self.setting()` / `self.
            // translations_for()` helpers: those take the same connection lock
            // this closure already holds, and it is not reentrant.
            let lang: String = c
                .query_row(
                    "SELECT value FROM settings WHERE key='display_language'",
                    [],
                    |r| r.get(0),
                )
                .optional()?
                .unwrap_or_else(|| "Tiếng Việt".to_string());
            if !lang.trim().is_empty() {
                let mut ts = c.prepare(
                    "SELECT title,description FROM translations WHERE article_id=?1 AND lang=?2",
                )?;
                let mut n = 0;
                for a in timeline.iter_mut() {
                    let Some(aid) = a["id"].as_i64() else { continue };
                    let row: Option<(String, String)> = ts
                        .query_row(params![aid, &lang], |r| Ok((r.get(0)?, r.get(1)?)))
                        .optional()?;
                    if let Some((t, d)) = row {
                        a["title_translated"] = json!(t);
                        a["description_translated"] = json!(d);
                        n += 1;
                    }
                }
                story["display_language"] = json!(lang);
                story["translated_count"] = json!(n);
            }
            story["timeline"] = json!(timeline);
            let mut hs = c.prepare(
                "SELECT id,summary,model,article_count,last_at,created_at
                 FROM story_summaries WHERE story_id=?1 ORDER BY created_at DESC, id DESC",
            )?;
            let history: Vec<Value> = hs
                .query_map(params![id], |r| {
                    Ok(json!({
                        "id": r.get::<_, i64>(0)?,
                        "summary": r.get::<_, String>(1)?,
                        "model": r.get::<_, String>(2)?,
                        "article_count": r.get::<_, i64>(3)?,
                        "last_at": iso(r.get::<_, i64>(4)?),
                        "created_at": iso(r.get::<_, i64>(5)?),
                    }))
                })?
                .flatten()
                .collect();
            story["summaries"] = json!(history);
            Ok(Some(story))
        })
        .unwrap_or(None)
    }

    /// Stories in a window WITH the headlines of their articles — input of the
    /// story graph, which links on phrases rather than single syllables.
    /// Returns (meta json, titles) pairs, biggest stories first.
    pub fn stories_with_titles(
        &self,
        since_ts: i64,
        min_articles: i64,
        limit: i64,
    ) -> Vec<(Value, Vec<String>)> {
        self.with(|c| {
            let mut st = c.prepare(
                "SELECT id,title,first_at,last_at,article_count
                 FROM stories WHERE last_at>=?1 AND article_count>=?2
                 ORDER BY article_count DESC, last_at DESC LIMIT ?3",
            )?;
            let metas: Vec<(i64, Value)> = st
                .query_map(params![since_ts, min_articles, limit], |r| {
                    let id = r.get::<_, i64>(0)?;
                    Ok((
                        id,
                        json!({
                            "id": id,
                            "title": r.get::<_, String>(1)?,
                            "first_at": iso(r.get::<_, i64>(2)?),
                            "last_at": iso(r.get::<_, i64>(3)?),
                            "article_count": r.get::<_, i64>(4)?,
                        }),
                    ))
                })?
                .flatten()
                .collect();
            drop(st);

            let mut out = Vec::with_capacity(metas.len());
            let mut titles_st = c.prepare(
                "SELECT title FROM articles WHERE story_id=?1 ORDER BY published_at DESC LIMIT 30",
            )?;
            for (id, meta) in metas {
                let titles: Vec<String> = titles_st
                    .query_map(params![id], |r| r.get(0))?
                    .flatten()
                    .collect();
                out.push((meta, titles));
            }
            Ok(out)
        })
        .unwrap_or_default()
    }

    /// Store a brief as the story's current summary AND as a history entry.
    pub fn set_story_summary(&self, id: i64, summary: &str, model: &str) -> Result<()> {
        self.with(|c| {
            let now = now_ts();
            let n = c.execute(
                "UPDATE stories SET summary=?1,summary_model=?2,summary_at=?3 WHERE id=?4",
                params![summary, model, now, id],
            )?;
            if n == 0 {
                return Err(anyhow!("dòng sự kiện #{id} không tồn tại"));
            }
            let (count, last_at): (i64, i64) = c.query_row(
                "SELECT article_count,last_at FROM stories WHERE id=?1",
                params![id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            c.execute(
                "INSERT INTO story_summaries(story_id,summary,model,article_count,last_at,created_at)
                 VALUES(?1,?2,?3,?4,?5,?6)",
                params![id, summary, model, count, last_at, now],
            )?;
            // Keep the last 20 readings per story — enough to look back through,
            // bounded so a story re-summarised nightly can't grow without limit.
            c.execute(
                "DELETE FROM story_summaries WHERE story_id=?1 AND id NOT IN
                   (SELECT id FROM story_summaries WHERE story_id=?1
                    ORDER BY created_at DESC, id DESC LIMIT 20)",
                params![id],
            )?;
            Ok(())
        })
    }

    /// Remove stories that lost all their articles (after deletes/cleanup).
    fn sweep_stories(c: &Connection) -> Result<()> {
        c.execute(
            "DELETE FROM stories WHERE id NOT IN (SELECT DISTINCT story_id FROM articles WHERE story_id IS NOT NULL)",
            [],
        )?;
        c.execute(
            "DELETE FROM story_summaries WHERE story_id NOT IN (SELECT id FROM stories)",
            [],
        )?;
        Ok(())
    }

    // ---- analyses ----

    fn analysis_row(c: &Connection, article_id: i64) -> Result<Option<Value>> {
        Ok(c.query_row(
            "SELECT summary,sentiment,importance,clickbait,reliability,tags,model,created_at
             FROM analyses WHERE article_id=?1",
            params![article_id],
            |r| {
                Ok(json!({
                    "summary": r.get::<_, String>(0)?,
                    "sentiment": r.get::<_, String>(1)?,
                    "importance": r.get::<_, i64>(2)?,
                    "clickbait": r.get::<_, i64>(3)? != 0,
                    "reliability": r.get::<_, String>(4)?,
                    "tags": r.get::<_, String>(5)?.split(',').filter(|s| !s.is_empty()).collect::<Vec<_>>(),
                    "model": r.get::<_, String>(6)?,
                    "at": iso(r.get::<_, i64>(7)?),
                }))
            },
        )
        .optional()?)
    }

    pub fn get_analysis(&self, article_id: i64) -> Option<Value> {
        self.with(|c| Self::analysis_row(c, article_id))
            .unwrap_or(None)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn save_analysis(
        &self,
        article_id: i64,
        summary: &str,
        sentiment: &str,
        importance: i64,
        clickbait: bool,
        reliability: &str,
        tags: &[String],
        model: &str,
    ) -> Result<()> {
        self.with(|c| {
            c.execute(
                "INSERT INTO analyses(article_id,summary,sentiment,importance,clickbait,reliability,tags,model,created_at)
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)
                 ON CONFLICT(article_id) DO UPDATE SET summary=excluded.summary,sentiment=excluded.sentiment,
                   importance=excluded.importance,clickbait=excluded.clickbait,reliability=excluded.reliability,
                   tags=excluded.tags,model=excluded.model,created_at=excluded.created_at",
                params![
                    article_id, summary, sentiment, importance.clamp(0, 5),
                    clickbait as i64, reliability, tags.join(","), model, now_ts()
                ],
            )?;
            Ok(())
        })
    }

    // ---- dashboard / maintenance ----

    pub fn dashboard(&self) -> Value {
        let now = now_ts();
        let day = 86400i64;
        self.with(|c| {
            let count = |sql: &str, binds: &[&dyn rusqlite::types::ToSql]| -> i64 {
                c.query_row(sql, binds, |r| r.get(0)).unwrap_or(0)
            };
            let total = count("SELECT COUNT(*) FROM articles", &[]);
            let today = count(
                "SELECT COUNT(*) FROM articles WHERE published_at>=?1",
                &[&(now - day)],
            );
            let sources_active = count("SELECT COUNT(*) FROM sources WHERE status='active'", &[]);
            let sources_error = count(
                "SELECT COUNT(*) FROM sources WHERE status='active' AND last_status='error'",
                &[],
            );
            let last_fetch: i64 = count("SELECT COALESCE(MAX(last_fetch_at),0) FROM sources", &[]);

            // Bar chart: articles per day, last 14 days.
            let mut per_day: Vec<Value> = Vec::new();
            for i in (0..14).rev() {
                let from = now - (i + 1) * day;
                let to = now - i * day;
                let n = count(
                    "SELECT COUNT(*) FROM articles WHERE published_at>=?1 AND published_at<?2",
                    &[&from, &to],
                );
                per_day.push(
                    json!({ "day": iso(to).chars().take(10).collect::<String>(), "count": n }),
                );
            }

            // Top topics (7d).
            let mut st = c.prepare(
                "SELECT t.id,t.name,t.color,COUNT(*) FROM topics t
                 JOIN article_topics at ON at.topic_id=t.id
                 JOIN articles a ON a.id=at.article_id AND a.published_at>=?1
                 GROUP BY t.id ORDER BY COUNT(*) DESC LIMIT 8",
            )?;
            let top_topics: Vec<Value> = st
                .query_map(params![now - 7 * day], |r| {
                    Ok(json!({
                        "id": r.get::<_, i64>(0)?,
                        "name": r.get::<_, String>(1)?,
                        "color": r.get::<_, String>(2)?,
                        "count": r.get::<_, i64>(3)?,
                    }))
                })?
                .flatten()
                .collect();
            drop(st);

            Ok(json!({
                "articles_total": total,
                "articles_24h": today,
                "sources_active": sources_active,
                "sources_error": sources_error,
                "last_fetch_at": if last_fetch > 0 { iso(last_fetch) } else { String::new() },
                "per_day": per_day,
                "top_topics": top_topics,
            }))
        })
        .unwrap_or_else(|e| json!({ "error": e.to_string() }))
    }

    /// Drop articles older than `retention_days` + orphaned rows. Returns the
    /// number of removed articles.
    pub fn cleanup(&self, retention_days: i64) -> Result<i64> {
        let cutoff = now_ts() - retention_days.max(1) * 86400;
        self.with(|c| {
            c.execute(
                "DELETE FROM articles_fts WHERE article_id IN (SELECT id FROM articles WHERE published_at<?1)",
                params![cutoff],
            )?;
            c.execute(
                "DELETE FROM article_topics WHERE article_id IN (SELECT id FROM articles WHERE published_at<?1)",
                params![cutoff],
            )?;
            c.execute(
                "DELETE FROM analyses WHERE article_id IN (SELECT id FROM articles WHERE published_at<?1)",
                params![cutoff],
            )?;
            let n = c.execute("DELETE FROM articles WHERE published_at<?1", params![cutoff])? as i64;
            // Expire reject memories too, so a page that later becomes a real
            // article (or a site that fixes its markup) gets another look.
            c.execute("DELETE FROM scrape_rejects WHERE at<?1", params![cutoff])?;
            // Recount story members; sweep empties.
            c.execute(
                "UPDATE stories SET article_count=(SELECT COUNT(*) FROM articles WHERE story_id=stories.id)",
                [],
            )?;
            Self::sweep_stories(c)?;
            Ok(n)
        })
    }

    /// Bumped whenever the clustering rules change. Stories built by an older
    /// version are regrouped once, automatically, at the next boot — otherwise
    /// a fixed algorithm would keep serving groupings the broken one produced.
    pub const CLUSTER_VERSION: &'static str = "3";

    /// Does the archive need regrouping right now? Either the rules changed
    /// under it, or enough time has passed since the last pass.
    pub fn regroup_due(&self) -> bool {
        if self.setting("cluster_version", "0") != Self::CLUSTER_VERSION {
            return true;
        }
        let hours: i64 = self
            .setting("auto_regroup_hours", "12")
            .parse()
            .unwrap_or(12);
        if hours <= 0 {
            return false; // user turned it off
        }
        let last: i64 = self.setting("last_regroup_at", "0").parse().unwrap_or(0);
        now_ts() - last >= hours * 3600
    }

    /// Re-cluster every article from scratch with the current algorithm.
    ///
    /// Needed whenever the clustering rules change: the stored profiles were
    /// built by the old rules, so leaving them in place would keep serving the
    /// old (wrong) groupings forever. Summaries are keyed to stories that no
    /// longer exist afterwards, so history rows are dropped with them.
    ///
    /// Runs in ONE transaction over articles in publication order, which is the
    /// order the live ingest would have seen them in.
    pub fn rebuild_stories(&self) -> Result<Value> {
        let started = now_ts();
        let out = self.rebuild_stories_inner(started)?;
        let _ = self.set_setting("cluster_version", Self::CLUSTER_VERSION);
        let _ = self.set_setting("last_regroup_at", &now_ts().to_string());
        Ok(out)
    }

    fn rebuild_stories_inner(&self, started: i64) -> Result<Value> {
        self.with_mut(|c| {
            let tx = c.transaction()?;
            let articles: Vec<(i64, String, i64)> = {
                let mut st = tx.prepare(
                    "SELECT id,title,published_at FROM articles ORDER BY published_at ASC, id ASC",
                )?;
                let rows = st
                    .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
                    .flatten()
                    .collect();
                rows
            };

            // Pass 1: archive-wide phrase frequency, rebuilt from scratch so a
            // repair also repairs the "measured stopword list" itself.
            let mut corpus = cluster::Corpus::default();
            for (_, title, _) in &articles {
                corpus.add(title);
            }
            tx.execute("DELETE FROM phrase_df", [])?;
            {
                let mut ins = tx.prepare("INSERT INTO phrase_df(phrase,df) VALUES(?1,?2)")?;
                for (p, n) in &corpus.df {
                    ins.execute(params![p, *n as i64])?;
                }
            }

            // Pass 2 — in-memory accumulator. Candidates are kept in a sliding window —
            // articles arrive in time order, so a story whose last article is
            // beyond the join gap can never match again and is retired. That is
            // what keeps this O(articles × active stories) instead of O(n²).
            struct Acc {
                title: String,
                first_at: i64,
                last_at: i64,
                count: u32,
                profile: HashMap<String, u32>,
                facts: cluster::StoryFacts,
                members: Vec<i64>,
            }
            let mut acc: Vec<Acc> = Vec::new();
            // Inverted index phrase → stories using it. Only stories that share
            // at least one phrase can possibly match, so this replaces scanning
            // every open story for every article (which is what made a full
            // archive take minutes instead of seconds).
            let mut index: HashMap<String, Vec<usize>> = HashMap::new();
            let mut skipped_digest = 0i64;

            for (aid, title, ts) in &articles {
                if cluster::is_digest_title(title) {
                    skipped_digest += 1;
                    continue;
                }
                let keys = cluster::key_phrases(title);
                if keys.len() < 2 {
                    continue;
                }

                let mut seen: Vec<usize> = Vec::new();
                for k in &keys {
                    if corpus.is_common(k) {
                        continue;
                    }
                    for &i in index.get(k.as_str()).map(|v| v.as_slice()).unwrap_or(&[]) {
                        if !seen.contains(&i) {
                            seen.push(i);
                        }
                    }
                }
                let facts = cluster::facts_of(title);
                let mut best: Option<(f64, usize, i64)> = None;
                for i in seen {
                    let a = &acc[i];
                    if !cluster::span_allows(*ts, a.first_at, a.last_at) {
                        continue;
                    }
                    if cluster::facts_conflict(&facts, &a.facts, &corpus) {
                        continue;
                    }
                    let Some((score, shared)) =
                        cluster::score_match(&keys, &a.profile, a.count, &corpus)
                    else {
                        continue;
                    };
                    if cluster::better_than(&best, score, shared) {
                        best = Some((score, shared, i as i64));
                    }
                }

                let target = match best {
                    Some((_, _, i)) => {
                        let i = i as usize;
                        acc[i].first_at = acc[i].first_at.min(*ts);
                        acc[i].last_at = acc[i].last_at.max(*ts);
                        acc[i].count += 1;
                        acc[i].members.push(*aid);
                        cluster::profile_merge(&mut acc[i].profile, title);
                        acc[i].facts.absorb(&facts);
                        i
                    }
                    None => {
                        let mut profile = HashMap::new();
                        cluster::profile_merge(&mut profile, title);
                        acc.push(Acc {
                            title: title.trim().to_string(),
                            first_at: *ts,
                            last_at: *ts,
                            count: 1,
                            profile,
                            facts: cluster::StoryFacts::seed(&facts),
                            members: vec![*aid],
                        });
                        acc.len() - 1
                    }
                };
                // Index only what the story actually kept: `profile_merge` caps
                // the profile, and a phrase evicted from it can never match.
                for k in &keys {
                    if corpus.is_common(k) || !acc[target].profile.contains_key(k.as_str()) {
                        continue;
                    }
                    let e = index.entry(k.clone()).or_default();
                    if !e.contains(&target) {
                        e.push(target);
                    }
                }
            }

            tx.execute("UPDATE articles SET story_id=NULL", [])?;
            tx.execute("DELETE FROM story_summaries", [])?;
            tx.execute("DELETE FROM stories", [])?;
            let mut ins = tx.prepare(
                "INSERT INTO stories(title,profile,first_at,last_at,article_count,facts)
                 VALUES(?1,?2,?3,?4,?5,?6)",
            )?;
            let mut link = tx.prepare("UPDATE articles SET story_id=?1 WHERE id=?2")?;
            let mut stories = 0i64;
            let mut grouped = 0i64;
            for a in &acc {
                ins.execute(params![
                    a.title,
                    cluster::profile_to_json(&a.profile),
                    a.first_at,
                    a.last_at,
                    a.count as i64,
                    serde_json::to_string(&a.facts).unwrap_or_else(|_| "{}".into())
                ])?;
                let sid = tx.last_insert_rowid();
                for m in &a.members {
                    link.execute(params![sid, m])?;
                }
                stories += 1;
                if a.count >= 2 {
                    grouped += a.count as i64;
                }
            }
            drop(ins);
            drop(link);
            tx.commit()?;

            Ok(json!({
                "ok": true,
                "at": iso(started),
                "articles": articles.len(),
                "stories": stories,
                "multi_article_stories": acc.iter().filter(|a| a.count >= 2).count(),
                "articles_in_multi_stories": grouped,
                "skipped_digest": skipped_digest,
                "biggest": acc.iter().map(|a| a.count).max().unwrap_or(0),
                "took_sec": now_ts() - started,
            }))
        })
    }
}

/// FTS5 query builder: quote every term (implicit AND) so user input can never
/// be an FTS syntax error ("giá-vàng", "AND", quotes, …).
fn fts_query(q: &str) -> String {
    q.split_whitespace()
        .map(|t| format!("\"{}\"", t.replace('"', "")))
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> Db {
        Db::open_memory().unwrap()
    }

    fn add_src(db: &Db) -> i64 {
        db.add_source(
            "Test",
            "https://example.com/rss",
            "Tổng hợp",
            "vi",
            "",
            "feed",
        )
        .unwrap()
    }

    fn quick_article(db: &Db, src: i64, title: &str, url: &str, ts: i64) -> i64 {
        let id = db
            .insert_article(src, "", url, title, "mô tả", "", "", "", ts)
            .unwrap()
            .expect("new article");
        let candidates = db.recent_story_profiles(7, 300);
        let corpus = db.corpus_for(&cluster::key_phrases(title));
        let sid = cluster::assign_story(title, ts, &candidates, &corpus);
        db.place_in_story(id, title, ts, sid).unwrap();
        db.bump_phrase_df(title);
        id
    }

    #[test]
    fn source_crud_and_duplicate_url_rejected() {
        let db = db();
        let id = add_src(&db);
        assert!(db
            .add_source("Dup", "https://example.com/rss", "", "", "", "feed")
            .is_err());
        assert!(db
            .add_source("Bad", "not-a-url", "", "", "", "feed")
            .is_err());
        db.update_source(id, &json!({"status": "paused", "name": "Đổi tên"}))
            .unwrap();
        let s = db.get_source(id).unwrap();
        assert_eq!(s["status"], "paused");
        assert_eq!(s["name"], "Đổi tên");
        assert!(db.update_source(id, &json!({"status": "bogus"})).is_err());
    }

    #[test]
    fn source_kind_defaults_to_feed_and_is_validated() {
        let db = db();
        // Legacy callers pass "" — that must mean feed, not a broken source.
        let a = db
            .add_source("A", "https://a.vn/rss", "", "vi", "", "")
            .unwrap();
        assert_eq!(db.get_source(a).unwrap()["kind"], "feed");

        let b = db
            .add_source("B", "https://b.vn/thoi-su", "", "vi", "", "scrape")
            .unwrap();
        assert_eq!(db.get_source(b).unwrap()["kind"], "scrape");

        assert!(db
            .add_source("C", "https://c.vn", "", "", "", "gõ nhầm")
            .is_err());
        db.update_source(a, &json!({"kind": "scrape"})).unwrap();
        assert_eq!(db.get_source(a).unwrap()["kind"], "scrape");
        assert!(db.update_source(a, &json!({"kind": "bogus"})).is_err());
    }

    #[test]
    fn known_urls_reports_only_stored_ones() {
        let db = db();
        let src = add_src(&db);
        quick_article(
            &db,
            src,
            "Tin một",
            "https://example.com/tin-mot-1.html",
            1_780_000_000,
        );
        let asked = vec![
            "https://example.com/tin-mot-1.html".to_string(),
            "https://example.com/chua-co-2.html".to_string(),
        ];
        let known = db.known_urls(&asked);
        assert_eq!(known.len(), 1);
        assert!(known.contains(&asked[0]));
        assert!(db.known_urls(&[]).is_empty());
    }

    #[test]
    fn rejected_urls_remembered_and_scoped_to_source() {
        let db = db();
        let src = add_src(&db);
        let section = "https://example.com/bat-dong-san.htm".to_string();
        let article = "https://example.com/tin-that-123.htm".to_string();
        db.mark_rejected(src, std::slice::from_ref(&section));

        let asked = vec![section.clone(), article.clone()];
        let r = db.rejected_urls(&asked);
        assert_eq!(r.len(), 1);
        assert!(
            r.contains(&section),
            "trang chuyên mục phải được nhớ để khỏi mở lại"
        );
        assert!(!r.contains(&article));

        // Xoá nguồn thì quên luôn — nguồn thêm lại được quét sạch từ đầu.
        db.delete_source(src).unwrap();
        assert!(db.rejected_urls(&asked).is_empty());
    }

    #[test]
    fn cleanup_expires_reject_memory() {
        let db = db();
        let src = add_src(&db);
        let u = "https://example.com/muc-luc.htm".to_string();
        db.mark_rejected(src, std::slice::from_ref(&u));
        // Retention chưa tới hạn → vẫn nhớ.
        db.cleanup(30).unwrap();
        assert_eq!(db.rejected_urls(std::slice::from_ref(&u)).len(), 1);
        // Hết hạn → quên, để trang đổi markup còn có cơ hội được xét lại.
        db.with(|c| {
            c.execute(
                "UPDATE scrape_rejects SET at=?1",
                params![now_ts() - 100 * 86400],
            )?;
            Ok(())
        })
        .unwrap();
        db.cleanup(30).unwrap();
        assert!(db.rejected_urls(std::slice::from_ref(&u)).is_empty());
    }

    #[test]
    fn article_dedup_by_url() {
        let db = db();
        let src = add_src(&db);
        let a = db
            .insert_article(
                src,
                "g1",
                "https://x.vn/1",
                "Tin một",
                "d",
                "",
                "",
                "",
                1000,
            )
            .unwrap();
        assert!(a.is_some());
        let b = db
            .insert_article(
                src,
                "g1",
                "https://x.vn/1",
                "Tin một",
                "d",
                "",
                "",
                "",
                1000,
            )
            .unwrap();
        assert!(b.is_none(), "same url must dedup");
    }

    #[test]
    fn fts_search_finds_without_diacritics() {
        let db = db();
        let src = add_src(&db);
        db.insert_article(
            src,
            "",
            "https://x.vn/vang",
            "Giá vàng lập đỉnh mới",
            "vàng miếng tăng mạnh",
            "",
            "",
            "",
            now_ts(),
        )
        .unwrap();
        let hits = db.list_articles(Some("gia vang"), None, None, None, None, None, None, 10, 0);
        assert_eq!(hits.len(), 1);
        let none = db.list_articles(Some("bóng đá"), None, None, None, None, None, None, 10, 0);
        assert!(none.is_empty());
        // hostile FTS syntax must not error
        let weird = db.list_articles(Some("\"AND (giá OR"), None, None, None, None, None, None, 10, 0);
        let _ = weird;
    }

    #[test]
    fn topics_keyword_assignment() {
        let db = db();
        let src = add_src(&db);
        let t = db
            .add_topic("Kinh tế", "giá vàng, chứng khoán", "gold")
            .unwrap();
        let id = db
            .insert_article(
                src,
                "",
                "https://x.vn/2",
                "Giá vàng hôm nay tăng",
                "",
                "",
                "",
                "",
                now_ts(),
            )
            .unwrap()
            .unwrap();
        db.assign_topics(id, "Giá vàng hôm nay tăng", "");
        let arts = db.list_articles(None, None, Some(t), None, None, None, None, 10, 0);
        assert_eq!(arts.len(), 1);
        // editing keywords + reassign
        db.update_topic(t, &json!({"keywords": "bóng đá"})).unwrap();
        let n = db.reassign_topic(t, 0).unwrap();
        assert_eq!(n, 0);
        assert!(db
            .list_articles(None, None, Some(t), None, None, None, None, 10, 0)
            .is_empty());
    }

    #[test]
    fn system_one_verdicts_queue_replace_and_filter() {
        let db = db();
        let src = add_src(&db);
        let econ = db.add_topic("Kinh tế", "giá vàng", "gold").unwrap();
        let gold = db.add_topic("Vàng", "giá vàng", "gold").unwrap();
        let mut ids = Vec::new();
        for (i, title) in ["Giá vàng hôm nay tăng", "Khởi tố vụ lừa đảo giá vàng", "Bài cũ giá vàng"]
            .iter()
            .enumerate()
        {
            let ts = if i == 2 { now_ts() - 30 * 86400 } else { now_ts() - 60 };
            let id = db
                .insert_article(src, "", &format!("https://s.vn/{i}"), title, "", "", "", "", ts)
                .unwrap()
                .unwrap();
            db.assign_topics(id, title, "");
            ids.push(id);
        }
        let week = now_ts() - 7 * 86400;
        // Only the window is queued, newest first; the month-old article is not.
        let mut pending: Vec<i64> = db.pending_sections(week, 10).iter().map(|p| p.0).collect();
        pending.sort();
        assert_eq!(pending, vec![ids[0], ids[1]]);
        assert_eq!(db.section_progress(week), (0, 2));

        // Confident nested verdict replaces the keyword tags with both topics.
        db.apply_section(ids[0], "Kinh tế", 0.95, Some(&[econ, gold])).unwrap();
        // Confident built-in section clears the keyword tags entirely.
        db.apply_section(ids[1], "Pháp luật", 0.9, Some(&[])).unwrap();
        let topics_of = |id: i64| -> Vec<i64> {
            let a = db.get_article(id).unwrap();
            a["topics"].as_array().unwrap().iter().map(|t| t["id"].as_i64().unwrap()).collect()
        };
        assert_eq!(topics_of(ids[0]), vec![econ, gold]);
        assert!(topics_of(ids[1]).is_empty());
        // A failed call (`None`) leaves the keyword tags alone.
        assert_eq!(topics_of(ids[2]).len(), 2);
        db.apply_section(ids[2], "", 0.0, None).unwrap();
        assert_eq!(topics_of(ids[2]).len(), 2);
        // A verdict for an article that is gone writes nothing.
        db.apply_section(999_999, "Kinh tế", 0.9, Some(&[econ])).unwrap();
        let orphans: i64 = db
            .with(|c| Ok(c.query_row("SELECT COUNT(*) FROM article_topics WHERE article_id=999999", [], |r| r.get(0))?))
            .unwrap();
        assert_eq!(orphans, 0);

        assert!(db.pending_sections(week, 10).is_empty());
        assert_eq!(db.section_progress(week), (2, 0));
        let law = db.list_articles(None, None, None, None, None, Some("Pháp luật"), None, 10, 0);
        assert_eq!(law.len(), 1);
        assert_eq!(law[0]["section"], "Pháp luật");
        assert_eq!(
            db.section_counts(0),
            vec![json!({"section": "Kinh tế", "count": 1}), json!({"section": "Pháp luật", "count": 1})]
        );

        // A topic edit puts the window back in the queue, and only the window.
        assert_eq!(db.requeue_sections(week), 2);
        assert_eq!(db.section_progress(week), (0, 2));
    }

    #[test]
    fn story_clustering_builds_timeline() {
        let db = db();
        let src = add_src(&db);
        let t0 = now_ts() - 3600;
        let a1 = quick_article(
            &db,
            src,
            "Bão số 3 đổ bộ Quảng Ninh, dân sơ tán khẩn cấp",
            "https://x.vn/b1",
            t0,
        );
        let a2 = quick_article(
            &db,
            src,
            "Quảng Ninh sơ tán hàng nghìn dân trước bão số 3",
            "https://x.vn/b2",
            t0 + 600,
        );
        let _unrelated = quick_article(
            &db,
            src,
            "Giá vàng lập đỉnh mới trên thị trường",
            "https://x.vn/v1",
            t0 + 700,
        );

        let s1 = db.get_article(a1).unwrap()["story_id"].as_i64().unwrap();
        let s2 = db.get_article(a2).unwrap()["story_id"].as_i64().unwrap();
        assert_eq!(s1, s2, "same event must share a story");

        let story = db.get_story(s1).unwrap();
        assert_eq!(story["article_count"], 2);
        let tl = story["timeline"].as_array().unwrap();
        assert_eq!(tl.len(), 2);
        assert_eq!(tl[0]["id"], a2, "timeline is newest-first");
        assert_eq!(tl[1]["id"], a1);

        let stories = db.list_stories(0, 2, 10);
        assert_eq!(stories.len(), 1, "single-article stories are hidden");
        assert_eq!(db.related_articles(a1, 10).len(), 1);
    }

    #[test]
    fn analysis_cache_roundtrip() {
        let db = db();
        let src = add_src(&db);
        let id = db
            .insert_article(src, "", "https://x.vn/3", "Tin", "", "", "", "", now_ts())
            .unwrap()
            .unwrap();
        db.save_analysis(
            id,
            "tóm tắt",
            "neutral",
            4,
            true,
            "nguồn chính thống",
            &["vàng".into(), "kinh tế".into()],
            "m1",
        )
        .unwrap();
        let a = db.get_analysis(id).unwrap();
        assert_eq!(a["importance"], 4);
        assert_eq!(a["clickbait"], true);
        assert_eq!(a["tags"].as_array().unwrap().len(), 2);
        // upsert overwrites
        db.save_analysis(id, "mới", "positive", 2, false, "", &[], "m2")
            .unwrap();
        assert_eq!(db.get_analysis(id).unwrap()["summary"], "mới");
    }

    #[test]
    fn summaries_are_kept_as_history() {
        let db = db();
        let src = add_src(&db);
        let ts = now_ts();
        quick_article(&db, src, "Bão số 3 đổ bộ Quảng Ninh, hàng nghìn hộ dân sơ tán", "https://e.vn/1", ts);
        quick_article(&db, src, "Quảng Ninh sơ tán dân trước khi bão số 3 đổ bộ", "https://e.vn/2", ts + 60);
        let sid = db.list_stories(0, 2, 5)[0]["id"].as_i64().unwrap();

        db.set_story_summary(sid, "bản đọc thứ nhất", "model-a").unwrap();
        db.set_story_summary(sid, "bản đọc thứ hai", "model-b").unwrap();

        let story = db.get_story(sid).unwrap();
        assert_eq!(story["summary"], "bản đọc thứ hai", "bản mới nhất là bản hiện hành");
        let hist = story["summaries"].as_array().unwrap();
        assert_eq!(hist.len(), 2, "lần tóm tắt trước phải còn xem lại được");
        assert_eq!(hist[0]["summary"], "bản đọc thứ hai");
        assert_eq!(hist[1]["summary"], "bản đọc thứ nhất");
        assert_eq!(hist[1]["model"], "model-a");
        assert_eq!(hist[0]["article_count"], 2);
    }

    #[test]
    fn digest_pages_stay_out_of_stories() {
        let db = db();
        let src = add_src(&db);
        let ts = now_ts();
        let id = db
            .insert_article(
                src,
                "",
                "https://e.vn/diem-tin",
                "Điểm tin 6h: TP.HCM đầu tư 7.000 tỷ chống ngập",
                "",
                "",
                "",
                "",
                ts,
            )
            .unwrap()
            .unwrap();
        let placed = db
            .place_in_story(id, "Điểm tin 6h: TP.HCM đầu tư 7.000 tỷ chống ngập", ts, None)
            .unwrap();
        assert_eq!(placed, None, "trang điểm tin không được mở dòng sự kiện");
        assert!(db.list_stories(0, 1, 10).is_empty());
    }

    #[test]
    fn rebuild_regroups_and_marks_version() {
        let db = db();
        let src = add_src(&db);
        let ts = now_ts();
        for (i, t) in [
            "Bão số 3 đổ bộ Quảng Ninh, hàng nghìn hộ dân sơ tán",
            "Quảng Ninh sơ tán dân trước khi bão số 3 đổ bộ",
            "Giá vàng lập đỉnh mới, vượt 100 triệu đồng mỗi lượng",
        ]
        .iter()
        .enumerate()
        {
            quick_article(&db, src, t, &format!("https://e.vn/r{i}"), ts + i as i64);
        }
        assert!(db.regroup_due(), "phiên bản thuật toán mới thì phải gom lại");
        let r = db.rebuild_stories().unwrap();
        assert_eq!(r["multi_article_stories"], 1);
        assert_eq!(r["stories"], 2, "bão gom một dòng, vàng đứng riêng");
        assert!(!db.regroup_due(), "gom xong thì không đến hạn nữa");
    }

    #[test]
    fn translations_are_cached_per_language() {
        let db = db();
        let src = add_src(&db);
        let ts = now_ts();
        let id = quick_article(&db, src, "Bão số 3 đổ bộ Quảng Ninh, dân sơ tán", "https://e.vn/t1", ts);
        db.save_translation(id, "English", "Typhoon No.3 hits Quang Ninh", "residents evacuated")
            .unwrap();
        let got = db.translations_for(&[id], "English");
        assert_eq!(got[&id].0, "Typhoon No.3 hits Quang Ninh");
        assert!(
            db.translations_for(&[id], "Français").is_empty(),
            "bản dịch của ngôn ngữ này không dùng cho ngôn ngữ khác"
        );
    }

    #[test]
    fn cleanup_drops_old_articles_and_empty_stories() {
        let db = db();
        let src = add_src(&db);
        let old_ts = now_ts() - 90 * 86400;
        let id = db
            .insert_article(
                src,
                "",
                "https://x.vn/old",
                "Bài rất cũ về sự kiện xưa",
                "",
                "",
                "",
                "",
                old_ts,
            )
            .unwrap()
            .unwrap();
        db.place_in_story(id, "Bài rất cũ về sự kiện xưa", old_ts, None)
            .unwrap();
        let fresh = db
            .insert_article(
                src,
                "",
                "https://x.vn/new",
                "Bài mới",
                "",
                "",
                "",
                "",
                now_ts(),
            )
            .unwrap()
            .unwrap();
        let removed = db.cleanup(30).unwrap();
        assert_eq!(removed, 1);
        assert!(db.get_article(id).is_none());
        assert!(db.get_article(fresh).is_some());
        assert!(db.list_stories(0, 1, 10).is_empty(), "empty story swept");
    }

    #[test]
    fn repair_entities_fixes_old_rows_and_is_idempotent() {
        let db = db();
        let src = add_src(&db);
        let id = db
            .insert_article(
                src,
                "",
                "https://x.vn/e",
                "C&ocirc;ng ty TTC",
                "xem x&eacute;t hồ sơ",
                "",
                "",
                "",
                now_ts(),
            )
            .unwrap()
            .unwrap();
        db.set_article_content(
            id,
            "Đoạn m&ocirc;̣t.\n\nBi&ecirc;n bản đ&atilde; ho&agrave;n th&agrave;nh.",
        )
        .unwrap();

        assert_eq!(db.repair_entities(), 1);
        let a = db.get_article(id).unwrap();
        assert_eq!(a["title"], "Công ty TTC");
        assert_eq!(a["description"], "xem xét hồ sơ");
        let content = a["content"].as_str().unwrap();
        assert!(
            content.contains("Biên bản đã hoàn thành"),
            "got {content:?}"
        );
        assert!(
            content.contains("\n\n"),
            "toàn văn phải giữ xuống dòng: {content:?}"
        );

        // FTS đã được viết lại theo nội dung mới.
        assert_eq!(
            db.list_articles(Some("cong ty"), None, None, None, None, None, None, 10, 0)
                .len(),
            1
        );
        // Chạy lại không đổi gì nữa.
        assert_eq!(db.repair_entities(), 0);
    }

    #[test]
    fn settings_and_activity() {
        let db = db();
        assert_eq!(db.setting("fetch_interval_min", "30"), "30");
        db.set_setting("fetch_interval_min", "15").unwrap();
        assert_eq!(db.setting("fetch_interval_min", "30"), "15");
        db.log("fetch", "thu thập 5 bài", "");
        assert_eq!(db.recent_activity(10).len(), 1);
    }

    #[test]
    fn seed_populates_sources_and_topics() {
        let db = db();
        db.seed_if_empty();
        assert!(db.list_sources(None).len() >= 5);
        assert_eq!(db.list_topics().len(), 3);
        // seeding twice must not duplicate
        db.seed_if_empty();
        assert_eq!(db.list_topics().len(), 3);
    }
}

/// Manual check against a real archive, not part of the suite:
///   NEWS_DB=/path/to/news.db cargo test -p news -- --ignored --nocapture real_archive
#[cfg(test)]
mod real_archive {
    use super::*;

    #[test]
    #[ignore]
    fn rebuild_on_a_real_database() {
        let Ok(path) = std::env::var("NEWS_DB") else {
            eprintln!("set NEWS_DB to run this");
            return;
        };
        let db = Db::open(PathBuf::from(path)).unwrap();
        let report = db.rebuild_stories().unwrap();
        println!("{}", serde_json::to_string_pretty(&report).unwrap());
        let big = db.list_stories(0, 2, 12);
        for s in &big {
            println!("{:>5} bài  {} → {}  {}", s["article_count"], s["first_at"], s["last_at"], s["title"]);
        }
    }
}

