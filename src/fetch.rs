//! Feed fetching + parsing for the News app.
//!
//! * RSS 2.0 and Atom, parsed with quick-xml (tolerant: unknown tags are
//!   skipped, CDATA and entities handled, dates in RFC 2822 / RFC 3339 /
//!   a few sloppy variants).
//! * Conditional GET — we replay `ETag` / `Last-Modified` and treat 304 as
//!   "no change", so polling every N minutes stays cheap for the publisher.
//! * `extract_page_text` — a deliberately simple readability pass used for
//!   "xem toàn văn": drop script/style/nav blocks, prefer <p> text, cap size.
//!   It is NOT a browser; JS-only pages return little text and that's fine.
//! * `scan_page_articles` + `parse_page_meta` — the RSS-less path. For sites
//!   that publish no feed, article links are harvested from a listing page's
//!   HTML (anchor text that reads like a headline, href that looks like an
//!   article) and each new one is opened for its Open Graph `<head>`. Same
//!   caveat: server-rendered HTML only, no JavaScript.

use anyhow::{anyhow, Result};
use quick_xml::events::Event;
use quick_xml::Reader;
use std::time::Duration;

/// One parsed feed entry, normalized across RSS/Atom.
#[derive(Debug, Clone, Default)]
pub struct FeedItem {
    pub title: String,
    pub url: String,
    pub guid: String,
    pub description: String,
    pub author: String,
    pub category: String,
    pub image_url: String,
    /// Unix seconds; 0 = the feed didn't say (caller substitutes fetch time).
    pub published_at: i64,
}

/// Result of fetching one feed URL.
pub struct FetchOutcome {
    /// None = HTTP 304, nothing new.
    pub items: Option<Vec<FeedItem>>,
    pub etag: String,
    pub last_modified: String,
    /// Feed-level title, when the source has no name yet.
    pub feed_title: String,
}

pub fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .user_agent(
            "Mozilla/5.0 (compatible; SenClawNews/1.0; +https://github.com/midea-ai/SenClaw)",
        )
        .timeout(Duration::from_secs(20))
        .build()
        .expect("build http client")
}

/// Fetch + parse one feed with conditional-GET support.
pub async fn fetch_feed(
    client: &reqwest::Client,
    url: &str,
    etag: &str,
    last_modified: &str,
) -> Result<FetchOutcome> {
    let mut req = client.get(url);
    if !etag.is_empty() {
        req = req.header("If-None-Match", etag);
    }
    if !last_modified.is_empty() {
        req = req.header("If-Modified-Since", last_modified);
    }
    let resp = req
        .send()
        .await
        .map_err(|e| anyhow!("không tải được feed: {e}"))?;
    let status = resp.status();
    if status.as_u16() == 304 {
        return Ok(FetchOutcome {
            items: None,
            etag: etag.to_string(),
            last_modified: last_modified.to_string(),
            feed_title: String::new(),
        });
    }
    if !status.is_success() {
        return Err(anyhow!("feed trả về HTTP {}", status.as_u16()));
    }
    let new_etag = header_str(&resp, "etag");
    let new_lm = header_str(&resp, "last-modified");
    let body = resp
        .text()
        .await
        .map_err(|e| anyhow!("không đọc được nội dung feed: {e}"))?;
    let (items, feed_title) = parse_feed(&body)?;
    Ok(FetchOutcome {
        items: Some(items),
        etag: new_etag,
        last_modified: new_lm,
        feed_title,
    })
}

fn header_str(resp: &reqwest::Response, name: &str) -> String {
    resp.headers()
        .get(name)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string()
}

/// Fetch an article page and extract readable text (for AI analysis).
pub async fn fetch_page_text(client: &reqwest::Client, url: &str) -> Result<String> {
    let resp = client
        .get(url)
        .header("Accept", "text/html,application/xhtml+xml")
        .send()
        .await
        .map_err(|e| anyhow!("không tải được trang: {e}"))?;
    if !resp.status().is_success() {
        return Err(anyhow!("trang trả về HTTP {}", resp.status().as_u16()));
    }
    let html = resp
        .text()
        .await
        .map_err(|e| anyhow!("không đọc được trang: {e}"))?;
    let text = extract_page_text(&html);
    if text.trim().is_empty() {
        return Err(anyhow!(
            "không trích được nội dung (trang có thể cần JavaScript)"
        ));
    }
    Ok(text)
}

// ---------------------------------------------------------------------------
// Feed parsing (RSS 2.0 + Atom)
// ---------------------------------------------------------------------------

/// Parse a feed document. Returns (items, feed_title).
pub fn parse_feed(xml: &str) -> Result<(Vec<FeedItem>, String)> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    let mut items: Vec<FeedItem> = Vec::new();
    let mut feed_title = String::new();
    let mut cur: Option<FeedItem> = None;
    // Path of local tag names from the root, e.g. ["rss","channel","item","title"].
    let mut path: Vec<String> = Vec::new();
    let mut buf = Vec::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let name = local_name(e.name().as_ref());
                path.push(name.clone());
                if name == "item" || name == "entry" {
                    cur = Some(FeedItem::default());
                }
                // Atom: <link href="..."/> is usually Empty, but some feeds
                // write <link ...></link> as Start+End.
                if name == "link" {
                    if let Some(it) = cur.as_mut() {
                        atom_link(&e, it);
                    }
                }
            }
            Ok(Event::Empty(e)) => {
                let name = local_name(e.name().as_ref());
                if let Some(it) = cur.as_mut() {
                    match name.as_str() {
                        "link" => atom_link(&e, it),
                        "enclosure" | "content" | "thumbnail" => media_url(&e, it),
                        _ => {}
                    }
                }
            }
            Ok(Event::Text(t)) => {
                let text = t.unescape().map(|c| c.into_owned()).unwrap_or_default();
                absorb_text(&path, &mut cur, &mut feed_title, &text);
            }
            Ok(Event::CData(t)) => {
                let text = String::from_utf8_lossy(t.as_ref()).into_owned();
                absorb_text(&path, &mut cur, &mut feed_title, &text);
            }
            Ok(Event::End(e)) => {
                let name = local_name(e.name().as_ref());
                if name == "item" || name == "entry" {
                    if let Some(mut it) = cur.take() {
                        it.title = strip_html(&it.title).trim().to_string();
                        it.description = clip(&strip_html(&it.description), 2000);
                        if !it.title.is_empty() && !(it.url.is_empty() && it.guid.is_empty()) {
                            if it.url.is_empty() {
                                it.url = it.guid.clone();
                            }
                            items.push(it);
                        }
                    }
                }
                // Tolerate slightly malformed nesting instead of dying mid-feed.
                if path.last().map(|p| p == &name).unwrap_or(false) {
                    path.pop();
                } else if let Some(pos) = path.iter().rposition(|p| p == &name) {
                    path.truncate(pos);
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(anyhow!("XML không hợp lệ: {e}")),
            _ => {}
        }
        buf.clear();
    }

    if items.is_empty() && feed_title.is_empty() {
        return Err(anyhow!(
            "không phải RSS/Atom (không tìm thấy item/entry nào)"
        ));
    }
    Ok((items, strip_html(&feed_title).trim().to_string()))
}

