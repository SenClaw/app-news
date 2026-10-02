//! HTTP API of the News app. Every handler funnels through `*_value` helpers
//! that the MCP server ([`crate::mcp`]) reuses, so REST and agent tools always
//! behave identically. The collect pipeline (fetch → dedup → topics → story
//! clustering) also lives here: [`fetch_all_value`] is what the background
//! scheduler in `main.rs` calls on its interval.

use crate::cluster;
use crate::db::{now_ts, Db};
use crate::fetch;
use crate::laya;
use crate::llm;
use app_space_sdk::SpaceClient;
use axum::{
    extract::{Path, Query, State},
    routing::{get, post},
    Json, Router,
};
use futures_util::StreamExt;
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;

#[derive(Clone)]
pub struct AppState {
    pub db: Arc<Db>,
    pub sc: SpaceClient,
    pub http: reqwest::Client,
    /// Fan-out of MCP JSON-RPC responses to any connected SSE client.
    pub mcp_tx: tokio::sync::broadcast::Sender<String>,
    /// Long-running work in flight, keyed by job id. Lives on the SERVER, not
    /// in a component's state, so "đang xử lý" survives a tab switch, a reload
    /// or a second window — and an agent's MCP call shows up in the UI too.
    pub jobs: Arc<std::sync::Mutex<std::collections::BTreeMap<String, Value>>>,
}

/// Removes its job from the registry when dropped — including on early return
/// or panic, so a failed AI call can never leave a phantom "đang xử lý".
pub struct JobGuard {
    jobs: Arc<std::sync::Mutex<std::collections::BTreeMap<String, Value>>>,
    key: String,
}

impl Drop for JobGuard {
    fn drop(&mut self) {
        if let Ok(mut g) = self.jobs.lock() {
            g.remove(&self.key);
        }
    }
}

impl AppState {
    /// Register a job under `key` (one slot per kind of work, so two clicks on
    /// the same button don't show up as two jobs). `label` is what the user
    /// reads in the UI.
    pub fn track_job(&self, key: &str, kind: &str, label: &str) -> JobGuard {
        if let Ok(mut g) = self.jobs.lock() {
            g.insert(
                key.to_string(),
                json!({ "key": key, "kind": kind, "label": label, "started_at": now_ts() }),
            );
        }
        JobGuard { jobs: self.jobs.clone(), key: key.to_string() }
    }

    /// Snapshot of running jobs, oldest first, with elapsed seconds filled in.
    pub fn jobs_snapshot(&self) -> Vec<Value> {
        let now = now_ts();
        let mut list: Vec<Value> = self
            .jobs
            .lock()
            .map(|g| g.values().cloned().collect())
            .unwrap_or_default();
        for j in list.iter_mut() {
            let started = j["started_at"].as_i64().unwrap_or(now);
            j["elapsed_sec"] = json!((now - started).max(0));
        }
        list.sort_by_key(|j| j["started_at"].as_i64().unwrap_or(0));
        list
    }

    pub fn job_running(&self, key: &str) -> Option<Value> {
        let now = now_ts();
        self.jobs.lock().ok().and_then(|g| {
            g.get(key).map(|j| {
                let mut j = j.clone();
                j["elapsed_sec"] = json!((now - j["started_at"].as_i64().unwrap_or(now)).max(0));
                j
            })
        })
    }
}

pub fn make_state() -> AppState {
    let db = Arc::new(Db::open_default().expect("open news db"));
    let (mcp_tx, _) = tokio::sync::broadcast::channel(100);
    AppState {
        db,
        sc: SpaceClient::from_env(),
        http: fetch::http_client(),
        mcp_tx,
        jobs: Arc::new(std::sync::Mutex::new(Default::default())),
    }
}

pub fn api_router(state: AppState) -> Router {
    Router::new()
        .route("/status", get(status))
        .route("/dashboard", get(dashboard))
        .route("/sources", get(list_sources).post(add_source))
        .route("/sources/:id", post(update_source))
        .route("/sources/:id/delete", post(delete_source))
        .route("/sources/:id/fetch", post(fetch_one))
        .route("/fetch", post(fetch_all))
        .route("/articles", get(list_articles))
        .route("/articles/:id", get(get_article))
        .route("/articles/:id/content", post(fetch_content))
        .route("/articles/:id/analyze", post(analyze_article))
        .route("/topics", get(list_topics).post(add_topic))
        .route("/topics/:id", post(update_topic))
        .route("/topics/:id/delete", post(delete_topic))
        .route("/sections", get(sections))
        .route("/trends", get(trends))
        .route("/trends/analyze", post(analyze_trends))
        .route("/trends/summary", post(trend_summary))
        .route("/stories", get(list_stories))
        .route("/stories/graph", get(story_graph))
        .route("/stories/graph/analyze", post(analyze_graph))
        .route("/stories/rebuild", post(rebuild_stories))
        .route("/stories/:id", get(get_story))
        .route("/stories/:id/brief", post(story_brief))
        .route("/stories/:id/translate", post(translate_story))
        .route("/sources/discover", post(discover_sources))
        .route("/digest", post(digest))
        .route("/digests", get(digest_history))
        .route("/digests/:id", get(get_digest))
        .route("/digests/:id/delete", post(delete_digest))
        .route("/jobs", get(jobs))
        .route("/settings", get(get_settings).post(set_settings))
        .route("/activity", get(activity))
        // MCP (HTTP + SSE), same shape as the other Space Apps.
        .route(
            "/mcp/sse",
            get(crate::mcp::mcp_sse).post(crate::mcp::mcp_message),
        )
        .route("/mcp/message", post(crate::mcp::mcp_message))
        .with_state(state)
}

// ---- status / dashboard ----

pub(crate) fn status_value(s: &AppState) -> Value {
    let d = s.db.dashboard();
    json!({
        "ok": true,
        "app": "news",
        "articles_total": d["articles_total"],
        "articles_24h": d["articles_24h"],
        "sources_active": d["sources_active"],
        "sources_error": d["sources_error"],
        "last_fetch_at": d["last_fetch_at"],
        "laya": laya::status_json(&s.db),
    })
}

async fn status(State(s): State<AppState>) -> Json<Value> {
    Json(status_value(&s))
}

pub(crate) fn dashboard_value(s: &AppState) -> Value {
    let mut d = s.db.dashboard();
    let now = now_ts();
    d["trends"] = trends_value(s, 48)["trends"].clone();
    d["hot_stories"] = json!(s.db.list_stories(now - 72 * 3600, 2, 6));
    d["recent_articles"] = json!(s.db.list_articles(None, None, None, None, None, None, None, 8, 0));
    d
}

async fn dashboard(State(s): State<AppState>) -> Json<Value> {
    Json(dashboard_value(&s))
}

// ---- collect pipeline ----

/// Ingest one parsed feed item: insert (dedup), assign topics, place in story.
/// Returns true when the article was new.
fn ingest_item(db: &Db, source_id: i64, it: &fetch::FeedItem) -> bool {
    let inserted = db.insert_article(
        source_id,
        &it.guid,
        &it.url,
        &it.title,
        &it.description,
        &it.image_url,
        &it.author,
        &it.category,
        it.published_at,
    );
    let Ok(Some(id)) = inserted else { return false };
    db.assign_topics(id, &it.title, &it.description);
    let ts = if it.published_at > 0 {
        it.published_at
    } else {
        now_ts()
    };
    let candidates = db.recent_story_profiles(7, 300);
    let corpus = db.corpus_for(&cluster::key_phrases(&it.title));
    let sid = cluster::assign_story(&it.title, ts, &candidates, &corpus);
    let _ = db.place_in_story(id, &it.title, ts, sid);
    db.bump_phrase_df(&it.title);
    true
}

/// How many article pages one scrape cycle may open. Only links never seen
/// before are opened, so this bounds the FIRST fetch of a source (a listing
/// page is entirely new) — the steady state is a handful per cycle no matter
/// what this is set to. Sized to cover a whole listing page in one go, because
/// anything left over waits for the next cycle.
const SCRAPE_ENRICH_PER_CYCLE: usize = 60;

/// Is this Open Graph title usable as the headline?
///
/// Some sites template `og:title` to the bare site name. Real headlines are
/// sentences, so the same shape test the link scanner uses on anchor text
/// separates them.
fn usable_og_title(t: &str) -> bool {
    t.chars().count() >= 15 && t.split_whitespace().count() >= 3
}