fn local_name(raw: &[u8]) -> String {
    let s = String::from_utf8_lossy(raw);
    s.rsplit(':').next().unwrap_or(&s).to_ascii_lowercase()
}

fn attr(e: &quick_xml::events::BytesStart, key: &str) -> String {
    e.attributes()
        .flatten()
        .find(|a| local_name(a.key.as_ref()) == key)
        .and_then(|a| a.unescape_value().ok().map(|v| v.into_owned()))
        .unwrap_or_default()
}

/// Atom `<link rel="alternate" href="…">` (rel omitted = alternate too).
fn atom_link(e: &quick_xml::events::BytesStart, it: &mut FeedItem) {
    let rel = attr(e, "rel");
    let href = attr(e, "href");
    if !href.is_empty() && (rel.is_empty() || rel == "alternate") && it.url.is_empty() {
        it.url = href;
    }
}

/// `<enclosure url=…>`, `<media:content url=…>`, `<media:thumbnail url=…>`.
fn media_url(e: &quick_xml::events::BytesStart, it: &mut FeedItem) {
    let ty = attr(e, "type");
    let url = attr(e, "url");
    if !url.is_empty() && it.image_url.is_empty() && (ty.is_empty() || ty.starts_with("image")) {
        it.image_url = url;
    }
}

/// Route a text/CDATA node to the right field based on the current path.
fn absorb_text(path: &[String], cur: &mut Option<FeedItem>, feed_title: &mut String, text: &str) {
    let Some(tag) = path.last().map(String::as_str) else {
        return;
    };
    let in_item = path.iter().any(|p| p == "item" || p == "entry");
    if !in_item {
        // channel/feed level
        if tag == "title" && feed_title.is_empty() {
            feed_title.push_str(text);
        }
        return;
    }
    let Some(it) = cur.as_mut() else { return };
    match tag {
        "title" => push_sp(&mut it.title, text),
        "link" => {
            if it.url.is_empty() {
                it.url = text.trim().to_string();
            }
        }
        "guid" | "id" => {
            if it.guid.is_empty() {
                it.guid = text.trim().to_string();
            }
        }
        "description" | "summary" | "encoded" => push_sp(&mut it.description, text),
        "content" => {
            // Atom <content> (inline text). RSS media:content is an Empty tag
            // handled elsewhere; a text node here is safe to use as body.
            if it.description.chars().count() < 200 {
                push_sp(&mut it.description, text);
            }
        }
        "author" | "creator" | "name" => {
            if it.author.is_empty() && !path.iter().any(|p| p == "source") {
                it.author = clip(text.trim(), 120);
            }
        }
        "category" => {
            if it.category.is_empty() {
                it.category = clip(text.trim(), 80);
            }
        }
        "pubdate" | "published" | "updated" | "date" => {
            if it.published_at == 0 {
                it.published_at = parse_date(text.trim());
            }
        }
        _ => {}
    }
}

fn push_sp(dst: &mut String, s: &str) {
    if !dst.is_empty() {
        dst.push(' ');
    }
    dst.push_str(s);
}

/// RFC 2822 ("Mon, 28 Jul 2026 08:30:00 +0700"), RFC 3339
/// ("2026-07-28T08:30:00+07:00"), plus common sloppy variants. 0 = unparsable.
pub fn parse_date(s: &str) -> i64 {
    if s.is_empty() {
        return 0;
    }
    if let Ok(d) = chrono::DateTime::parse_from_rfc2822(s) {
        return d.timestamp();
    }
    if let Ok(d) = chrono::DateTime::parse_from_rfc3339(s) {
        return d.timestamp();
    }
    // "2026-07-28 08:30:00" (assume UTC — better than dropping the date)
    if let Ok(d) = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S") {
        return d.and_utc().timestamp();
    }
    // RFC2822 with a stray weekday or "GMT+7" style — last resort: strip the
    // weekday and retry.
    if let Some(rest) = s.split_once(", ").map(|x| x.1) {
        if let Ok(d) = chrono::DateTime::parse_from_rfc2822(rest) {
            return d.timestamp();
        }
    }
    0
}

// ---------------------------------------------------------------------------
// Feed autodiscovery (tìm feed từ một trang web)
// ---------------------------------------------------------------------------

/// Extract RSS/Atom feed URLs advertised by an HTML page via
/// `<link rel="alternate" type="application/rss+xml" href="…">` (the standard
/// autodiscovery convention). Relative hrefs are resolved against `base_url`.
pub fn autodiscover_links(html: &str, base_url: &str) -> Vec<String> {
    let lower = html.to_ascii_lowercase();
    let mut out: Vec<String> = Vec::new();
    let mut pos = 0;
    while let Some(start) = lower[pos..].find("<link") {
        let start = pos + start;
        let Some(end) = lower[start..].find('>') else {
            break;
        };
        let tag = &html[start..start + end];
        pos = start + end + 1;

        let attr = |name: &str| -> String {
            let tl = tag.to_ascii_lowercase();
            let Some(i) = tl.find(&format!("{name}=")) else {
                return String::new();
            };
            let rest = &tag[i + name.len() + 1..];
            let mut chars = rest.chars();
            match chars.next() {
                Some(q @ ('"' | '\'')) => chars.as_str().split(q).next().unwrap_or("").to_string(),
                Some(c) => {
                    // unquoted value: up to whitespace or '>'
                    let mut v = c.to_string();
                    v.push_str(
                        chars
                            .as_str()
                            .split([' ', '\t', '\n', '/'])
                            .next()
                            .unwrap_or(""),
                    );
                    v
                }
                None => String::new(),
            }
        };

        let rel = attr("rel").to_ascii_lowercase();
        let ty = attr("type").to_ascii_lowercase();
        let href = attr("href");
        if href.is_empty() || !rel.contains("alternate") {
            continue;
        }
        if !(ty.contains("rss") || ty.contains("atom")) {
            continue;
        }
        let resolved = match url::Url::parse(base_url).and_then(|b| b.join(&href)) {
            Ok(u) => u.to_string(),
            Err(_) => continue,
        };
        if !out.contains(&resolved) {
            out.push(resolved);
        }
    }
    out
}

/// Feed URLs linked from an HTML page body — for the "RSS directory" pages
/// Vietnamese outlets use (vnexpress.net/rss lists ~30 feeds and carries no
/// autodiscovery tag at all, so `<link rel=alternate>` alone finds nothing).
pub fn scan_feed_hrefs(html: &str, base_url: &str) -> Vec<String> {
    let lower = html.to_ascii_lowercase();
    let mut out: Vec<String> = Vec::new();
    let mut pos = 0;
    while let Some(start) = lower[pos..].find("href=") {
        let start = pos + start + 5;
        pos = start;
        let rest = &html[start..];
        let mut chars = rest.chars();
        let href = match chars.next() {
            Some(q @ ('"' | '\'')) => chars.as_str().split(q).next().unwrap_or("").to_string(),
            Some(c) => {
                let mut v = c.to_string();
                v.push_str(
                    chars
                        .as_str()
                        .split([' ', '\t', '\n', '>', '"'])
                        .next()
                        .unwrap_or(""),
                );
                v
            }
            None => break,
        };
        let h = href.to_ascii_lowercase();
        let looks_like_feed = h.ends_with(".rss")
            || h.ends_with(".xml")
            || h.contains("/rss/")
            || h.contains("/feed/");
        // "/rss" alone is usually the directory page we're already reading.
        if !looks_like_feed || h.contains("javascript:") {
            continue;
        }
        if let Ok(u) = url::Url::parse(base_url).and_then(|b| b.join(&href)) {
            let s = u.to_string();
            if !out.contains(&s) {
                out.push(s);
            }
        }
    }
    out
}

/// Common feed paths tried as a fallback when a site's homepage advertises
/// nothing (only for root-ish URLs — probing deep pages is just noise).
pub fn common_feed_paths(input_url: &str) -> Vec<String> {
    let Ok(u) = url::Url::parse(input_url) else {
        return Vec::new();
    };
    if !matches!(u.path(), "" | "/") {
        return Vec::new();
    }
    ["/rss", "/feed", "/rss.xml", "/atom.xml", "/index.rss"]
        .iter()
        .filter_map(|p| u.join(p).ok().map(|x| x.to_string()))
        .collect()
}

// ---------------------------------------------------------------------------
// Page scraping (nguồn không có RSS — quét thẳng nội dung trang)
// ---------------------------------------------------------------------------

/// A headline is long. Nav chrome ("Trang chủ", "Thể thao", "Xem thêm") is
/// short, and that single fact removes most of the noise on a listing page.
const MIN_HEADLINE_CHARS: usize = 24;
/// …and real headlines are sentences, not two-word labels.
const MIN_HEADLINE_WORDS: usize = 4;
/// Upper bound on links harvested from one page, so a sitemap-ish page cannot
/// turn one fetch into hundreds of article requests.
const MAX_SCRAPED_LINKS: usize = 60;

/// Result of a conditional GET on an ordinary HTML page.
pub struct HtmlOutcome {
    /// None = HTTP 304, page unchanged since last visit.
    pub html: Option<String>,
    pub etag: String,
    pub last_modified: String,
}

/// Metadata lifted from an article page's `<head>` — the scrape-mode stand-in
/// for the fields RSS would have handed us.
#[derive(Debug, Clone, Default)]
pub struct PageMeta {
    pub title: String,
    pub description: String,
    pub image_url: String,
    pub author: String,
    /// Unix seconds; 0 = the page didn't say.
    pub published_at: i64,
    /// `og:type`, lowercased — `"article"` on a real story, `"website"` on a
    /// section landing page. The one dependable way to tell them apart when
    /// the URL shape cannot ([`PageMeta::is_article`]).
    pub og_type: String,
}

impl PageMeta {
    /// Is this an article page rather than a section index?
    ///
    /// Listing pages link to other listing pages ("Bất động sản", "Chính sách
    /// quyền riêng tư") with slugs indistinguishable from article slugs, so the
    /// link scanner cannot filter them out — but the page itself says what it
    /// is. Either marker is enough: `og:type=article`, or a publish date, which
    /// a section index never carries.
    pub fn is_article(&self) -> bool {
        self.og_type == "article" || self.published_at > 0
    }
}

/// Fetch a plain HTML page with the same conditional-GET manners as
/// [`fetch_feed`] — a listing page is polled just as often as a feed, and many
/// sites do honour `ETag` on them.
pub async fn fetch_html(
    client: &reqwest::Client,
    url: &str,
    etag: &str,
    last_modified: &str,
) -> Result<HtmlOutcome> {
    let mut req = client
        .get(url)
        .header("Accept", "text/html,application/xhtml+xml");
    if !etag.is_empty() {
        req = req.header("If-None-Match", etag);
    }
    if !last_modified.is_empty() {
        req = req.header("If-Modified-Since", last_modified);
    }
    let resp = req
        .send()
        .await
        .map_err(|e| anyhow!("không tải được trang: {e}"))?;
    let status = resp.status();
    if status.as_u16() == 304 {
        return Ok(HtmlOutcome {
            html: None,
            etag: etag.to_string(),
            last_modified: last_modified.to_string(),
        });
    }
    if !status.is_success() {
        return Err(anyhow!("trang trả về HTTP {}", status.as_u16()));
    }
    let new_etag = header_str(&resp, "etag");
    let new_lm = header_str(&resp, "last-modified");
    let html = resp
        .text()
        .await
        .map_err(|e| anyhow!("không đọc được trang: {e}"))?;
    Ok(HtmlOutcome {
        html: Some(html),
        etag: new_etag,
        last_modified: new_lm,
    })
}

/// Read one attribute out of a single tag's source text. Handles quoted and
/// bare values; returns `""` when absent.
fn tag_attr(tag: &str, name: &str) -> String {
    let tl = tag.to_ascii_lowercase();
    let pat = format!("{name}=");
    let mut from = 0;
    // Match on an attribute boundary so `data-href=` doesn't answer for `href=`.
    let i = loop {
        let Some(rel) = tl[from..].find(&pat) else {
            return String::new();
        };
        let at = from + rel;
        let prev_ok = at == 0 || tl.as_bytes()[at - 1].is_ascii_whitespace();
        if prev_ok {
            break at;
        }
        from = at + pat.len();
    };
    let rest = &tag[i + pat.len()..];
    let mut chars = rest.chars();
    match chars.next() {
        Some(q @ ('"' | '\'')) => chars.as_str().split(q).next().unwrap_or("").to_string(),
        Some(c) => {
            let mut v = c.to_string();
            v.push_str(
                chars
                    .as_str()
                    .split([' ', '\t', '\n', '\r', '>', '/'])
                    .next()
                    .unwrap_or(""),
            );
            v
        }
        None => String::new(),
    }
}

/// Path segments that are never an article on a news site.
const NON_ARTICLE_SEGMENTS: [&str; 12] = [
    "tag",
    "tags",
    "search",
    "tim-kiem",
    "login",
    "dang-nhap",
    "register",
    "account",
    "subscribe",
    "rss",
    "feed",
    "author",
];