/// Scrape ONE listing page: harvest article links, then open the *new* ones to
/// read their `<head>` for a real title, summary, image and publish date.
///
/// Returns `(items_to_ingest, deferred, rejected)`. Only enriched items that
/// the page itself confirms are articles get through:
///   * un-enriched — storing a link with no date and a teaser for a title would
///     fix that damage permanently, since the next cycle sees the URL as known
///     and never revisits it. Over-budget links are deferred instead; they are
///     still on the listing page next cycle, and arrive complete.
///   * not an article — listing pages link to other listing pages, whose slugs
///     look exactly like article slugs ([`fetch::PageMeta::is_article`]).
async fn scrape_source_items(
    s: &AppState,
    source_id: i64,
    url: &str,
    html: &str,
) -> (Vec<fetch::FeedItem>, usize, usize) {
    let found = fetch::scan_page_articles(html, url);
    if found.is_empty() {
        return (found, 0, 0);
    }
    let urls: Vec<String> = found.iter().map(|i| i.url.clone()).collect();
    let known = s.db.known_urls(&urls);
    // Section pages stay on the listing forever. Without this they would be
    // re-opened, re-rejected and re-counted every single cycle.
    let rejects = s.db.rejected_urls(&urls);

    let mut fresh: Vec<fetch::FeedItem> = found
        .into_iter()
        .filter(|i| !known.contains(&i.url) && !rejects.contains(&i.url))
        .collect();
    let deferred = fresh.len().saturating_sub(SCRAPE_ENRICH_PER_CYCLE);
    fresh.truncate(SCRAPE_ENRICH_PER_CYCLE);

    // Each link resolves to Ok(article) | Err(Some(url)) "confirmed not an
    // article, remember it" | Err(None) "page did not load, try again later".
    let outcomes: Vec<Result<fetch::FeedItem, Option<String>>> =
        futures_util::stream::iter(fresh.into_iter().map(|mut it| {
            let http = s.http.clone();
            async move {
                let Ok(m) = fetch::fetch_page_meta(&http, &it.url).await else {
                    return Err(None);
                };
                if !m.is_article() {
                    return Err(Some(it.url));
                }
                // The publisher's own headline beats anchor text, which on many
                // layouts is the teaser paragraph rather than the title.
                if usable_og_title(&m.title) {
                    it.title = m.title;
                }
                it.description = fetch::clip(&m.description, 600);
                it.image_url = m.image_url;
                it.author = m.author;
                it.published_at = m.published_at;
                Ok(it)
            }
        }))
        .buffer_unordered(5)
        .collect()
        .await;

    let mut items = Vec::new();
    let mut not_articles = Vec::new();
    for o in outcomes {
        match o {
            Ok(it) => items.push(it),
            Err(Some(u)) => not_articles.push(u),
            Err(None) => {}
        }
    }
    let rejected = not_articles.len();
    s.db.mark_rejected(source_id, &not_articles);
    (items, deferred, rejected)
}

/// Fetch ONE source end-to-end. Returns `{ok, new, skipped}` or `{error}`.
pub(crate) async fn fetch_source_value(s: &AppState, id: i64) -> Value {
    let Some(src) = s.db.get_source(id) else {
        return json!({ "error": format!("nguồn #{id} không tồn tại") });
    };
    let url = src["url"].as_str().unwrap_or("").to_string();
    let (etag, lm) = s.db.source_fetch_meta(id).unwrap_or_default();
    if src["kind"].as_str() == Some("scrape") {
        return scrape_source_value(s, id, &src, &url, &etag, &lm).await;
    }

    match fetch::fetch_feed(&s.http, &url, &etag, &lm).await {
        Ok(out) => {
            let (mut new, mut skipped) = (0i64, 0i64);
            if let Some(items) = &out.items {
                for it in items {
                    if ingest_item(&s.db, id, it) {
                        new += 1;
                    } else {
                        skipped += 1;
                    }
                }
            }
            s.db.mark_source_fetch(id, &out.etag, &out.last_modified, "ok", "");
            // A source added with just a URL gets its real name from the feed.
            let cur_name = src["name"].as_str().unwrap_or("");
            if !out.feed_title.is_empty() && cur_name == url {
                let _ = s.db.update_source(id, &json!({ "name": out.feed_title }));
            }
            if new > 0 {
                s.db.log(
                    "fetch",
                    &format!(
                        "thu thập {} bài mới từ \"{}\"",
                        new,
                        src["name"].as_str().unwrap_or("?")
                    ),
                    &id.to_string(),
                );
            }
            json!({ "ok": true, "source_id": id, "new": new, "skipped": skipped, "not_modified": out.items.is_none() })
        }
        Err(e) => {
            s.db.mark_source_fetch(id, &etag, &lm, "error", &e.to_string());
            json!({ "error": format!("nguồn \"{}\": {}", src["name"].as_str().unwrap_or("?"), e), "source_id": id })
        }
    }
}

/// The `kind = "scrape"` half of [`fetch_source_value`]: the source URL is an
/// ordinary page, so links are harvested from its HTML instead of parsed from
/// XML. Everything downstream (dedupe, topics, story clustering) is identical —
/// a scraped item is just a [`fetch::FeedItem`] like any other.
async fn scrape_source_value(
    s: &AppState,
    id: i64,
    src: &Value,
    url: &str,
    etag: &str,
    lm: &str,
) -> Value {
    let name = src["name"].as_str().unwrap_or("?");
    match fetch::fetch_html(&s.http, url, etag, lm).await {
        Ok(out) => {
            let Some(html) = out.html else {
                s.db.mark_source_fetch(id, &out.etag, &out.last_modified, "ok", "");
                return json!({ "ok": true, "source_id": id, "new": 0, "skipped": 0, "not_modified": true });
            };
            // "No links at all on the page" is a configuration problem the
            // user must see; "links, but all already stored" is a normal quiet
            // cycle. Only the first is an error.
            if fetch::scan_page_articles(&html, url).is_empty() {
                let err =
                    "không tìm thấy link bài viết nào trên trang (trang có thể cần JavaScript, \
                           hoặc hãy trỏ vào trang chuyên mục thay vì trang chủ)";
                s.db.mark_source_fetch(id, &out.etag, &out.last_modified, "error", err);
                return json!({ "error": format!("nguồn \"{name}\": {err}"), "source_id": id });
            }
            let (items, deferred, rejected) = scrape_source_items(s, id, url, &html).await;
            // This source has never produced a single article, and this cycle
            // did not either — the URL is wrong (a table of contents rather
            // than a list of stories). Reporting "0 bài mới" forever would hide
            // that. Keyed on the source's lifetime total, not on this cycle, so
            // a healthy source having a quiet cycle never trips it.
            let ever = src["article_count"].as_i64().unwrap_or(0);
            if ever == 0 && items.is_empty() {
                let err = format!(
                    "quét được link nhưng không link nào là trang bài viết \
                     ({rejected} link thiếu og:type=article và ngày đăng) — URL này có lẽ là \
                     trang mục lục, thử trỏ vào trang chuyên mục có danh sách bài"
                );
                s.db.mark_source_fetch(id, &out.etag, &out.last_modified, "error", &err);
                return json!({ "error": format!("nguồn \"{name}\": {err}"), "source_id": id });
            }
            let (mut new, mut skipped) = (0i64, 0i64);
            for it in &items {
                if ingest_item(&s.db, id, it) {
                    new += 1;
                } else {
                    skipped += 1;
                }
            }
            s.db.mark_source_fetch(id, &out.etag, &out.last_modified, "ok", "");
            if src["name"].as_str() == Some(url) {
                let t = fetch::page_title(&html);
                if !t.trim().is_empty() {
                    let _ =
                        s.db.update_source(id, &json!({ "name": fetch::clip(t.trim(), 80) }));
                }
            }
            if new > 0 {
                s.db.log(
                    "fetch",
                    &format!("quét trang: {new} bài mới từ \"{name}\""),
                    &id.to_string(),
                );
            }
            json!({
                "ok": true, "source_id": id, "new": new, "skipped": skipped,
                "not_modified": false, "scanned": items.len(),
                "deferred": deferred, "rejected": rejected,
            })
        }
        Err(e) => {
            s.db.mark_source_fetch(id, etag, lm, "error", &e.to_string());
            json!({ "error": format!("nguồn \"{name}\": {e}"), "source_id": id })
        }
    }
}

/// Fetch EVERY active source (6 at a time), then retention cleanup.
pub(crate) async fn fetch_all_value(s: &AppState) -> Value {
    let sources = s.db.sources_to_fetch();
    let total = sources.len();
    let _job = s.track_job("fetch", "fetch", &format!("Đang thu thập tin từ {total} nguồn"));
    let results: Vec<Value> = futures_util::stream::iter(sources.into_iter().map(|(id, ..)| {
        let s = s.clone();
        async move { fetch_source_value(&s, id).await }
    }))
    .buffer_unordered(6)
    .collect()
    .await;

    let new: i64 = results.iter().filter_map(|r| r["new"].as_i64()).sum();
    let errors: Vec<Value> = results
        .iter()
        .filter(|r| r.get("error").is_some())
        .map(|r| json!({ "source_id": r["source_id"], "error": r["error"] }))
        .collect();

    let retention: i64 = s.db.setting("retention_days", "30").parse().unwrap_or(30);
    let removed = s.db.cleanup(retention).unwrap_or(0);
    if new > 0 || !errors.is_empty() {
        s.db.log(
            "fetch",
            &format!("quét {total} nguồn: {new} bài mới, {} lỗi", errors.len()),
            "",
        );
    }
    json!({ "ok": true, "sources": total, "new": new, "errors": errors, "removed_old": removed })
}