/// Does this path look like an article rather than a section index?
///
/// News slugs are hyphenated or carry a numeric id (`/bai-viet-abc-123.html`);
/// section roots are single bare words (`/the-thao`, `/kinh-doanh`). Not
/// perfect — a wrong guess costs one skipped link or one junk row, both cheap.
fn looks_like_article_path(path: &str) -> bool {
    let segs: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    let Some(last) = segs.last() else {
        return false;
    };
    if segs
        .iter()
        .any(|s| NON_ARTICLE_SEGMENTS.contains(&s.to_ascii_lowercase().as_str()))
    {
        return false;
    }
    let stem = last.rsplit_once('.').map(|(s, _)| s).unwrap_or(last);
    // A slug has internal structure; a section root ("the-thao") is one word —
    // so require either a digit (an id) or several hyphen-joined parts…
    if stem.chars().any(|c| c.is_ascii_digit()) || stem.matches('-').count() >= 2 {
        return true;
    }
    // …or a date-style permalink (/2026/07/29/tin-moi), where the slug itself
    // is short but the depth gives it away.
    segs.len() >= 3 && stem.contains('-')
}

fn same_site(a: &url::Url, b: &url::Url) -> bool {
    let host = |u: &url::Url| {
        u.host_str()
            .unwrap_or("")
            .trim_start_matches("www.")
            .to_ascii_lowercase()
    };
    host(a) == host(b)
}

/// Harvest article links out of an ordinary listing/category page.
///
/// This is the RSS-less path: an `<a>` whose anchor text reads like a headline
/// and whose href looks like an article becomes a [`FeedItem`] carrying just
/// title + url. Dates and summaries are not on the listing page in any
/// dependable form — [`fetch_page_meta`] fills those in per article afterwards.
///
/// Deliberately heuristic and deliberately strict: on a listing page a missed
/// headline shows up again next cycle, whereas a swallowed nav link becomes a
/// permanent junk row.
pub fn scan_page_articles(html: &str, base_url: &str) -> Vec<FeedItem> {
    let Ok(base) = url::Url::parse(base_url) else {
        return Vec::new();
    };
    let mut h = html.to_string();
    for tag in [
        "script", "style", "noscript", "svg", "nav", "header", "footer", "form", "iframe",
    ] {
        h = drop_blocks(&h, tag);
    }
    let lower = h.to_ascii_lowercase();

    // url -> headline. Listing pages link the same article two or three times:
    // thumbnail (no text — filtered by the length rule), then the headline,
    // then a teaser paragraph. Document order puts the headline first, so the
    // FIRST qualifying anchor wins; taking the longest would pick the teaser.
    let mut best: Vec<(String, String)> = Vec::new();
    let mut pos = 0;
    while let Some(rel) = lower[pos..].find("<a") {
        let start = pos + rel;
        pos = start + 2;
        // "<a" must open an anchor, not "<article" / "<aside".
        if !lower[pos..].starts_with(|c: char| c.is_ascii_whitespace()) {
            continue;
        }
        let Some(open_end) = h[start..].find('>') else {
            break;
        };
        let tag = &h[start..start + open_end];
        let content_start = start + open_end + 1;
        let Some(close) = lower[content_start..].find("</a>") else {
            break;
        };
        let title = decode_entities(&strip_html(&h[content_start..content_start + close]))
            .trim()
            .to_string();
        pos = content_start + close + 4;

        if title.chars().count() < MIN_HEADLINE_CHARS
            || title.split_whitespace().count() < MIN_HEADLINE_WORDS
        {
            continue;
        }
        let href = tag_attr(tag, "href");
        if href.is_empty() || href.starts_with('#') {
            continue;
        }
        let Ok(u) = base.join(&href) else { continue };
        if !matches!(u.scheme(), "http" | "https") || !same_site(&u, &base) {
            continue;
        }
        if !looks_like_article_path(u.path()) {
            continue;
        }
        let mut clean = u.clone();
        clean.set_fragment(None);
        let key = clean.to_string();
        if !best.iter().any(|(k, _)| *k == key) {
            best.push((key, title));
        }
    }

    best.truncate(MAX_SCRAPED_LINKS);
    best.into_iter()
        .map(|(url, title)| FeedItem {
            title,
            url,
            ..Default::default()
        })
        .collect()
}

/// Drop a trailing `" | Vietstock"` / `" - VnExpress"` outlet stamp from a
/// headline, but only when the page's own `og:site_name` says that is what it
/// is. Left in, the outlet name rides into every title and shows up as a fake
/// trending phrase; guessed at, it would eat real words — so this only cuts
/// what the page itself identified.
fn strip_site_suffix(title: &str, site_name: &str) -> String {
    let title = title.trim();
    let site = site_name.trim();
    if site.is_empty() {
        return title.to_string();
    }
    for sep in [" | ", " - ", " — ", " – ", " · "] {
        if let Some(head) = title
            .rsplit_once(sep)
            .and_then(|(h, tail)| tail.trim().eq_ignore_ascii_case(site).then_some(h.trim()))
        {
            // Never strip the whole title away (a section page titled with just
            // the outlet name would end up empty).
            if !head.is_empty() {
                return head.to_string();
            }
        }
    }
    title.to_string()
}

/// Parse the `<head>` metadata of an article page (Open Graph first, then the
/// plain HTML equivalents).
pub fn parse_page_meta(html: &str) -> PageMeta {
    let lower = html.to_ascii_lowercase();
    let mut tags: Vec<(String, String)> = Vec::new();
    let mut pos = 0;
    while let Some(rel) = lower[pos..].find("<meta") {
        let start = pos + rel;
        let Some(end) = html[start..].find('>') else {
            break;
        };
        let tag = &html[start..start + end];
        pos = start + end + 1;
        let key = {
            let p = tag_attr(tag, "property");
            if p.is_empty() {
                tag_attr(tag, "name")
            } else {
                p
            }
        };
        let content = tag_attr(tag, "content");
        if !key.is_empty() && !content.is_empty() {
            tags.push((key.to_ascii_lowercase(), decode_entities(&content)));
        }
    }
    let pick = |keys: &[&str]| -> String {
        for k in keys {
            if let Some((_, v)) = tags.iter().find(|(tk, _)| tk == k) {
                if !v.trim().is_empty() {
                    return v.trim().to_string();
                }
            }
        }
        String::new()
    };

    let published = pick(&[
        "article:published_time",
        "og:article:published_time",
        "datepublished",
        "publishdate",
        "pubdate",
        "date",
    ]);
    let published_at = if published.is_empty() {
        // Fall back to the first <time datetime="…"> in the body.
        lower
            .find("<time")
            .and_then(|i| {
                html[i..]
                    .find('>')
                    .map(|e| tag_attr(&html[i..i + e], "datetime"))
            })
            .map(|d| parse_date(&d))
            .unwrap_or(0)
    } else {
        parse_date(&published)
    };

    PageMeta {
        title: strip_site_suffix(
            &pick(&["og:title", "twitter:title"]),
            &pick(&["og:site_name"]),
        ),
        description: pick(&["og:description", "twitter:description", "description"]),
        image_url: pick(&["og:image", "twitter:image", "twitter:image:src"]),
        author: pick(&["article:author", "author", "og:article:author"]),
        published_at,
        og_type: pick(&["og:type"]).to_ascii_lowercase(),
    }
}

/// Fetch one article page and read its `<head>` metadata. Errors are the
/// caller's cue to keep the link with just its anchor text.
pub async fn fetch_page_meta(client: &reqwest::Client, url: &str) -> Result<PageMeta> {
    let resp = client
        .get(url)
        .header("Accept", "text/html,application/xhtml+xml")
        .send()
        .await
        .map_err(|e| anyhow!("không tải được trang: {e}"))?;
    if !resp.status().is_success() {
        return Err(anyhow!("trang trả về HTTP {}", resp.status().as_u16()));
    }
    let html = resp
        .text()
        .await
        .map_err(|e| anyhow!("không đọc được trang: {e}"))?;
    Ok(parse_page_meta(&html))
}

/// `<title>` of a page, for naming a scrape source added with only a URL.
pub fn page_title(html: &str) -> String {
    let lower = html.to_ascii_lowercase();
    let Some(start) = lower.find("<title") else {
        return String::new();
    };
    let Some(open_end) = html[start..].find('>') else {
        return String::new();
    };
    let content_start = start + open_end + 1;
    let Some(end) = lower[content_start..].find("</title>") else {
        return String::new();
    };
    // Page titles are usually "Headline — Section | Outlet"; keep the outlet.
    decode_entities(&strip_html(&html[content_start..content_start + end]))
        .trim()
        .to_string()
}

// ---------------------------------------------------------------------------
// HTML → text
// ---------------------------------------------------------------------------

/// Decode HTML entities, leaving everything else — including line breaks —
/// untouched. Separate from [`strip_html`] so already-extracted article text
/// can be repaired without losing its paragraph breaks.
pub fn decode_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '&' {
            out.push(c);
            continue;
        }
        let mut ent = String::new();
        while let Some(&n) = chars.peek() {
            if n == ';' || ent.chars().count() > 10 {
                break;
            }
            ent.push(n);
            chars.next();
        }
        if chars.peek() == Some(&';') {
            chars.next();
            out.push_str(&decode_entity(&ent));
        } else {
            out.push('&');
            out.push_str(&ent);
        }
    }
    out
}

/// Strip tags + decode entities. Whitespace collapsed (for titles/snippets).
pub fn strip_html(s: &str) -> String {
    let mut stripped = String::with_capacity(s.len());
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => {
                in_tag = true;
                stripped.push(' ');
            }
            '>' if in_tag => in_tag = false,
            _ if !in_tag => stripped.push(c),
            _ => {}
        }
    }
    decode_entities(&stripped)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Named entities for accented Latin letters. Vietnamese CMSes emit these
/// constantly (`C&ocirc;ng ty`, `xem x&eacute;t`), and an unknown name is
/// rendered literally — so a missing row here shows up as raw `&eacute;` in
/// the article text. Vietnamese-only letters (ơ ư ạ ả …) have no named form
/// and arrive as numeric references, handled separately.
const NAMED_ENTITIES: &[(&str, char)] = &[
    ("agrave", 'à'),
    ("aacute", 'á'),
    ("acirc", 'â'),
    ("atilde", 'ã'),
    ("auml", 'ä'),
    ("aring", 'å'),
    ("Agrave", 'À'),
    ("Aacute", 'Á'),
    ("Acirc", 'Â'),
    ("Atilde", 'Ã'),
    ("Auml", 'Ä'),
    ("Aring", 'Å'),
    ("egrave", 'è'),
    ("eacute", 'é'),
    ("ecirc", 'ê'),
    ("euml", 'ë'),
    ("Egrave", 'È'),
    ("Eacute", 'É'),
    ("Ecirc", 'Ê'),
    ("Euml", 'Ë'),
    ("igrave", 'ì'),
    ("iacute", 'í'),
    ("icirc", 'î'),
    ("iuml", 'ï'),
    ("Igrave", 'Ì'),
    ("Iacute", 'Í'),
    ("Icirc", 'Î'),
    ("Iuml", 'Ï'),
    ("ograve", 'ò'),
    ("oacute", 'ó'),
    ("ocirc", 'ô'),
    ("otilde", 'õ'),
    ("ouml", 'ö'),
    ("oslash", 'ø'),
    ("Ograve", 'Ò'),
    ("Oacute", 'Ó'),
    ("Ocirc", 'Ô'),
    ("Otilde", 'Õ'),
    ("Ouml", 'Ö'),
    ("Oslash", 'Ø'),
    ("ugrave", 'ù'),
    ("uacute", 'ú'),
    ("ucirc", 'û'),
    ("uuml", 'ü'),
    ("Ugrave", 'Ù'),
    ("Uacute", 'Ú'),
    ("Ucirc", 'Û'),
    ("Uuml", 'Ü'),
    ("yacute", 'ý'),
    ("yuml", 'ÿ'),
    ("Yacute", 'Ý'),
    ("ntilde", 'ñ'),
    ("Ntilde", 'Ñ'),
    ("ccedil", 'ç'),
    ("Ccedil", 'Ç'),
    ("eth", 'ð'),
    ("ETH", 'Ð'),
    ("dstrok", 'đ'),
    ("Dstrok", 'Đ'),
    ("szlig", 'ß'),
    ("thorn", 'þ'),
    ("THORN", 'Þ'),
    ("aelig", 'æ'),
    ("AElig", 'Æ'),
    ("deg", '°'),
    ("plusmn", '±'),
    ("times", '×'),
    ("divide", '÷'),
    ("micro", 'µ'),
    ("euro", '€'),
    ("pound", '£'),
    ("yen", '¥'),
    ("cent", '¢'),
    ("copy", '©'),
    ("reg", '®'),
    ("trade", '™'),
    ("bull", '•'),
    ("middot", '·'),
    ("laquo", '«'),
    ("raquo", '»'),
    ("sbquo", '‚'),
    ("bdquo", '„'),
    ("dagger", '†'),
    ("permil", '‰'),
    ("frac12", '½'),
];