async fn fetch_all(State(s): State<AppState>) -> Json<Value> {
    Json(fetch_all_value(&s).await)
}

async fn fetch_one(State(s): State<AppState>, Path(id): Path<i64>) -> Json<Value> {
    Json(fetch_source_value(&s, id).await)
}

// ---- sources ----

#[derive(Deserialize, Default)]
pub(crate) struct SourceIn {
    pub name: String,
    pub url: String,
    #[serde(default)]
    pub category: String,
    #[serde(default)]
    pub lang: String,
    #[serde(default)]
    pub note: String,
    /// "feed" (RSS/Atom, default) or "scrape" (harvest links from the page).
    #[serde(default)]
    pub kind: String,
}

pub(crate) fn add_source_value(s: &AppState, b: &SourceIn) -> Value {
    let name = if b.name.trim().is_empty() {
        b.url.trim()
    } else {
        b.name.trim()
    };
    match s
        .db
        .add_source(name, &b.url, &b.category, &b.lang, &b.note, &b.kind)
    {
        Ok(id) => {
            s.db.log("source", &format!("thêm nguồn \"{name}\""), &id.to_string());
            json!({ "ok": true, "source": s.db.get_source(id) })
        }
        Err(e) => json!({ "error": e.to_string() }),
    }
}

async fn add_source(State(s): State<AppState>, Json(b): Json<SourceIn>) -> Json<Value> {
    Json(add_source_value(&s, &b))
}

#[derive(Deserialize)]
struct StatusQuery {
    status: Option<String>,
}

pub(crate) fn list_sources_value(s: &AppState, status: Option<&str>) -> Value {
    json!({ "sources": s.db.list_sources(status) })
}

async fn list_sources(State(s): State<AppState>, Query(q): Query<StatusQuery>) -> Json<Value> {
    Json(list_sources_value(&s, q.status.as_deref()))
}

pub(crate) fn update_source_value(s: &AppState, id: i64, patch: &Value) -> Value {
    match s.db.update_source(id, patch) {
        Ok(()) => {
            s.db.log("source", &format!("cập nhật nguồn #{id}"), &id.to_string());
            json!({ "ok": true, "source": s.db.get_source(id) })
        }
        Err(e) => json!({ "error": e.to_string() }),
    }
}

async fn update_source(
    State(s): State<AppState>,
    Path(id): Path<i64>,
    Json(patch): Json<Value>,
) -> Json<Value> {
    Json(update_source_value(&s, id, &patch))
}

pub(crate) fn delete_source_value(s: &AppState, id: i64) -> Value {
    match s.db.delete_source(id) {
        Ok(removed) => {
            s.db.log(
                "source",
                &format!("xoá nguồn #{id} ({removed} bài)"),
                &id.to_string(),
            );
            json!({ "ok": true, "removed_articles": removed })
        }
        Err(e) => json!({ "error": e.to_string() }),
    }
}

async fn delete_source(State(s): State<AppState>, Path(id): Path<i64>) -> Json<Value> {
    Json(delete_source_value(&s, id))
}

// ---- articles ----