fn decode_entity(ent: &str) -> String {
    match ent {
        "amp" => "&".into(),
        "lt" => "<".into(),
        "gt" => ">".into(),
        "quot" => "\"".into(),
        "apos" | "#39" => "'".into(),
        "nbsp" => " ".into(),
        "hellip" => "…".into(),
        "ndash" => "–".into(),
        "mdash" => "—".into(),
        "lsquo" => "'".into(),
        "rsquo" => "'".into(),
        "ldquo" => "\u{201C}".into(),
        "rdquo" => "\u{201D}".into(),
        name if NAMED_ENTITIES.iter().any(|(n, _)| *n == name) => NAMED_ENTITIES
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, c)| c.to_string())
            .unwrap_or_default(),
        _ => {
            if let Some(num) = ent.strip_prefix("#x").or_else(|| ent.strip_prefix("#X")) {
                if let Ok(n) = u32::from_str_radix(num, 16) {
                    if let Some(c) = char::from_u32(n) {
                        return c.to_string();
                    }
                }
            } else if let Some(num) = ent.strip_prefix('#') {
                if let Ok(n) = num.parse::<u32>() {
                    if let Some(c) = char::from_u32(n) {
                        return c.to_string();
                    }
                }
            }
            format!("&{ent};")
        }
    }
}

/// Remove `<tag …>…</tag>` blocks wholesale (script, style, …), case-insensitive.
fn drop_blocks(html: &str, tag: &str) -> String {
    // ASCII-lowercase keeps byte offsets identical to the original string —
    // full to_lowercase() can change byte lengths and desync the indexes.
    let lower = html.to_ascii_lowercase();
    let open = format!("<{tag}");
    let close = format!("</{tag}>");
    let mut out = String::with_capacity(html.len());
    let mut pos = 0;
    while let Some(start) = lower[pos..].find(&open) {
        let start = pos + start;
        out.push_str(&html[pos..start]);
        match lower[start..].find(&close) {
            Some(end) => pos = start + end + close.len(),
            None => {
                pos = html.len();
                break;
            }
        }
    }
    out.push_str(&html[pos..]);
    out
}

/// Readable text of an article page. Prefers <p> blocks; falls back to a full
/// strip when the page has few paragraphs. Capped at 20k chars.
pub fn extract_page_text(html: &str) -> String {
    let mut h = html.to_string();
    for tag in [
        "script", "style", "noscript", "svg", "nav", "header", "footer", "form", "iframe",
    ] {
        h = drop_blocks(&h, tag);
    }
    // Collect <p>…</p> inner HTML (ASCII-lowercase: offsets stay valid).
    let lower = h.to_ascii_lowercase();
    let mut paras: Vec<String> = Vec::new();
    let mut pos = 0;
    while let Some(start) = lower[pos..].find("<p") {
        let start = pos + start;
        let Some(open_end) = h[start..].find('>') else {
            break;
        };
        let content_start = start + open_end + 1;
        let Some(end) = lower[content_start..].find("</p>") else {
            break;
        };
        let inner = &h[content_start..content_start + end];
        let text = strip_html(inner);
        if text.chars().count() > 40 {
            paras.push(text);
        }
        pos = content_start + end + 4;
    }
    let joined = paras.join("\n\n");
    let text = if joined.chars().count() >= 400 {
        joined
    } else {
        strip_html(&h)
    };
    clip(&text, 20_000)
}

/// Truncate on a char boundary (never mid-multibyte — [[utf8-preview-slice-panic]]).
pub fn clip(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    s.chars().take(max_chars).collect::<String>() + "…"
}

// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const RSS: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<rss version="2.0" xmlns:media="http://search.yahoo.com/mrss/">
<channel>
  <title>VnExpress - Tin nhanh</title>
  <item>
    <title><![CDATA[Giá vàng lập đỉnh mới]]></title>
    <link>https://vnexpress.net/gia-vang-123.html</link>
    <guid>https://vnexpress.net/gia-vang-123.html</guid>
    <description><![CDATA[<p>Giá vàng miếng sáng nay <b>tăng mạnh</b>&nbsp;lên đỉnh.</p>]]></description>
    <pubDate>Mon, 27 Jul 2026 08:30:00 +0700</pubDate>
    <category>Kinh doanh</category>
    <media:content url="https://i1.vnecdn.net/vang.jpg" type="image/jpeg"/>
  </item>
  <item>
    <title>Bão số 3 đổ bộ</title>
    <link>https://vnexpress.net/bao-so-3.html</link>
    <pubDate>Mon, 27 Jul 2026 09:00:00 +0700</pubDate>
  </item>
</channel>
</rss>"#;

    const ATOM: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<feed xmlns="http://www.w3.org/2005/Atom">
  <title>The Verge</title>
  <entry>
    <title>New AI model released</title>
    <link rel="alternate" href="https://theverge.com/ai-model"/>
    <id>tag:theverge.com,2026:1</id>
    <summary>A new model &amp; benchmark results.</summary>
    <published>2026-07-27T10:00:00Z</published>
    <author><name>Alex</name></author>
  </entry>
</feed>"#;

    #[test]
    fn parses_rss_with_cdata_media_and_dates() {
        let (items, title) = parse_feed(RSS).unwrap();
        assert_eq!(title, "VnExpress - Tin nhanh");
        assert_eq!(items.len(), 2);
        let it = &items[0];
        assert_eq!(it.title, "Giá vàng lập đỉnh mới");
        assert_eq!(it.url, "https://vnexpress.net/gia-vang-123.html");
        assert_eq!(
            it.description,
            "Giá vàng miếng sáng nay tăng mạnh lên đỉnh."
        );
        assert_eq!(it.category, "Kinh doanh");
        assert_eq!(it.image_url, "https://i1.vnecdn.net/vang.jpg");
        assert_eq!(it.published_at, 1785115800); // 2026-07-27 01:30 UTC
    }

    #[test]
    fn parses_atom_link_href_and_rfc3339() {
        let (items, title) = parse_feed(ATOM).unwrap();
        assert_eq!(title, "The Verge");
        assert_eq!(items.len(), 1);
        let it = &items[0];
        assert_eq!(it.url, "https://theverge.com/ai-model");
        assert_eq!(it.description, "A new model & benchmark results.");
        assert_eq!(it.author, "Alex");
        assert!(it.published_at > 0);
    }

    #[test]
    fn rejects_non_feed_html() {
        assert!(parse_feed("<html><body>hello</body></html>").is_err());
    }

    #[test]
    fn strip_html_decodes_entities() {
        assert_eq!(strip_html("<b>a&amp;b</b> &#72;i &nbsp; x"), "a&b Hi x");
        assert_eq!(strip_html("gi&#225; v&#224;ng"), "giá vàng");
    }

    #[test]
    fn strip_html_decodes_accented_named_entities() {
        // Chuỗi thật từ một bài Thanh Niên: CMS Việt Nam trộn UTF-8 với entity
        // có tên cho chữ Latin có dấu.
        assert_eq!(
            strip_html("C&ocirc;ng ty CP Thương mại dịch vụ TTC Ch&acirc;u Th&agrave;nh"),
            "Công ty CP Thương mại dịch vụ TTC Châu Thành"
        );
        assert_eq!(strip_html("xem x&eacute;t hồ sơ"), "xem xét hồ sơ");
        assert_eq!(
            strip_html("bi&ecirc;n bản đ&atilde; ho&agrave;n th&agrave;nh"),
            "biên bản đã hoàn thành"
        );
        assert_eq!(
            strip_html("ch&iacute;nh thức c&ugrave;ng c&aacute;c"),
            "chính thức cùng các"
        );
        assert_eq!(strip_html("&Ocirc;ng &Aacute;nh"), "Ông Ánh");
        // Không phải entity hợp lệ thì giữ nguyên, không nuốt mất chữ.
        assert_eq!(strip_html("A &khongcoentity; B"), "A &khongcoentity; B");
    }

    #[test]
    fn decode_entities_keeps_paragraph_breaks() {
        let text = "Đoạn m&ocirc;̣t.\n\nĐoạn hai xem x&eacute;t.";
        let out = decode_entities(text);
        assert!(out.contains("\n\n"), "phải giữ xuống dòng: {out:?}");
        assert!(out.contains("xem xét"));
        assert!(!out.contains("&eacute;"));
    }

    #[test]
    fn date_variants() {
        assert!(parse_date("Mon, 27 Jul 2026 08:30:00 +0700") > 0);
        assert!(parse_date("2026-07-27T10:00:00+07:00") > 0);
        assert!(parse_date("2026-07-27 10:00:00") > 0);
        assert_eq!(parse_date("hôm qua"), 0);
    }

    #[test]
    fn extract_page_prefers_paragraphs_and_drops_script() {
        let html = format!(
            "<html><head><style>p{{color:red}}</style><script>alert(1)</script></head>\
             <body><nav>Menu Menu</nav><p>{}</p><p>{}</p><footer>© 2026</footer></body></html>",
            "Đây là đoạn văn thứ nhất của bài báo, đủ dài để được giữ lại trong phần trích xuất nội dung chính.",
            "Đoạn thứ hai nói về diễn biến tiếp theo của sự kiện, cũng đủ dài để vượt ngưỡng bốn mươi ký tự."
        );
        let text = extract_page_text(&html);
        assert!(text.contains("đoạn văn thứ nhất"));
        assert!(text.contains("diễn biến tiếp theo"));
        assert!(!text.contains("alert"));
        assert!(!text.contains("Menu"));
        assert!(!text.contains("©"));
    }

    // ---- page scraping (nguồn không có RSS) ----

    /// A listing page shaped like a real Vietnamese outlet's category page:
    /// nav chrome, a thumbnail link and a headline link to the SAME article,
    /// an off-site ad, a section root, and a tag page.
    const LISTING: &str = r##"<html><head><title>Thời sự — Báo Mẫu</title></head><body>
      <nav><a href="/">Trang chủ</a><a href="/the-thao">Thể thao</a></nav>
      <div class="item">
        <a href="/thoi-su/ha-noi-cam-xe-tren-pho-co-tu-thang-8-123456.html"><img src="/t.jpg"></a>
        <h3><a href="/thoi-su/ha-noi-cam-xe-tren-pho-co-tu-thang-8-123456.html">Hà Nội cấm xe trên phố cổ từ tháng 8</a></h3>
        <p><a href="/thoi-su/ha-noi-cam-xe-tren-pho-co-tu-thang-8-123456.html">Từ 1/8, toàn bộ phố cổ Hà Nội cấm xe máy vào cuối tuần để mở rộng không gian đi bộ, theo quyết định vừa ban hành.</a></p>
      </div>
      <div class="item">
        <a
           href="https://bao-mau.vn/kinh-doanh/gia-vang-trong-nuoc-lap-dinh-moi-987654.html">Giá vàng trong nước lập đỉnh mới sáng nay</a>
      </div>
      <a href="/thoi-su">Thời sự</a>
      <a href="/tag/ha-noi-nhieu-tin-tuc-moi-nhat-hom-nay">Hà Nội mới nhất hôm nay có gì</a>
      <a href="https://quang-cao.example.com/mua-ngay-uu-dai-lon-nhat-nam-2026">Mua ngay ưu đãi lớn nhất năm</a>
      <a href="#top">Lên đầu trang xem thêm tin tức khác</a>
      <footer><a href="/gioi-thieu-ve-chung-toi-va-toa-soan">Giới thiệu về chúng tôi và toà soạn</a></footer>
    </body></html>"##;

    #[test]
    fn scan_page_articles_keeps_headlines_and_drops_chrome() {
        let items = scan_page_articles(LISTING, "https://bao-mau.vn/thoi-su");
        let urls: Vec<&str> = items.iter().map(|i| i.url.as_str()).collect();
        assert_eq!(items.len(), 2, "chỉ 2 bài thật: {urls:?}");

        // Same article linked three times (thumbnail, headline, teaser) → one
        // row, and the HEADLINE wins — the teaser is longer, so "longest anchor"
        // would have stored the summary paragraph as the title.
        let a = items
            .iter()
            .find(|i| i.url.contains("ha-noi-cam-xe"))
            .unwrap();
        assert_eq!(a.title, "Hà Nội cấm xe trên phố cổ từ tháng 8");
        // Absolute href on the same host, and a multi-line <a> tag, both work.
        assert!(items
            .iter()
            .any(|i| i.title.starts_with("Giá vàng trong nước")));

        // Rejected: nav, section root, /tag/, off-site, fragment, footer blocks.
        assert!(
            !urls.iter().any(|u| u.contains("quang-cao")),
            "link ngoài site"
        );
        assert!(!urls.iter().any(|u| u.contains("/tag/")), "trang tag");
        assert!(
            !urls.iter().any(|u| u.ends_with("/thoi-su")),
            "trang chuyên mục"
        );
        assert!(
            !urls.iter().any(|u| u.contains("gioi-thieu")),
            "link trong <footer>"
        );
    }

    #[test]
    fn scan_page_articles_needs_a_resolvable_base() {
        assert!(scan_page_articles(LISTING, "not a url").is_empty());
        assert!(scan_page_articles("<html></html>", "https://x.vn").is_empty());
    }

    #[test]
    fn looks_like_article_path_separates_slugs_from_sections() {
        assert!(looks_like_article_path(
            "/thoi-su/ha-noi-cam-xe-123456.html"
        ));
        assert!(looks_like_article_path("/2026/07/29/tin-moi"));
        assert!(!looks_like_article_path("/the-thao"));
        assert!(!looks_like_article_path("/"));
        assert!(!looks_like_article_path("/tag/ha-noi-tin-moi-nhat"));
    }

    #[test]
    fn parse_page_meta_reads_open_graph() {
        let html = r#"<html><head>
          <meta property="og:title" content="Hà Nội c&#7845;m xe trên ph&#7889; c&#7893;">
          <meta property="og:description" content="Từ tháng 8, phố cổ cấm xe máy cuối tuần.">
          <meta property="og:image" content="https://bao-mau.vn/img/1.jpg">
          <meta property="article:published_time" content="2026-07-29T08:30:00+07:00">
          <meta property="og:type" content="article">
          <meta name="author" content="Minh Anh">
        </head><body></body></html>"#;
        let m = parse_page_meta(html);
        assert_eq!(
            m.title, "Hà Nội cấm xe trên phố cổ",
            "entity phải được giải mã"
        );
        assert!(m.description.starts_with("Từ tháng 8"));
        assert_eq!(m.image_url, "https://bao-mau.vn/img/1.jpg");
        assert_eq!(m.author, "Minh Anh");
        assert!(m.published_at > 1_780_000_000, "phải parse được ngày ISO");
        assert!(m.is_article());
    }

    #[test]
    fn strip_site_suffix_only_cuts_the_declared_outlet() {
        assert_eq!(
            strip_site_suffix("Top cổ phiếu đáng chú ý | Vietstock", "Vietstock"),
            "Top cổ phiếu đáng chú ý"
        );
        assert_eq!(
            strip_site_suffix("Giá vàng lập đỉnh - VnExpress", "vnexpress"),
            "Giá vàng lập đỉnh"
        );
        // Đuôi không phải tên trang → giữ nguyên, không đoán.
        assert_eq!(
            strip_site_suffix("Nga - Ukraine đàm phán", "Vietstock"),
            "Nga - Ukraine đàm phán"
        );
        // Không biết tên trang → không cắt gì.
        assert_eq!(
            strip_site_suffix("Tin nóng | Báo X", ""),
            "Tin nóng | Báo X"
        );
        // Tiêu đề chỉ có mỗi tên trang → không được cắt thành rỗng.
        assert_eq!(
            strip_site_suffix("Trang chủ | Vietstock", "Vietstock"),
            "Trang chủ"
        );
        assert_eq!(strip_site_suffix("Vietstock", "Vietstock"), "Vietstock");
    }

    #[test]
    fn parse_page_meta_strips_outlet_from_title() {
        let html = r#"<meta property="og:site_name" content="Vietstock">
          <meta property="og:title" content="TVC bán bất thành 24.8 triệu cp TVB | Vietstock">
          <meta property="og:type" content="article">"#;
        assert_eq!(
            parse_page_meta(html).title,
            "TVC bán bất thành 24.8 triệu cp TVB"
        );
    }

    #[test]
    fn is_article_rejects_section_landing_pages() {
        // Trang chuyên mục: og:type=website, không có ngày đăng.
        let section = r#"<html><head>
          <meta property="og:type" content="website">
          <meta property="og:title" content="Bất động sản — tin nhà đất">
        </head><body></body></html>"#;
        assert!(!parse_page_meta(section).is_article());

        // Hai dấu hiệu độc lập — chỉ cần một cái là đủ.
        let only_type = r#"<meta property="og:type" content="article">"#;
        assert!(parse_page_meta(only_type).is_article());
        let only_date =
            r#"<meta property="article:published_time" content="2026-07-29T08:30:00+07:00">"#;
        assert!(parse_page_meta(only_date).is_article());
    }

    #[test]
    fn parse_page_meta_falls_back_to_time_tag() {
        let html = r#"<html><head><meta name="description" content="Tóm tắt ngắn."></head>
          <body><time datetime="2026-07-29T09:00:00Z">hôm nay</time></body></html>"#;
        let m = parse_page_meta(html);
        assert_eq!(m.description, "Tóm tắt ngắn.");
        assert!(m.published_at > 1_780_000_000);
        // Nothing claimed a title or image — those stay empty, not guessed.
        assert!(m.title.is_empty() && m.image_url.is_empty());
    }

    #[test]
    fn tag_attr_matches_on_attribute_boundary() {
        assert_eq!(
            tag_attr(r#"<a data-href="/x" href="/real">"#, "href"),
            "/real"
        );
        assert_eq!(
            tag_attr("<a href=/bare-slug-1 class=z>", "href"),
            "/bare-slug-1"
        );
        assert_eq!(tag_attr(r#"<a class="c">"#, "href"), "");
    }

    #[test]
    fn page_title_reads_head_title() {
        assert_eq!(page_title(LISTING), "Thời sự — Báo Mẫu");
        assert_eq!(page_title("<html><body>no title</body></html>"), "");
    }

    #[test]
    fn autodiscover_finds_feed_links() {
        let html = r#"<html><head>
            <link rel="stylesheet" href="/style.css">
            <link rel="alternate" type="application/rss+xml" title="RSS" href="/rss/tin-moi-nhat.rss">
            <link rel='alternate' type='application/atom+xml' href='https://other.example/atom.xml'>
            <link rel="alternate" type="text/html" href="/mobile">
        </head><body></body></html>"#;
        let links = autodiscover_links(html, "https://vnexpress.net/");
        assert_eq!(
            links,
            vec![
                "https://vnexpress.net/rss/tin-moi-nhat.rss".to_string(),
                "https://other.example/atom.xml".to_string(),
            ]
        );
    }

    #[test]
    fn autodiscover_ignores_pages_without_feeds() {
        assert!(autodiscover_links(
            "<html><head><link rel=stylesheet href=/a.css></head></html>",
            "https://x.vn"
        )
        .is_empty());
    }

    #[test]
    fn scan_feed_hrefs_finds_links_on_a_directory_page() {
        let html = r#"<html><body>
            <a href="/rss/tin-moi-nhat.rss">Tin mới nhất</a>
            <a href="/rss/the-thao.rss">Thể thao</a>
            <a href="https://other.vn/feed/main">Khác</a>
            <a href="/gioi-thieu.html">Giới thiệu</a>
            <a href="javascript:void(0)">JS</a>
        </body></html>"#;
        let links = scan_feed_hrefs(html, "https://vnexpress.net/rss");
        assert_eq!(
            links,
            vec![
                "https://vnexpress.net/rss/tin-moi-nhat.rss".to_string(),
                "https://vnexpress.net/rss/the-thao.rss".to_string(),
                "https://other.vn/feed/main".to_string(),
            ]
        );
    }

    #[test]
    fn common_feed_paths_only_for_site_roots() {
        assert_eq!(common_feed_paths("https://x.vn").len(), 5);
        assert!(common_feed_paths("https://x.vn/some/article.html").is_empty());
        assert!(common_feed_paths("not a url").is_empty());
    }

    #[test]
    fn clip_is_multibyte_safe() {
        let s = "một hai ba bốn năm";
        let c = clip(s, 7);
        assert!(c.starts_with("một hai"));
        assert!(c.ends_with('…'));
    }
}