#[derive(Deserialize, Default)]
pub(crate) struct ArticleQuery {
    pub q: Option<String>,
    pub source_id: Option<i64>,
    pub topic_id: Option<i64>,
    pub story_id: Option<i64>,
    pub category: Option<String>,
    /// Chuyên mục do System One chọn (xem laya.rs).
    pub section: Option<String>,
    /// Look-back window in hours (mutually additive with other filters).
    pub hours: Option<i64>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

pub(crate) fn list_articles_value(s: &AppState, q: &ArticleQuery) -> Value {
    let since = q.hours.filter(|h| *h > 0).map(|h| now_ts() - h * 3600);
    json!({
        "articles": s.db.list_articles(
            q.q.as_deref(),
            q.source_id,
            q.topic_id,
            q.story_id,
            q.category.as_deref(),
            q.section.as_deref(),
            since,
            q.limit.unwrap_or(50),
            q.offset.unwrap_or(0),
        )
    })
}

async fn list_articles(State(s): State<AppState>, Query(q): Query<ArticleQuery>) -> Json<Value> {
    Json(list_articles_value(&s, &q))
}

pub(crate) fn get_article_value(s: &AppState, id: i64) -> Value {
    match s.db.get_article(id) {
        Some(mut a) => {
            a["related"] = json!(s.db.related_articles(id, 10));
            json!({ "article": a })
        }
        None => json!({ "error": format!("bài #{id} không tồn tại") }),
    }
}

async fn get_article(State(s): State<AppState>, Path(id): Path<i64>) -> Json<Value> {
    Json(get_article_value(&s, id))
}

/// Fetch the article's own page and store readable text ("xem toàn văn").
pub(crate) async fn fetch_content_value(s: &AppState, id: i64) -> Value {
    let Some(a) = s.db.get_article(id) else {
        return json!({ "error": format!("bài #{id} không tồn tại") });
    };
    let existing = a["content"].as_str().unwrap_or("");
    if !existing.trim().is_empty() {
        return json!({ "ok": true, "article_id": id, "content": existing, "cached": true });
    }
    let url = a["url"].as_str().unwrap_or("");
    match fetch::fetch_page_text(&s.http, url).await {
        Ok(text) => {
            if let Err(e) = s.db.set_article_content(id, &text) {
                return json!({ "error": e.to_string() });
            }
            json!({ "ok": true, "article_id": id, "content": text, "cached": false })
        }
        Err(e) => json!({ "error": e.to_string(), "url": url }),
    }
}

async fn fetch_content(State(s): State<AppState>, Path(id): Path<i64>) -> Json<Value> {
    Json(fetch_content_value(&s, id).await)
}

#[derive(Deserialize, Default)]
pub(crate) struct AnalyzeIn {
    #[serde(default)]
    pub force: bool,
    /// Fetch full text first when the article only has a short description.
    #[serde(default)]
    pub with_content: bool,
}

/// AI đánh giá một bài (cached in `analyses` until force=true).
pub(crate) async fn analyze_article_value(s: &AppState, id: i64, b: &AnalyzeIn) -> Value {
    if !b.force {
        if let Some(cached) = s.db.get_analysis(id) {
            return json!({ "ok": true, "article_id": id, "analysis": cached, "cached": true });
        }
    }
    let _job = s.track_job(&format!("article:{id}"), "article", "Đang đánh giá một bài báo");
    if b.with_content {
        let _ = fetch_content_value(s, id).await; // best-effort; analysis works without it
    }
    let Some(a) = s.db.get_article(id) else {
        return json!({ "error": format!("bài #{id} không tồn tại") });
    };
    match llm::analyze_article(&s.sc, &a).await {
        Ok((v, model)) => {
            let _ = s.db.save_analysis(
                id,
                &v.summary,
                &v.sentiment,
                v.importance,
                v.clickbait,
                &v.reliability,
                &v.tags,
                &model,
            );
            s.db.log("ai", &format!("đánh giá bài #{id}"), &id.to_string());
            json!({ "ok": true, "article_id": id, "analysis": s.db.get_analysis(id), "cached": false })
        }
        Err(e) => json!({ "error": format!("không gọi được AI qua bridge SenClaw: {e}") }),
    }
}

async fn analyze_article(
    State(s): State<AppState>,
    Path(id): Path<i64>,
    body: Option<Json<AnalyzeIn>>,
) -> Json<Value> {
    let b = body.map(|Json(x)| x).unwrap_or_default();
    Json(analyze_article_value(&s, id, &b).await)
}

// ---- source discovery (AI gợi ý / autodiscovery từ trang web) ----

#[derive(Deserialize, Default)]
pub(crate) struct DiscoverIn {
    /// Chủ đề tự do ("tin công nghệ tiếng Việt") HOẶC URL một trang web.
    #[serde(default)]
    pub query: String,
    /// true = thêm luôn mọi nguồn đã validate; false = chỉ trả danh sách.
    #[serde(default)]
    pub auto_add: bool,
}

/// One validated source: it was actually fetched, and something came out.
struct FeedHit {
    url: String,
    title: String,
    item_count: usize,
    sample: Vec<String>,
    /// "feed" when parsed as RSS/Atom, "scrape" when the links were harvested
    /// from an ordinary page.
    kind: &'static str,
}

/// Last-resort probe for a site with no feed at all: read the page as HTML and
/// see whether it yields article links. Returns None when it yields too few to
/// be worth polling — one or two hits usually means the heuristics latched onto
/// stray links rather than a real listing.
async fn probe_scrape(http: &reqwest::Client, page_url: &str) -> Option<FeedHit> {
    let out = fetch::fetch_html(http, page_url, "", "").await.ok()?;
    let html = out.html?;
    let items = fetch::scan_page_articles(&html, page_url);
    if items.len() < 3 {
        return None;
    }
    Some(FeedHit {
        url: page_url.to_string(),
        title: fetch::clip(fetch::page_title(&html).trim(), 80),
        item_count: items.len(),
        sample: items.iter().take(3).map(|i| i.title.clone()).collect(),
        kind: "scrape",
    })
}

/// Feed URLs a page points at: `<link rel=alternate>` first, then feed-looking
/// `<a href>`s. The second pass matters for Vietnamese outlets — vnexpress.net
/// carries no autodiscovery tag and its /rss page is a plain HTML directory.
async fn links_from_page(http: &reqwest::Client, page_url: &str) -> Vec<String> {
    let Ok(resp) = http.get(page_url).send().await else {
        return Vec::new();
    };
    if !resp.status().is_success() {
        return Vec::new();
    }
    let Ok(html) = resp.text().await else {
        return Vec::new();
    };
    let mut out = fetch::autodiscover_links(&html, page_url);
    if out.is_empty() {
        out = fetch::scan_feed_hrefs(&html, page_url);
    }
    out
}

/// Probe a URL for WORKING sources: direct parse → links on the page → common
/// paths (site roots only, each of which may itself be a directory page). When
/// the site publishes no feed anywhere, fall back to scraping the page itself,
/// so "trang này không có RSS" stops being a dead end.
/// Returns everything that actually produced articles, capped at `max`.
async fn probe_feeds(
    http: &reqwest::Client,
    input_url: &str,
    max: usize,
) -> Result<Vec<FeedHit>, String> {
    let hit = |url: &str, out: fetch::FetchOutcome| -> Option<FeedHit> {
        out.items.map(|items| FeedHit {
            url: url.to_string(),
            title: out.feed_title,
            item_count: items.len(),
            sample: items.iter().take(3).map(|i| i.title.clone()).collect(),
            kind: "feed",
        })
    };

    if let Ok(out) = fetch::fetch_feed(http, input_url, "", "").await {
        if let Some(h) = hit(input_url, out) {
            return Ok(vec![h]);
        }
    }

    let mut candidates = links_from_page(http, input_url).await;
    if candidates.is_empty() {
        // Homepage said nothing — try /rss, /feed… each of which may be a feed
        // itself or another directory page.
        for path in fetch::common_feed_paths(input_url).into_iter().take(3) {
            if let Ok(out) = fetch::fetch_feed(http, &path, "", "").await {
                if let Some(h) = hit(&path, out) {
                    return Ok(vec![h]);
                }
            }
            candidates.extend(links_from_page(http, &path).await);
            if !candidates.is_empty() {
                break;
            }
        }
    }
    candidates.truncate(max);

    let mut hits = Vec::new();
    for c in candidates {
        if let Ok(out) = fetch::fetch_feed(http, &c, "", "").await {
            if let Some(h) = hit(&c, out) {
                hits.push(h);
            }
        }
    }
    if hits.is_empty() {
        return match probe_scrape(http, input_url).await {
            Some(h) => Ok(vec![h]),
            None => Err(
                "trang này không có feed RSS/Atom, và cũng không quét được link bài viết nào \
                         (có thể cần JavaScript — thử trỏ vào một trang chuyên mục cụ thể)"
                    .into(),
            ),
        };
    }
    Ok(hits)
}

pub(crate) async fn discover_sources_value(s: &AppState, b: &DiscoverIn) -> Value {
    let q = b.query.trim();
    if q.is_empty() {
        return json!({ "error": "thiếu 'query' — nhập chủ đề muốn theo dõi hoặc URL một trang web" });
    }
    let _job = s.track_job("discover", "discover", &format!("Đang tìm & kiểm chứng nguồn: {}", fetch::clip(q, 60)));
    let existing: Vec<String> =
        s.db.list_sources(None)
            .into_iter()
            .filter_map(|src| src["url"].as_str().map(String::from))
            .collect();

    // Candidates: a bare URL goes straight to probing; free text asks the AI.
    let (mut candidates, model, via) = if q.starts_with("http://") || q.starts_with("https://") {
        (
            vec![llm::CandidateSource {
                name: String::new(),
                url: q.to_string(),
                category: String::new(),
                lang: String::new(),
            }],
            String::new(),
            "url",
        )
    } else {
        match llm::suggest_sources(&s.sc, q, &existing).await {
            Ok((list, model)) => (list, model, "ai"),
            Err(e) => {
                return json!({ "error": format!("không gọi được AI qua bridge SenClaw: {e}") })
            }
        }
    };
    candidates.dedup_by(|a, b| a.url == b.url);
    if candidates.is_empty() {
        return json!({ "error": "AI không gợi ý được nguồn nào cho chủ đề này — thử mô tả cụ thể hơn" });
    }

    // A bare URL may be a whole feed directory, so it gets several slots; an
    // AI suggestion names one specific feed and gets one.
    let per_candidate = if via == "url" { 10 } else { 1 };

    // Validate every candidate by ACTUALLY fetching it, 4 at a time.
    let nested: Vec<Vec<Value>> = futures_util::stream::iter(candidates.into_iter().map(|c| {
        let http = s.http.clone();
        let existing = existing.clone();
        async move {
            if existing.iter().any(|u| u == &c.url) {
                return vec![json!({ "status": "exists", "url": c.url, "name": c.name })];
            }
            match probe_feeds(&http, &c.url, per_candidate).await {
                Ok(hits) => hits
                    .into_iter()
                    .map(|h| {
                        if existing.iter().any(|u| u == &h.url) {
                            return json!({ "status": "exists", "url": h.url, "name": c.name });
                        }
                        // The AI's own label wins for a single named feed;
                        // for a directory sweep each feed keeps its own title.
                        let name = if !h.title.trim().is_empty() {
                            h.title.trim().to_string()
                        } else if !c.name.trim().is_empty() {
                            c.name.trim().to_string()
                        } else {
                            h.url.clone()
                        };
                        json!({
                            "status": "ok",
                            "url": h.url,
                            "input_url": c.url,
                            "name": name,
                            "category": c.category,
                            "lang": c.lang,
                            "feed_title": h.title,
                            "item_count": h.item_count,
                            "sample": h.sample,
                            "kind": h.kind,
                        })
                    })
                    .collect(),
                Err(e) => {
                    vec![json!({ "status": "error", "url": c.url, "name": c.name, "error": e })]
                }
            }
        }
    }))
    .buffer_unordered(4)
    .collect()
    .await;
    let results: Vec<Value> = nested.into_iter().flatten().collect();

    let mut added = 0i64;
    let mut results = results;
    if b.auto_add {
        for r in results.iter_mut() {
            if r["status"] == "ok" {
                let (name, url) = (
                    r["name"].as_str().unwrap_or(""),
                    r["url"].as_str().unwrap_or(""),
                );
                let kind = r["kind"].as_str().unwrap_or("feed");
                match s.db.add_source(
                    name,
                    url,
                    r["category"].as_str().unwrap_or(""),
                    r["lang"].as_str().unwrap_or(""),
                    if kind == "scrape" {
                        "tự tìm — quét nội dung trang"
                    } else {
                        "tự tìm qua AI/autodiscovery"
                    },
                    kind,
                ) {
                    Ok(id) => {
                        r["added"] = json!(true);
                        r["source_id"] = json!(id);
                        added += 1;
                    }
                    Err(e) => r["add_error"] = json!(e.to_string()),
                }
            }
        }
        if added > 0 {
            s.db.log(
                "source",
                &format!("tự tìm nguồn \"{q}\": thêm {added} nguồn"),
                "",
            );
        }
    }
    let ok = results.iter().filter(|r| r["status"] == "ok").count();
    json!({
        "ok": true,
        "via": via,
        "model": model,
        "query": q,
        "found": ok,
        "added": added,
        "results": results,
    })
}

async fn discover_sources(
    State(s): State<AppState>,
    body: Option<Json<DiscoverIn>>,
) -> Json<Value> {
    let b = body.map(|Json(x)| x).unwrap_or_default();
    Json(discover_sources_value(&s, &b).await)
}

// ---- topics ----

#[derive(Deserialize, Default)]
pub(crate) struct TopicIn {
    pub name: String,
    #[serde(default)]
    pub keywords: String,
    #[serde(default)]
    pub color: String,
}

pub(crate) fn add_topic_value(s: &AppState, b: &TopicIn) -> Value {
    match s.db.add_topic(&b.name, &b.keywords, &b.color) {
        Ok(id) => {
            // Backfill: match the new topic against the recent archive.
            let n = s.db.reassign_topic(id, now_ts() - 30 * 86400).unwrap_or(0);
            requeue_sections(s);
            s.db.log(
                "topic",
                &format!("thêm chủ đề \"{}\" ({} bài khớp)", b.name.trim(), n),
                &id.to_string(),
            );
            json!({ "ok": true, "topic_id": id, "matched": n })
        }
        Err(e) => json!({ "error": e.to_string() }),
    }
}

async fn add_topic(State(s): State<AppState>, Json(b): Json<TopicIn>) -> Json<Value> {
    Json(add_topic_value(&s, &b))
}

pub(crate) fn list_topics_value(s: &AppState) -> Value {
    json!({ "topics": s.db.list_topics() })
}

async fn list_topics(State(s): State<AppState>) -> Json<Value> {
    Json(list_topics_value(&s))
}

pub(crate) fn update_topic_value(s: &AppState, id: i64, patch: &Value) -> Value {
    match s.db.update_topic(id, patch) {
        Ok(()) => {
            // Keywords changed → recompute membership over the recent archive.
            let matched = if patch.get("keywords").is_some() {
                s.db.reassign_topic(id, now_ts() - 30 * 86400).unwrap_or(0)
            } else {
                -1
            };
            if patch.get("keywords").is_some() || patch.get("name").is_some() {
                requeue_sections(s);
            }
            s.db.log("topic", &format!("cập nhật chủ đề #{id}"), &id.to_string());
            let mut v = json!({ "ok": true });
            if matched >= 0 {
                v["matched"] = json!(matched);
            }
            v
        }
        Err(e) => json!({ "error": e.to_string() }),
    }
}

async fn update_topic(
    State(s): State<AppState>,
    Path(id): Path<i64>,
    Json(patch): Json<Value>,
) -> Json<Value> {
    Json(update_topic_value(&s, id, &patch))
}

pub(crate) fn delete_topic_value(s: &AppState, id: i64) -> Value {
    match s.db.delete_topic(id) {
        Ok(()) => {
            requeue_sections(s);
            s.db.log("topic", &format!("xoá chủ đề #{id}"), &id.to_string());
            json!({ "ok": true })
        }
        Err(e) => json!({ "error": e.to_string() }),
    }
}

async fn delete_topic(State(s): State<AppState>, Path(id): Path<i64>) -> Json<Value> {
    Json(delete_topic_value(&s, id))
}

/// Chủ đề là phương án của câu hỏi System One, nên mọi lần thêm/sửa/xoá chủ đề
/// làm kết luận cũ trong cửa sổ phân loại hết đúng — đưa chúng về hàng chờ.
fn requeue_sections(s: &AppState) {
    s.db.requeue_sections(now_ts() - laya::WINDOW_SECS);
}

// ---- sections (chuyên mục do System One chọn) ----

#[derive(Deserialize, Default)]
pub(crate) struct SectionQuery {
    pub hours: Option<i64>,
}

pub(crate) fn sections_value(s: &AppState, hours: Option<i64>) -> Value {
    let hours = hours.unwrap_or(168).clamp(1, 24 * 90);
    json!({
        "hours": hours,
        "sections": s.db.section_counts(now_ts() - hours * 3600),
        "laya": laya::status_json(&s.db),
    })
}

async fn sections(State(s): State<AppState>, Query(q): Query<SectionQuery>) -> Json<Value> {
    Json(sections_value(&s, q.hours))
}

// ---- trends ----

/// Trending phrases in the last `hours`, each with sample articles.
pub(crate) fn trends_value(s: &AppState, hours: i64) -> Value {
    let hours = hours.clamp(6, 24 * 14);
    let now = now_ts();
    let current = s.db.titles_between(now - hours * 3600, now + 3600);
    let previous =
        s.db.titles_between(now - 2 * hours * 3600, now - hours * 3600);
    // Background language is measured from the archive, not guessed: the db
    // answers one batched question about the top candidate phrases.
    let trends = cluster::detect_trends(&current, &previous, 2, 15, &|p| s.db.corpus_for(p));
    let mut out = cluster::trends_to_json(&trends);
    if let Some(arr) = out.as_array_mut() {
        for (t, trend) in arr.iter_mut().zip(trends.iter()) {
            t["samples"] = json!(s.db.brief_articles(&trend.article_ids, 3));
        }
    }
    json!({ "hours": hours, "article_count": current.len(), "trends": out })
}

#[derive(Deserialize)]
struct TrendsQuery {
    hours: Option<i64>,
}

async fn trends(State(s): State<AppState>, Query(q): Query<TrendsQuery>) -> Json<Value> {
    Json(trends_value(&s, q.hours.unwrap_or(48)))
}

/// AI nhận định xu hướng (trên số liệu trends_value đã tính).
pub(crate) async fn analyze_trends_value(s: &AppState, hours: i64) -> Value {
    let data = trends_value(s, hours);
    if data["trends"]
        .as_array()
        .map(|a| a.is_empty())
        .unwrap_or(true)
    {
        return json!({ "error": "chưa có xu hướng nào trong khoảng thời gian này — thu thập thêm tin trước" });
    }
    let _job = s.track_job("trends", "trends", &format!("Đang phân tích xu hướng {hours}h"));
    let samples: Value = data["trends"]
        .as_array()
        .map(|arr| {
            json!(arr
                .iter()
                .map(|t| json!({"phrase": t["phrase"], "samples": t["samples"]}))
                .collect::<Vec<_>>())
        })
        .unwrap_or(json!([]));
    match llm::analyze_trends(&s.sc, &data["trends"], &samples).await {
        Ok((text, model, truncated)) => {
            s.db.log("ai", "nhận định xu hướng", "");
            json!({
                "ok": true, "analysis": text, "model": model,
                "truncated": truncated, "trends": data["trends"],
            })
        }
        Err(e) => json!({ "error": format!("không gọi được AI qua bridge SenClaw: {e}") }),
    }
}

#[derive(Deserialize, Default)]
struct TrendsAnalyzeIn {
    hours: Option<i64>,
}

/// How many of a trend's articles the summary is allowed to read. They arrive
/// newest first, so this is a recency window, not a truncation of the event.
const TREND_SUMMARY_ARTICLES: usize = 30;

/// AI tóm tắt MỘT xu hướng: cụm từ + các bài thuộc cụm đó.
///
/// Takes the phrase rather than a list of article ids so that a caller who only
/// saw the card — or an MCP client that only knows the phrase — can ask for the
/// summary. The window is recomputed, so the articles summarised are the ones
/// the card is showing right now.
pub(crate) async fn trend_summary_value(s: &AppState, phrase: &str, hours: i64) -> Value {
    let phrase = phrase.trim();
    if phrase.is_empty() {
        return json!({ "error": "thiếu cụm từ xu hướng" });
    }
    let data = trends_value(s, hours);
    let Some(trend) = data["trends"]
        .as_array()
        .and_then(|a| a.iter().find(|t| t["phrase"].as_str() == Some(phrase)))
        .cloned()
    else {
        return json!({
            "error": format!("xu hướng \"{phrase}\" không còn trong cửa sổ {hours}h — tải lại tab Xu hướng"),
        });
    };
    let ids: Vec<i64> = trend["article_ids"]
        .as_array()
        .map(|a| a.iter().filter_map(|v| v.as_i64()).collect())
        .unwrap_or_default();
    let articles = s.db.brief_articles(&ids, TREND_SUMMARY_ARTICLES);
    if articles.is_empty() {
        return json!({ "error": format!("xu hướng \"{phrase}\" không còn bài nào") });
    }
    let _job = s.track_job(
        &format!("trend:{phrase}"),
        "trends",
        &format!("Đang tóm tắt xu hướng: {}", fetch::clip(phrase, 40)),
    );
    match llm::trend_summary(&s.sc, &trend, &articles).await {
        Ok((text, model, truncated)) => {
            s.db.log("ai", &format!("tóm tắt xu hướng \"{phrase}\""), "");
            json!({
                "ok": true, "phrase": phrase, "hours": hours,
                "summary": text, "model": model, "truncated": truncated,
                "count": trend["count"], "prev_count": trend["prev_count"],
                "articles_read": articles.len(),
            })
        }
        Err(e) => json!({ "error": format!("không gọi được AI qua bridge SenClaw: {e}") }),
    }
}

#[derive(Deserialize, Default)]
struct TrendSummaryIn {
    phrase: Option<String>,
    hours: Option<i64>,
}

async fn trend_summary(
    State(s): State<AppState>,
    body: Option<Json<TrendSummaryIn>>,
) -> Json<Value> {
    let b = body.map(|Json(x)| x).unwrap_or_default();
    Json(trend_summary_value(&s, b.phrase.as_deref().unwrap_or(""), b.hours.unwrap_or(48)).await)
}

async fn analyze_trends(
    State(s): State<AppState>,
    body: Option<Json<TrendsAnalyzeIn>>,
) -> Json<Value> {
    let b = body.map(|Json(x)| x).unwrap_or_default();
    Json(analyze_trends_value(&s, b.hours.unwrap_or(48)).await)
}

// ---- stories ----

#[derive(Deserialize)]
struct StoriesQuery {
    days: Option<i64>,
    min_articles: Option<i64>,
    limit: Option<i64>,
}

pub(crate) fn list_stories_value(s: &AppState, days: i64, min_articles: i64, limit: i64) -> Value {
    let since = now_ts() - days.clamp(1, 90) * 86400;
    json!({ "stories": s.db.list_stories(since, min_articles.max(1), limit.clamp(1, 100)) })
}

async fn list_stories(State(s): State<AppState>, Query(q): Query<StoriesQuery>) -> Json<Value> {
    Json(list_stories_value(
        &s,
        q.days.unwrap_or(7),
        q.min_articles.unwrap_or(2),
        q.limit.unwrap_or(30),
    ))
}

pub(crate) fn get_story_value(s: &AppState, id: i64) -> Value {
    match s.db.get_story(id) {
        Some(st) => json!({ "story": st }),
        None => json!({ "error": format!("dòng sự kiện #{id} không tồn tại") }),
    }
}

async fn get_story(State(s): State<AppState>, Path(id): Path<i64>) -> Json<Value> {
    Json(get_story_value(&s, id))
}

/// Graph of related stories: nodes = dòng sự kiện, edges = thematic overlap
/// between their token profiles ("cùng mạch chuyện" — dưới ngưỡng gom cụm).
/// Default links kept per story. Enough to show the shape, few enough that the
/// map stays a map — see `cluster::prune_links`.
const DEFAULT_LINKS_PER_STORY: usize = 3;

pub(crate) fn story_graph_value(
    s: &AppState,
    days: i64,
    min_articles: i64,
    limit: i64,
    per_node: usize,
) -> Value {
    let since = now_ts() - days.clamp(1, 90) * 86400;
    let rows =
        s.db.stories_with_titles(since, min_articles.max(1), limit.clamp(2, 150));
    let stories: Vec<cluster::StoryPhrases> = rows
        .iter()
        .map(|(meta, titles)| cluster::StoryPhrases {
            story_id: meta["id"].as_i64().unwrap_or(0),
            phrases: cluster::phrases_of(titles),
        })
        .collect();
    // Archive-wide frequencies for every phrase on this map, so an edge can
    // never rest on language that means nothing here.
    let all: std::collections::BTreeSet<String> =
        stories.iter().flat_map(|st| st.phrases.iter().cloned()).collect();
    let corpus = s.db.corpus_for(&all);
    let links = cluster::story_links(&stories, &corpus);
    let total = links.len();
    let (links, hidden) = cluster::prune_links(links, per_node);
    json!({
        "days": days,
        "links_per_story": per_node,
        "edges_total": total,
        "edges_hidden": hidden,
        "nodes": rows.iter().map(|(meta, _)| meta.clone()).collect::<Vec<_>>(),
        "edges": links
            .iter()
            .map(|l| json!({ "a": l.a, "b": l.b, "weight": l.weight, "shared": l.shared }))
            .collect::<Vec<_>>(),
    })
}

#[derive(Deserialize)]
struct GraphQuery {
    days: Option<i64>,
    min_articles: Option<i64>,
    limit: Option<i64>,
    /// 0 = show every link (the old, unreadable behaviour — kept for the AI
    /// pass, which reasons over the full set rather than looking at it).
    per_node: Option<usize>,
}

async fn story_graph(State(s): State<AppState>, Query(q): Query<GraphQuery>) -> Json<Value> {
    Json(story_graph_value(
        &s,
        q.days.unwrap_or(7),
        q.min_articles.unwrap_or(2),
        q.limit.unwrap_or(60),
        q.per_node.unwrap_or(DEFAULT_LINKS_PER_STORY).min(20),
    ))
}

#[derive(Deserialize, Default)]
pub(crate) struct GraphAnalyzeIn {
    pub days: Option<i64>,
    pub min_articles: Option<i64>,
    pub limit: Option<i64>,
    #[serde(default)]
    pub question: String,
}

/// AI re-maps the graph: gom mạch chuyện, nối thêm quan hệ máy bỏ sót, chỉ ra
/// liên kết nhiễu. Edge ids are validated against the real graph before return.
pub(crate) async fn analyze_graph_value(s: &AppState, b: &GraphAnalyzeIn) -> Value {
    // The AI reads the FULL link set (per_node = 0): pruning exists so the map
    // stays legible to a human eye, and a weak link it never sees is a link it
    // can never tell us is noise.
    let graph = story_graph_value(
        s,
        b.days.unwrap_or(7),
        b.min_articles.unwrap_or(2),
        b.limit.unwrap_or(40),
        0,
    );
    let empty = Vec::new();
    let nodes = graph["nodes"].as_array().unwrap_or(&empty);
    if nodes.len() < 2 {
        return json!({ "error": "cần ít nhất 2 dòng sự kiện để phân tích liên kết — thu thập thêm tin trước" });
    }
    let valid: std::collections::HashSet<i64> =
        nodes.iter().filter_map(|n| n["id"].as_i64()).collect();
    let _job = s.track_job(
        "graph",
        "graph",
        &format!("Đang phân tích liên kết {} sự kiện", nodes.len()),
    );

    match llm::map_graph(&s.sc, &graph["nodes"], &graph["edges"], &b.question, &valid).await {
        Ok((m, model)) => {
            s.db.log("ai", "phân tích bản đồ liên kết sự kiện", "");
            json!({
                "ok": true,
                "model": model,
                "summary": m.summary,
                "clusters": m.clusters.iter().map(|c| json!({
                    "name": c.name, "story_ids": c.story_ids, "why": c.why
                })).collect::<Vec<_>>(),
                "ai_links": m.links.iter().map(|l| json!({
                    "a": l.a, "b": l.b, "relation": l.relation, "why": l.why
                })).collect::<Vec<_>>(),
                "noise": m.noise.iter().map(|l| json!({
                    "a": l.a, "b": l.b, "relation": l.relation, "why": l.why
                })).collect::<Vec<_>>(),
                "graph": graph,
            })
        }
        Err(e) => json!({ "error": format!("không gọi được AI qua bridge SenClaw: {e}") }),
    }
}

async fn analyze_graph(
    State(s): State<AppState>,
    body: Option<Json<GraphAnalyzeIn>>,
) -> Json<Value> {
    let b = body.map(|Json(x)| x).unwrap_or_default();
    Json(analyze_graph_value(&s, &b).await)
}

#[derive(Deserialize, Default)]
pub(crate) struct BriefIn {
    #[serde(default)]
    pub force: bool,
}

/// AI tóm tắt dòng sự kiện. Cached in `stories.summary`; invalidated whenever
/// a new article joins (place_in_story resets summary_at).
pub(crate) async fn story_brief_value(s: &AppState, id: i64, force: bool) -> Value {
    let Some(story) = s.db.get_story(id) else {
        return json!({ "error": format!("dòng sự kiện #{id} không tồn tại") });
    };
    let fresh = story["summary_at"]
        .as_str()
        .map(|x| !x.is_empty())
        .unwrap_or(false)
        && !story["summary"].as_str().unwrap_or("").is_empty();
    if fresh && !force {
        return json!({ "ok": true, "story_id": id, "summary": story["summary"], "model": story["summary_model"], "cached": true });
    }
    let _job = s.track_job(
        &format!("story:{id}"),
        "story_brief",
        &format!(
            "Đang tóm tắt diễn biến: {}",
            fetch::clip(story["title"].as_str().unwrap_or(""), 50)
        ),
    );
    match llm::story_brief(&s.sc, &story).await {
        Ok((text, model, truncated)) => {
            let _ = s.db.set_story_summary(id, &text, &model);
            s.db.log(
                "ai",
                &format!("tóm tắt dòng sự kiện #{id}"),
                &id.to_string(),
            );
            json!({
                "ok": true, "story_id": id, "summary": text, "model": model,
                "truncated": truncated, "cached": false,
            })
        }
        Err(e) => json!({ "error": format!("không gọi được AI qua bridge SenClaw: {e}") }),
    }
}

/// How many articles one translate request may send to the bridge. The reply is
/// one JSON object holding every item, so this is really an output-size limit.
const TRANSLATE_BATCH: usize = 20;

/// Translate a story's timeline into the configured display language.
///
/// On demand rather than at ingest: most readers never need it, translations
/// cost an LLM call each, and the cache makes a second visit free.
pub(crate) async fn translate_story_value(s: &AppState, id: i64) -> Value {
    let lang = s.db.display_language();
    if lang.trim().is_empty() {
        return json!({ "error": "chưa đặt ngôn ngữ hiển thị trong Cài đặt" });
    }
    let Some(story) = s.db.get_story(id) else {
        return json!({ "error": format!("dòng sự kiện #{id} không tồn tại") });
    };
    let empty = Vec::new();
    let timeline = story["timeline"].as_array().unwrap_or(&empty);
    let ids: Vec<i64> = timeline.iter().filter_map(|a| a["id"].as_i64()).collect();
    let have = s.db.translations_for(&ids, &lang);

    let todo: Vec<(i64, String, String)> = timeline
        .iter()
        .filter_map(|a| {
            let aid = a["id"].as_i64()?;
            if have.contains_key(&aid) {
                return None;
            }
            Some((
                aid,
                a["title"].as_str().unwrap_or("").to_string(),
                a["description"].as_str().unwrap_or("").to_string(),
            ))
        })
        .collect();
    if todo.is_empty() {
        return json!({ "ok": true, "lang": lang, "translated": 0, "cached": ids.len() });
    }

    let _job = s.track_job(
        &format!("translate:{id}"),
        "translate_story",
        &format!("Đang dịch diễn biến sang {lang}"),
    );
    let mut done = 0usize;
    let mut failed: Option<String> = None;
    for chunk in todo.chunks(TRANSLATE_BATCH) {
        match llm::translate(&s.sc, chunk, &lang).await {
            Ok(items) => {
                for t in items {
                    let _ = s.db.save_translation(t.id, &lang, &t.title, &t.description);
                    done += 1;
                }
            }
            Err(e) => {
                failed = Some(e.to_string());
                break; // whatever already landed is kept and served from cache
            }
        }
    }
    s.db.log("ai", &format!("dịch dòng sự kiện #{id} sang {lang}"), &id.to_string());
    match failed {
        // Partial success is still success: the cached rows are usable now and
        // the next call picks up where this one stopped.
        Some(e) if done == 0 => json!({ "error": format!("không dịch được: {e}") }),
        Some(e) => json!({ "ok": true, "lang": lang, "translated": done, "warning": e }),
        None => json!({ "ok": true, "lang": lang, "translated": done }),
    }
}

async fn translate_story(State(s): State<AppState>, Path(id): Path<i64>) -> Json<Value> {
    Json(translate_story_value(&s, id).await)
}

/// Re-cluster every article with the current rules. Long-running (seconds on a
/// full database) but synchronous — it is a one-shot repair, not a hot path.
pub(crate) fn rebuild_stories_value(s: &AppState) -> Value {
    match s.db.rebuild_stories() {
        Ok(v) => {
            s.db.log(
                "system",
                &format!(
                    "gom lại dòng sự kiện: {} dòng từ {} bài",
                    v["stories"], v["articles"]
                ),
                "",
            );
            v
        }
        Err(e) => json!({ "error": format!("không gom lại được dòng sự kiện: {e}") }),
    }
}

async fn rebuild_stories(State(s): State<AppState>) -> Json<Value> {
    Json(rebuild_stories_value(&s))
}

async fn story_brief(
    State(s): State<AppState>,
    Path(id): Path<i64>,
    body: Option<Json<BriefIn>>,
) -> Json<Value> {
    let b = body.map(|Json(x)| x).unwrap_or_default();
    Json(story_brief_value(&s, id, b.force).await)
}

// ---- digest (điểm tin) ----

#[derive(Deserialize, Default)]
pub(crate) struct DigestIn {
    /// Look-back window, hours (default 24).
    pub hours: Option<i64>,
    /// Optional reader focus, free text ("công nghệ", "kinh tế vĩ mô"…).
    #[serde(default)]
    pub focus: String,
    /// Optional topic filter.
    pub topic_id: Option<i64>,
}

pub(crate) async fn digest_value(s: &AppState, b: &DigestIn) -> Value {
    let hours = b.hours.unwrap_or(24).clamp(1, 24 * 7);
    let since = now_ts() - hours * 3600;
    let articles =
        s.db.list_articles(None, None, b.topic_id, None, None, None, Some(since), 60, 0);
    if articles.is_empty() {
        return json!({ "error": "chưa có bài nào trong khoảng thời gian này — bấm thu thập trước" });
    }
    let topic_name = b
        .topic_id
        .and_then(|id| {
            s.db.list_topics()
                .into_iter()
                .find(|t| t["id"] == id)
                .and_then(|t| t["name"].as_str().map(String::from))
        })
        .unwrap_or_default();
    let stories = s.db.list_stories(since, 2, 8);
    let _job = s.track_job(
        "digest",
        "digest",
        &format!(
            "Đang viết điểm tin {hours}h{}",
            if topic_name.is_empty() { String::new() } else { format!(" · {topic_name}") }
        ),
    );
    match llm::digest(&s.sc, &articles, &stories, &b.focus).await {
        Ok((text, model, truncated)) => {
            s.db.log("ai", &format!("điểm tin {hours}h"), "");
            let id = s
                .db
                .save_digest(
                    hours,
                    &b.focus,
                    b.topic_id,
                    &topic_name,
                    articles.len() as i64,
                    &text,
                    &model,
                    truncated,
                )
                .unwrap_or(0);
            json!({
                "ok": true, "digest": text, "model": model, "truncated": truncated,
                "hours": hours, "article_count": articles.len(), "digest_id": id,
            })
        }
        Err(e) => json!({ "error": format!("không gọi được AI qua bridge SenClaw: {e}") }),
    }
}

async fn digest(State(s): State<AppState>, body: Option<Json<DigestIn>>) -> Json<Value> {
    let b = body.map(|Json(x)| x).unwrap_or_default();
    Json(digest_value(&s, &b).await)
}

/// Lịch sử điểm tin + job đang chạy (nếu có), để UI mở lên là biết ngay.
pub(crate) fn digest_history_value(s: &AppState, limit: i64) -> Value {
    json!({
        "digests": s.db.list_digests(limit),
        "running": s.job_running("digest"),
    })
}

#[derive(Deserialize)]
struct HistoryQuery {
    limit: Option<i64>,
}

async fn digest_history(State(s): State<AppState>, Query(q): Query<HistoryQuery>) -> Json<Value> {
    Json(digest_history_value(&s, q.limit.unwrap_or(30)))
}

pub(crate) fn get_digest_value(s: &AppState, id: i64) -> Value {
    match s.db.get_digest(id) {
        Some(d) => json!({ "digest": d }),
        None => json!({ "error": format!("bản điểm tin #{id} không tồn tại") }),
    }
}

async fn get_digest(State(s): State<AppState>, Path(id): Path<i64>) -> Json<Value> {
    Json(get_digest_value(&s, id))
}

async fn delete_digest(State(s): State<AppState>, Path(id): Path<i64>) -> Json<Value> {
    Json(match s.db.delete_digest(id) {
        Ok(()) => json!({ "ok": true }),
        Err(e) => json!({ "error": e.to_string() }),
    })
}

/// Mọi việc dài hơi đang chạy — UI hiện "đang xử lý" từ đây.
async fn jobs(State(s): State<AppState>) -> Json<Value> {
    Json(json!({ "jobs": s.jobs_snapshot() }))
}

// ---- settings / activity ----

pub(crate) fn settings_value(s: &AppState) -> Value {
    json!({
        "fetch_interval_min": s.db.setting("fetch_interval_min", "30").parse::<i64>().unwrap_or(30),
        "retention_days": s.db.setting("retention_days", "30").parse::<i64>().unwrap_or(30),
        "auto_fetch": s.db.setting("auto_fetch", "1") == "1",
        "display_language": s.db.display_language(),
        "auto_regroup_hours": s.db.setting("auto_regroup_hours", "12").parse::<i64>().unwrap_or(12),
        "digest_markers": s.db.digest_markers_setting(),
        "laya_classify": laya::enabled(&s.db),
    })
}

async fn get_settings(State(s): State<AppState>) -> Json<Value> {
    Json(settings_value(&s))
}

async fn set_settings(State(s): State<AppState>, Json(b): Json<Value>) -> Json<Value> {
    if let Some(v) = b.get("fetch_interval_min").and_then(|x| x.as_i64()) {
        let _ =
            s.db.set_setting("fetch_interval_min", &v.clamp(5, 24 * 60).to_string());
    }
    if let Some(v) = b.get("retention_days").and_then(|x| x.as_i64()) {
        let _ =
            s.db.set_setting("retention_days", &v.clamp(3, 365).to_string());
    }
    if let Some(v) = b.get("auto_fetch").and_then(|x| x.as_bool()) {
        let _ = s.db.set_setting("auto_fetch", if v { "1" } else { "0" });
    }
    if let Some(v) = b.get("display_language").and_then(|x| x.as_str()) {
        let _ = s.db.set_setting("display_language", v.trim());
        llm::set_output_language(v.trim());
    }
    if let Some(v) = b.get("auto_regroup_hours").and_then(|x| x.as_i64()) {
        // 0 disables; otherwise at least an hour so it can't spin.
        let v = if v <= 0 { 0 } else { v.clamp(1, 24 * 30) };
        let _ = s.db.set_setting("auto_regroup_hours", &v.to_string());
    }
    if let Some(v) = b.get("laya_classify").and_then(|x| x.as_bool()) {
        let _ = s.db.set_setting("laya_classify", if v { "1" } else { "0" });
    }
    if let Some(v) = b.get("digest_markers").and_then(|x| x.as_str()) {
        let _ = s.db.set_setting("digest_markers", v);
        s.db.apply_digest_markers();
    }
    Json(settings_value(&s))
}

async fn activity(State(s): State<AppState>) -> Json<Value> {
    Json(json!({ "activity": s.db.recent_activity(50) }))
}

// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> AppState {
        let db = Arc::new(Db::open_memory().unwrap());
        let (mcp_tx, _) = tokio::sync::broadcast::channel(4);
        AppState {
            db,
            sc: SpaceClient::new("http://127.0.0.1:1", "news"),
            http: fetch::http_client(),
            mcp_tx,
            jobs: Arc::new(std::sync::Mutex::new(Default::default())),
        }
    }

    fn item(title: &str, url: &str) -> fetch::FeedItem {
        fetch::FeedItem {
            title: title.into(),
            url: url.into(),
            published_at: now_ts() - 60,
            ..Default::default()
        }
    }

    #[test]
    fn ingest_pipeline_dedups_topics_and_stories() {
        let s = state();
        let src =
            s.db.add_source("T", "https://t.vn/rss", "", "vi", "", "feed")
                .unwrap();
        let topic = s.db.add_topic("Thiên tai", "bão, lũ lụt", "red").unwrap();

        assert!(ingest_item(
            &s.db,
            src,
            &item("Bão số 3 đổ bộ Quảng Ninh sáng nay", "https://t.vn/1")
        ));
        assert!(ingest_item(
            &s.db,
            src,
            &item(
                "Quảng Ninh thiệt hại nặng sau khi bão số 3 đổ bộ",
                "https://t.vn/2"
            )
        ));
        assert!(
            !ingest_item(
                &s.db,
                src,
                &item("Bão số 3 đổ bộ Quảng Ninh sáng nay", "https://t.vn/1")
            ),
            "dup url skipped"
        );

        // topic matched by keyword "bão"
        let by_topic =
            s.db.list_articles(None, None, Some(topic), None, None, None, None, 10, 0);
        assert_eq!(by_topic.len(), 2);
        // both articles share one story
        let stories = s.db.list_stories(0, 2, 10);
        assert_eq!(stories.len(), 1);
        assert_eq!(stories[0]["article_count"], 2);
    }

    #[test]
    fn dashboard_and_status_shapes() {
        let s = state();
        let src =
            s.db.add_source("T", "https://t.vn/rss", "", "vi", "", "feed")
                .unwrap();
        ingest_item(
            &s.db,
            src,
            &item("Giá vàng lập đỉnh phiên sáng", "https://t.vn/3"),
        );
        let st = status_value(&s);
        assert_eq!(st["ok"], true);
        assert_eq!(st["articles_total"], 1);
        let d = dashboard_value(&s);
        assert!(d["per_day"].as_array().unwrap().len() == 14);
        assert_eq!(d["recent_articles"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn trends_value_counts_and_samples() {
        let s = state();
        let src =
            s.db.add_source("T", "https://t.vn/rss", "", "vi", "", "feed")
                .unwrap();
        for i in 0..3 {
            ingest_item(
                &s.db,
                src,
                &item(
                    &format!("Giá vàng tăng mạnh phiên {i}"),
                    &format!("https://t.vn/v{i}"),
                ),
            );
        }
        let t = trends_value(&s, 48);
        let trends = t["trends"].as_array().unwrap();
        assert!(!trends.is_empty());
        assert!(trends[0]["samples"].as_array().unwrap().len() >= 2);
    }

    #[test]
    fn topic_add_backfills_recent_articles() {
        let s = state();
        let src =
            s.db.add_source("T", "https://t.vn/rss", "", "vi", "", "feed")
                .unwrap();
        ingest_item(
            &s.db,
            src,
            &item("Chứng khoán giảm sâu phiên chiều", "https://t.vn/ck"),
        );
        let r = add_topic_value(
            &s,
            &TopicIn {
                name: "Chứng khoán".into(),
                keywords: "chứng khoán".into(),
                color: String::new(),
            },
        );
        assert_eq!(r["ok"], true);
        assert_eq!(r["matched"], 1);
    }

    #[test]
    fn story_graph_links_related_stories() {
        let s = state();
        let src =
            s.db.add_source("T", "https://t.vn/rss", "", "vi", "", "feed")
                .unwrap();
        let mk = |title: &str, url: &str| {
            ingest_item(&s.db, src, &item(title, url));
        };
        // story A: giá vàng lập đỉnh — story B: Trung Quốc gom dự trữ (sự kiện
        // riêng, nhưng cùng mạch "giá vàng lập đỉnh" → phải có cạnh nối)
        mk("Giá vàng lập đỉnh lịch sử mới", "https://t.vn/g1");
        mk(
            "Giá vàng trong nước lập đỉnh phiên chiều",
            "https://t.vn/g2",
        );
        mk(
            "Trung Quốc gom hàng dự trữ chiến lược khi giá vàng lập đỉnh",
            "https://t.vn/c1",
        );
        mk(
            "Ngân hàng trung ương Trung Quốc gom hàng dự trữ lúc giá vàng lập đỉnh",
            "https://t.vn/c2",
        );
        // story C: bóng đá, không liên quan
        mk(
            "Tuyển Việt Nam thắng trận chung kết bóng đá",
            "https://t.vn/b1",
        );
        mk(
            "Chung kết bóng đá: tuyển Việt Nam thắng đậm",
            "https://t.vn/b2",
        );

        let g = story_graph_value(&s, 7, 2, 50, 3);
        let nodes = g["nodes"].as_array().unwrap();
        assert_eq!(nodes.len(), 3, "3 stories expected, got {nodes:?}");
        let edges = g["edges"].as_array().unwrap();
        assert_eq!(
            edges.len(),
            1,
            "only the two gold stories link, got {edges:?}"
        );
        // Liên kết dựa trên CỤM từ, không phải âm tiết đơn.
        assert!(
            edges[0]["shared"]
                .as_array()
                .unwrap()
                .iter()
                .any(|t| t == "giá vàng"),
            "shared phrases: {:?}",
            edges[0]["shared"]
        );
    }

    #[tokio::test]
    async fn discover_requires_query_and_probe_fails_cleanly() {
        let s = state();
        let r = discover_sources_value(&s, &DiscoverIn::default()).await;
        assert!(r["error"].as_str().unwrap().contains("query"));
        // URL mode with an unreachable address → validated as error, not a panic.
        let r = discover_sources_value(
            &s,
            &DiscoverIn {
                query: "http://127.0.0.1:1/rss".into(),
                auto_add: true,
            },
        )
        .await;
        assert_eq!(r["found"], 0);
        assert_eq!(r["added"], 0);
        assert_eq!(r["results"][0]["status"], "error");
    }

    #[test]
    fn job_guard_registers_and_clears() {
        let s = state();
        assert!(s.jobs_snapshot().is_empty());
        {
            let _g = s.track_job("digest", "digest", "Đang viết điểm tin 24h");
            let jobs = s.jobs_snapshot();
            assert_eq!(jobs.len(), 1);
            assert_eq!(jobs[0]["label"], "Đang viết điểm tin 24h");
            assert!(s.job_running("digest").is_some());
            // Cùng key bấm hai lần vẫn chỉ là MỘT việc đang chạy.
            let _g2 = s.track_job("digest", "digest", "Đang viết điểm tin 24h");
            assert_eq!(s.jobs_snapshot().len(), 1);
        }
        assert!(s.jobs_snapshot().is_empty(), "guard phải tự dọn khi kết thúc");
        assert!(s.job_running("digest").is_none());
    }

    #[test]
    fn job_guard_clears_on_early_return() {
        let s = state();
        fn bail_out(s: &AppState) -> &'static str {
            let _job = s.track_job("digest", "digest", "…");
            "lỗi giữa chừng"
        }
        assert_eq!(bail_out(&s), "lỗi giữa chừng");
        assert!(s.jobs_snapshot().is_empty(), "thoát sớm không được để lại job ma");
    }

    #[test]
    fn digest_history_roundtrip_and_delete() {
        let s = state();
        let id = s
            .db
            .save_digest(24, "công nghệ", None, "", 42, "## Tin chính\n- một tin", "m1", false)
            .unwrap();

        let h = digest_history_value(&s, 30);
        let list = h["digests"].as_array().unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0]["article_count"], 42);
        assert_eq!(list[0]["focus"], "công nghệ");
        assert!(list[0]["preview"].as_str().unwrap().contains("Tin chính"));
        assert!(list[0].get("text").is_none(), "danh sách chỉ mang preview");
        assert!(h["running"].is_null(), "không có job nào đang chạy");

        let full = get_digest_value(&s, id);
        assert!(full["digest"]["text"].as_str().unwrap().contains("một tin"));

        s.db.delete_digest(id).unwrap();
        assert!(digest_history_value(&s, 30)["digests"].as_array().unwrap().is_empty());
        assert!(get_digest_value(&s, id)["error"].as_str().unwrap().contains("không tồn tại"));
    }

    #[test]
    fn digest_history_keeps_only_the_newest_50() {
        let s = state();
        for i in 0..55 {
            s.db.save_digest(24, "", None, "", i, &format!("bản {i}"), "m", false).unwrap();
        }
        let list = s.db.list_digests(50);
        assert_eq!(list.len(), 50);
        assert_eq!(list[0]["article_count"], 54, "mới nhất đứng đầu");
    }

    #[tokio::test]
    async fn digest_without_articles_is_a_clear_error() {
        let s = state();
        let r = digest_value(&s, &DigestIn::default()).await;
        assert!(r["error"].as_str().unwrap().contains("chưa có bài"));
    }

    #[tokio::test]
    async fn analyze_missing_article_errors() {
        let s = state();
        let r = analyze_article_value(&s, 999, &AnalyzeIn::default()).await;
        assert!(r["error"].as_str().unwrap().contains("không tồn tại"));
    }
}
