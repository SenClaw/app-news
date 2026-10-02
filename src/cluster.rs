//! Text utilities + rule-based intelligence of the News app:
//!   * Vietnamese-aware normalization / tokenization (diacritics KEPT — "giá"
//!     vs "gia" are different words; FTS handles diacritic-less search),
//!   * incremental story clustering — a new article joins the best-matching
//!     recent story or starts a new one (chuỗi liên kết tin tức),
//!   * trend detection — n-gram doc-frequency spike between two windows.
//!
//! Everything here is deterministic; the AI (crate::llm) only NARRATES what
//! these functions computed, it never decides cluster membership.

use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

/// Vietnamese + English stopwords for title tokens. Deliberately small: over-
/// stripping hurts clustering more than leaving a few function words in.
const STOPWORDS: &[&str] = &[
    // Markup leftovers. A feed that double-encodes ("&amp;apos;" → "&apos;")
    // leaves the entity NAME in the text; without this it becomes a "word" and,
    // being present in every such headline, one of the strongest tokens a story
    // profile ever sees.
    "apos", "quot", "amp", "nbsp", "hellip", "ndash", "mdash", "lsquo", "rsquo", "ldquo", "rdquo",
    "gt", "lt", // Vietnamese
    "anh", "au", "bà", "bài", "bị", "bởi", "các", "cách", "cái", "cho", "chưa", "chỉ", "cô", "có",
    "cùng", "cũng", "của", "cứ", "do", "dù", "đã", "đang", "đây", "để", "đến", "được", "gì", "hai",
    "hay", "hơn", "khi", "không", "là", "làm", "lại", "lên", "loạt", "luôn", "mà", "mới", "một",
    "muốn", "này", "nên", "nếu", "ngày", "người", "nhất", "nhiều", "như", "nhưng", "những", "nói",
    "ông", "phải", "qua", "ra", "rằng", "rất", "rồi", "sau", "sao", "sẽ", "so", "sự", "tại",
    "theo", "thể", "thì", "trên", "trong", "trước", "từ", "và", "vào", "vẫn", "về", "vì", "việc",
    "với", "vừa", // English
    "a", "an", "and", "are", "as", "at", "be", "but", "by", "for", "from", "has", "have", "he",
    "her", "his", "how", "in", "is", "it", "its", "may", "more", "new", "not", "of", "on", "or",
    "over", "say", "says", "she", "that", "the", "their", "they", "this", "to", "up", "was",
    "were", "what", "when", "who", "why", "will", "with", "you",
];

/// Lowercase, strip punctuation to spaces, collapse whitespace. Diacritics kept.
pub fn normalize(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        let c = c.to_lowercase().next().unwrap_or(c);
        if c.is_alphanumeric() {
            out.push(c);
        } else {
            out.push(' ');
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn is_stopword(t: &str) -> bool {
    // Linear scan — the table is ~120 entries and titles are short. NOT
    // binary_search: Vietnamese doesn't sort the way the list reads.
    STOPWORDS.contains(&t) || t.chars().count() < 2 || t.chars().all(|c| c.is_numeric())
}

/// Content tokens of a title: normalized words minus stopwords / bare numbers.
pub fn tokens(s: &str) -> Vec<String> {
    normalize(s)
        .split_whitespace()
        .filter(|t| !is_stopword(t))
        .map(String::from)
        .collect()
}

// ---------------------------------------------------------------------------
// Story clustering
// ---------------------------------------------------------------------------

/// Fingerprint of one headline: its BIGRAMS.
///
/// Not single tokens. A token here is one Vietnamese SYLLABLE — "thế", "giới",
/// "công", "nam", "động" carry no event identity on their own and are shared by
/// unrelated news all day long. Two-syllable phrases ("động đất", "aeon mall",
/// "giá vàng") are the real lexical unit, and English headlines fingerprint just
/// as well on word pairs. The story graph has always compared phrases for
/// exactly this reason; clustering now uses the same unit.
///
/// The two words must be ADJACENT in the headline. Pairing across a dropped
/// word invents phrases the headline never contained — "giá vàng TRONG NƯỚC lập
/// đỉnh" would yield "vàng nước" — which both pads the fingerprint and matches
/// text nobody wrote.
pub fn key_phrases(title: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let mut prev: Option<&str> = None;
    let norm = normalize(title);
    for w in norm.split_whitespace() {
        if is_stopword(w) {
            prev = None; // a gap: whatever follows is not adjacent to what came before
            continue;
        }
        if let Some(p) = prev {
            out.insert(format!("{p} {w}"));
        }
        prev = Some(w);
    }
    out
}

/// Headlines that are a PAGE, not an event: roundups, hourly bulletins, "24h
/// qua" digests. Their text is a table of contents of everything that happened,
/// so it overlaps with every event at once — one of these seeding or joining a
/// story is how a timeline fills up with articles that share nothing.
///
/// These articles are still stored, searched and listed; they are only kept out
/// of the event streams.
pub fn is_digest_title(title: &str) -> bool {
    // Structural test, true in any language: two full headlines glued with a
    // separator is a bulletin listing several stories ("Điểm tin 6h: TP.HCM … |
    // VN-Index giành lại mốc 1.700 điểm").
    if title
        .split(['|', '؛'])
        .filter(|p| p.split_whitespace().count() >= 5)
        .count()
        >= 2
    {
        return true;
    }
    let n = normalize(title);
    let markers = digest_markers();
    markers.iter().any(|m| n.starts_with(m.as_str()) || n.contains(m.as_str()))
}

/// Section titles a publisher reuses for its roundups. Vietnamese by default
/// because that is what the shipped source list reads, but this is DATA, not a
/// rule: `news.digest_markers` in settings replaces it wholesale, so a German
/// or Japanese feed configures its own ("tagesüberblick", "まとめ") without a
/// code change.
pub const DEFAULT_DIGEST_MARKERS: &[&str] = &[
    "điểm tin",
    "bản tin",
    "tin tức thế giới",
    "tin thế giới",
    "tin trong nước",
    "tin sáng",
    "tin trưa",
    "tin chiều",
    "tin tối",
    "tin vắn",
    "tin nóng",
    "toàn cảnh",
    "nhịp sống",
    "chuyển động 24h",
    "thời sự 24h",
    "điểm nóng",
    "tổng hợp tin",
    "sự kiện nổi bật",
    "24h qua",
    "trong 24h",
    "tin tức 24h",
];

static DIGEST_MARKERS: std::sync::RwLock<Option<Vec<String>>> = std::sync::RwLock::new(None);

/// Install the marker list read from settings. Called once at boot and again
/// whenever the setting changes; everything downstream reads it through
/// [`is_digest_title`], which is why this is process state rather than a
/// parameter threaded through every call site.
pub fn set_digest_markers(markers: Vec<String>) {
    let cleaned: Vec<String> = markers
        .into_iter()
        .map(|m| normalize(&m))
        .filter(|m| !m.is_empty())
        .collect();
    if let Ok(mut g) = DIGEST_MARKERS.write() {
        *g = Some(cleaned);
    }
}

fn digest_markers() -> Vec<String> {
    match DIGEST_MARKERS.read() {
        Ok(g) => match g.as_ref() {
            Some(v) => v.clone(),
            None => DEFAULT_DIGEST_MARKERS.iter().map(|s| s.to_string()).collect(),
        },
        Err(_) => DEFAULT_DIGEST_MARKERS.iter().map(|s| s.to_string()).collect(),
    }
}

/// How often each phrase occurs across the WHOLE archive.
///
/// This is what replaces a hand-written stopword list. A curated list only ever
/// covers the languages someone wrote it for; document frequency is measured
/// from the sources actually being read, so a Vietnamese, English, Japanese or
/// Spanish feed each gets its own everyday-phrase suppression for free, with no
/// per-language table to maintain.
#[derive(Default)]
pub struct Corpus {
    pub total: u32,
    pub df: HashMap<String, u32>,
}

impl Corpus {
    /// Count one headline in. Building a corpus is the same operation whether
    /// it happens over a live feed or a test fixture.
    pub fn add(&mut self, title: &str) {
        self.total += 1;
        for p in key_phrases(title) {
            *self.df.entry(p).or_insert(0) += 1;
        }
    }

    /// Is this phrase everyday background language rather than an identifier?
    ///
    /// "Common" = present in more than 1% of everything ever indexed. Which
    /// phrases those are is never written down anywhere: for a Vietnamese feed
    /// it lands on "việt nam"/"hôm nay", for an English one on "earnings
    /// call"/"call summary", and for a language nobody anticipated it still
    /// lands on that language's filler.
    ///
    /// The 1% line is measured, not guessed. On a real 20k-headline archive the
    /// recurring TEMPLATES a wire keeps reusing sit above it ("earnings call"
    /// 1.6%, "call summary" 1.1%, "hôm nay" 1.1%) while the vocabulary of actual
    /// events sits below ("liệt sĩ" 0.9%, "giá vàng" 0.6%, "nhật bản" 0.6%,
    /// "động đất" 0.5%) — which is the difference between a hundred companies
    /// filing the same quarterly report and a hundred outlets covering one
    /// earthquake. The `df >= 5` floor keeps a fresh install — where every
    /// phrase is a large share of a tiny archive — from declaring the whole
    /// vocabulary generic.
    pub fn is_common(&self, phrase: &str) -> bool {
        let df = self.df.get(phrase).copied().unwrap_or(0);
        df >= 5 && df as u64 * 100 > self.total as u64
    }
}

// ---------------------------------------------------------------------------
// Facts of an article: WHO / WHERE / HOW MUCH
// ---------------------------------------------------------------------------

/// The concrete things a report names, pulled out of the headline and lead.
///
/// Phrase overlap answers "are these about the same KIND of thing"; it cannot
/// answer "are these the same INCIDENT". Two earthquakes a week apart share
/// every phrase that matters — "động đất", "trận động", "rung chuyển" — and the
/// only things that separate them are the place and the magnitude. So those get
/// extracted and used to VETO a merge that phrase overlap would otherwise wave
/// through.
#[derive(Debug, Default, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Facts {
    /// Proper nouns: places, organisations, people ("nhật bản", "napoli").
    #[serde(default)]
    pub names: BTreeSet<String>,
    /// `(unit, value)` pairs read off the text — ("độ", "7,1"), ("người", "34").
    #[serde(default)]
    pub measures: BTreeSet<(String, String)>,
}

/// Capitalised runs = proper nouns, in any Latin-script language.
///
/// The first word of a sentence is skipped: it is capitalised by grammar, not
/// because it names anything ("Động đất mạnh 4,7 độ … tại Ý" must yield "ý",
/// not "động đất").
pub fn proper_names(text: &str) -> BTreeSet<String> {
    let words: Vec<String> = text
        .split_whitespace()
        .map(|w| w.trim_matches(|c: char| !c.is_alphanumeric()).to_string())
        .collect();
    let ends = |i: usize| {
        text.split_whitespace()
            .nth(i)
            .map(|w| w.ends_with(['.', '!', '?', ':', ';']))
            .unwrap_or(false)
    };
    let capital = |w: &str| {
        w.chars()
            .next()
            .map(|c| c.is_alphabetic() && c.to_lowercase().next() != Some(c))
            .unwrap_or(false)
    };
    // ALL-CAPS is shouting or an acronym, not a reliable name.
    let shouting = |w: &str| {
        w.chars().filter(|c| c.is_alphabetic()).count() > 1
            && w.chars().filter(|c| c.is_alphabetic()).all(|c| c.is_uppercase())
    };

    let mut out = BTreeSet::new();
    let mut run: Vec<String> = Vec::new();
    let mut run_started_sentence = false;
    // Flush a finished run. When the run OPENED a sentence its first word may
    // be a common noun that grammar capitalised ("Tuyển Việt Nam thắng…"), so
    // the reading without it is recorded too — otherwise this headline's
    // "tuyển việt nam" would never match another's "việt nam".
    let mut flush = |run: &mut Vec<String>, led: bool| {
        if run.is_empty() {
            return;
        }
        out.insert(run.join(" "));
        if led && run.len() >= 3 {
            out.insert(run[1..].join(" "));
        }
        run.clear();
    };
    let mut sentence_start = true;
    for i in 0..words.len() {
        let w = &words[i];
        if w.is_empty() {
            sentence_start = true;
            continue;
        }
        // A capital at the start of a sentence is grammar, not a name — unless
        // the NEXT word is capitalised too, which means the sentence genuinely
        // opens with a multi-word proper noun ("Quảng Ninh sơ tán dân…" must
        // keep "quảng ninh"; "Động đất mạnh 4,7 độ…" must not keep "động").
        let leads_a_name = sentence_start
            && words
                .get(i + 1)
                .map(|n| capital(n) && !shouting(n))
                .unwrap_or(false);
        if capital(w) && !shouting(w) && (!sentence_start || leads_a_name) {
            if run.is_empty() {
                run_started_sentence = sentence_start;
            }
            run.push(w.to_lowercase());
        } else {
            flush(&mut run, run_started_sentence);
        }
        sentence_start = ends(i);
    }
    flush(&mut run, run_started_sentence);
    out
}

/// `(unit, value)` for every "number + word" in the text.
///
/// Language-agnostic on purpose: whatever word follows the number is treated as
/// its unit, so "4,7 độ", "34 người" and "40 years" all parse without a table of
/// units per language.
pub fn measures(text: &str) -> BTreeSet<(String, String)> {
    let mut out = BTreeSet::new();
    let words: Vec<&str> = text.split_whitespace().collect();
    for i in 0..words.len() {
        let num: String = words[i]
            .trim_matches(|c: char| !c.is_alphanumeric())
            .to_string();
        // A number, possibly with , or . as separators — "4,7", "150.000", "34".
        if num.is_empty()
            || !num.chars().next().unwrap().is_ascii_digit()
            || !num.chars().all(|c| c.is_ascii_digit() || c == ',' || c == '.')
        {
            continue;
        }
        let Some(next) = words.get(i + 1) else { continue };
        let unit: String = next
            .trim_matches(|c: char| !c.is_alphanumeric())
            .to_lowercase();
        if unit.is_empty() || unit.chars().next().unwrap().is_ascii_digit() {
            continue;
        }
        // "4.7" and "4,7" are the same number written two ways.
        out.insert((unit, num.replace(',', ".")));
    }
    out
}

/// Facts are read from the HEADLINE, not the lead paragraph.
///
/// Measured on the real archive, and it is not a close call. The lead of
/// "Italia xảy ra động đất mạnh nhất trong 40 năm qua tại Napoli" walks through
/// recent quakes for context and names Kamchatka, Myanmar, Đài Loan, Trung Quốc
/// and NHẬT BẢN — so a test that trusts the lead concludes this Italian
/// earthquake is the Japanese one, which is exactly the merge being fixed. The
/// headline is the one sentence that states what THIS report is about, and
/// numbers in a lead wander the same way names do.
pub fn facts_of(title: &str) -> Facts {
    Facts {
        names: proper_names(title),
        measures: measures(title),
    }
}

/// What a story has established: every name its articles named, and the
/// measures stated in the headline it was OPENED with.
#[derive(Debug, Default, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct StoryFacts {
    #[serde(default)]
    pub names: BTreeSet<String>,
    /// From the seed headline, and never updated. This is the identity of the
    /// event ("7,1 độ"), not a running tally — a death toll legitimately climbs
    /// from 13 to 34 over a week and must NOT read as a contradiction, while a
    /// second quake's "4,7 độ" must.
    #[serde(default)]
    pub seed_measures: BTreeSet<(String, String)>,
}

impl StoryFacts {
    pub fn seed(f: &Facts) -> Self {
        Self {
            names: f.names.clone(),
            seed_measures: f.measures.clone(),
        }
    }

    pub fn absorb(&mut self, f: &Facts) {
        for n in &f.names {
            self.names.insert(n.clone());
        }
        // Keep it bounded: a long-running story shouldn't accumulate an
        // ever-wider net of names that matches everything.
        if self.names.len() > 40 {
            let keep: BTreeSet<String> = self.names.iter().take(40).cloned().collect();
            self.names = keep;
        }
    }
}

/// Do these facts CONTRADICT the story's? `true` = different incident.
///
/// Deliberately one-sided: silence is never a contradiction. An article that
/// names nothing, or a story that has established nothing, can still merge on
/// phrases alone. Only positive disagreement blocks.
pub fn facts_conflict(article: &Facts, story: &StoryFacts, corpus: &Corpus) -> bool {
    // 1. The headline restates a quantity the story was opened with, differently.
    for (unit, value) in &article.measures {
        if let Some((_, seed)) = story.seed_measures.iter().find(|(u, _)| u == unit) {
            if seed != value {
                return true;
            }
        }
    }
    // 2. Both name specific places/people, and they have none in common.
    names_conflict(&article.names, &story.names, corpus)
}

/// Two sets of proper names that positively disagree: both name something
/// specific, and they name nothing in common. Silence is never a contradiction.
///
/// Everyday names are excluded first: half the archive says "việt nam", so
/// sharing it is not evidence of being the same incident, and it must not be
/// allowed to satisfy the test either.
pub fn names_conflict(a: &BTreeSet<String>, b: &BTreeSet<String>, corpus: &Corpus) -> bool {
    fn specific<'a>(set: &'a BTreeSet<String>, corpus: &Corpus) -> BTreeSet<&'a str> {
        set.iter()
            .filter(|n| !corpus.is_common(n))
            .map(|n| n.as_str())
            .collect()
    }
    let mine = specific(a, corpus);
    let theirs = specific(b, corpus);
    if mine.is_empty() || theirs.is_empty() {
        return false;
    }
    mine.is_disjoint(&theirs)
}

/// A candidate story to match a new article against: its phrase profile (how
/// many of the story's articles used each phrase) plus the span it covers.
pub struct StoryProfile {
    pub story_id: i64,
    pub first_at: i64,
    pub last_at: i64,
    pub article_count: u32,
    pub profile: HashMap<String, u32>,
    /// Places, people and headline quantities established so far — used to veto
    /// a phrase match that is really a different incident.
    pub facts: StoryFacts,
}

/// How far after a story's latest article a new one may still join it. An event
/// that has been silent for this long is over; a headline arriving later is a
/// new development, not the same thread.
pub const STORY_JOIN_GAP: i64 = 3 * 86400;

/// Hard ceiling on how long one story may span. Anything recurring for longer
/// ("giá vàng hôm nay", published daily forever) is a TOPIC, not an event — the
/// app has a separate topics feature for that. Without this ceiling the daily
/// instalment chains to the previous one and a single "story" grows for months.
pub const STORY_MAX_SPAN: i64 = 14 * 86400;

/// Serialize a profile map to the JSON stored in `stories.profile`.
pub fn profile_to_json(p: &HashMap<String, u32>) -> String {
    let m: BTreeMap<_, _> = p.iter().collect();
    serde_json::to_string(&m).unwrap_or_else(|_| "{}".into())
}

/// Parse `stories.profile` JSON back into a map (empty on garbage).
pub fn profile_from_json(s: &str) -> HashMap<String, u32> {
    serde_json::from_str::<HashMap<String, u32>>(s).unwrap_or_default()
}

/// Merge an article's phrases into a story profile, capping profile size so a
/// long-running story doesn't grow an ever-looser net that swallows everything.
pub fn profile_merge(profile: &mut HashMap<String, u32>, title: &str) {
    for p in key_phrases(title) {
        *profile.entry(p).or_insert(0) += 1;
    }
    if profile.len() > 80 {
        // Keep the 80 most frequent phrases (ties: lexicographic, deterministic).
        let mut entries: Vec<(String, u32)> = profile.drain().collect();
        entries.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        entries.truncate(80);
        profile.extend(entries);
    }
}

/// Can an article published at `ts` still join a story covering `first_at..last_at`?
///
/// Guards the two ways a story degenerates into a bucket: joining a thread that
/// already went quiet, and stretching one thread across months.
pub fn span_allows(ts: i64, first_at: i64, last_at: i64) -> bool {
    if ts > last_at + STORY_JOIN_GAP || ts < first_at - STORY_JOIN_GAP {
        return false;
    }
    last_at.max(ts) - first_at.min(ts) <= STORY_MAX_SPAN
}

/// A shared phrase must cover this much of the article's fingerprint before a
/// handful of overlaps counts as "the same event"…
const MIN_COVERAGE: f64 = 0.35;
/// …unless this many phrases are shared outright, which long headlines reach
/// without ever hitting the coverage ratio.
const STRONG_SHARED: usize = 4;
/// Below this many articles, "used by a third of the story" says nothing.
const MIN_ARTICLES_FOR_ANCHORING: u32 = 3;

/// Score an article's phrases against one story. `None` = not the same event.
///
/// Three conditions, and the second is the one that stops runaway buckets:
///
///  1. at least 2 shared phrases, and the overlap either covers a third of the
///     article's fingerprint or is large outright. An article that merely
///     mentions the story's subject in passing ("Trung Quốc gom dự trữ khi giá
///     vàng lập đỉnh") shares a phrase or two while most of what it is about is
///     elsewhere — that is a related thread for the graph, not a merge;
///  2. at least one shared phrase is CORE to the story — used by a third of its
///     articles. A story that has swallowed 150 headlines has a profile full of
///     phrases seen once or twice; none of them is core, so nothing new can join
///     and the bucket stops growing. A real running event ("động đất nhật bản")
///     repeats its phrases in most of its articles, so it keeps growing normally;
///  3. that core phrase must also be SPECIFIC — not everyday language for this
///     archive. "việt nam" is core to half the Vietnamese stories ever created
///     and identifies none of them.
pub fn score_match(
    keys: &BTreeSet<String>,
    profile: &HashMap<String, u32>,
    article_count: u32,
    corpus: &Corpus,
) -> Option<(f64, usize)> {
    // Everyday language is dropped before anything is compared. That is what
    // makes this work in a language the code knows nothing about: whatever that
    // language uses to glue a sentence together turns up in most of its
    // headlines, so the archive's own statistics recognise it without anyone
    // writing it down.
    let mine: Vec<&str> = keys
        .iter()
        .filter(|p| !corpus.is_common(p))
        .map(|p| p.as_str())
        .collect();
    if mine.len() < 2 {
        return None;
    }

    let mut shared = 0usize;
    let mut anchors = 0usize; // shared AND core to the story
    let n = article_count.max(1);
    for p in &mine {
        if let Some(&c) = profile.get(*p) {
            shared += 1;
            if c * 3 >= n {
                anchors += 1;
            }
        }
    }
    if shared < 2 || anchors == 0 {
        return None;
    }
    let coverage = shared as f64 / mine.len() as f64;
    // Two phrases that a third of the story's articles keep using is the story's
    // own vocabulary, and a long headline can carry that without ever reaching
    // the coverage ratio. Only stories with enough articles qualify: below that,
    // "a third of the articles" is true of every phrase they contain and would
    // wave through anything that mentions the subject in passing.
    let anchored = anchors >= 2 && article_count >= MIN_ARTICLES_FOR_ANCHORING;
    if coverage < MIN_COVERAGE && shared < STRONG_SHARED && !anchored {
        return None;
    }
    Some((coverage, shared))
}

/// Pick the story a new article belongs to, or `None` → start a new story.
pub fn assign_story(
    title: &str,
    published_at: i64,
    candidates: &[StoryProfile],
    corpus: &Corpus,
) -> Option<i64> {
    if is_digest_title(title) {
        return None; // a bulletin page is not an event
    }
    let keys = key_phrases(title);
    if keys.len() < 2 {
        return None; // too little signal — never cluster on a 3-word headline
    }
    let facts = facts_of(title);
    let mut best: Option<(f64, usize, i64)> = None;
    for c in candidates {
        if !span_allows(published_at, c.first_at, c.last_at) {
            continue;
        }
        if facts_conflict(&facts, &c.facts, corpus) {
            continue; // same subject, different incident
        }
        let Some((score, shared)) = score_match(&keys, &c.profile, c.article_count, corpus) else {
            continue;
        };
        if better_than(&best, score, shared) {
            best = Some((score, shared, c.story_id));
        }
    }
    best.map(|(_, _, id)| id)
}

/// Tie-break rule shared by live ingest and the full-archive rebuild, so both
/// resolve a contested article to the same story.
pub fn better_than(best: &Option<(f64, usize, i64)>, score: f64, shared: usize) -> bool {
    match best {
        None => true,
        Some((bs, bsh, _)) => score > *bs || (score == *bs && shared > *bsh),
    }
}

// ---------------------------------------------------------------------------
// Story graph (liên kết giữa các dòng sự kiện)
// ---------------------------------------------------------------------------

/// A story reduced to the PHRASES its headlines use.
///
/// Linking on single tokens does not work in Vietnamese: a token here is one
/// syllable, so "thế"/"giới"/"mạnh" match across completely unrelated events.
/// Two-syllable phrases ("giá vàng", "không kích", "nửa đầu") are the real
/// lexical unit, so the graph compares those instead.
pub struct StoryPhrases {
    pub story_id: i64,
    pub phrases: BTreeSet<String>,
}

/// Bigrams of a story's headlines — its phrase fingerprint.
pub fn phrases_of(titles: &[String]) -> BTreeSet<String> {
    titles.iter().flat_map(|t| key_phrases(t)).collect()
}

/// Phrases too common across the corpus to identify a thread ("thế giới",
/// "nửa đầu", "hôm nay"). Derived from the corpus, not a hand-written list:
/// a phrase in more than a fifth of the stories (min 2) links nothing.
pub fn generic_phrases(stories: &[StoryPhrases]) -> BTreeSet<String> {
    let n = stories.len();
    if n < 4 {
        return BTreeSet::new();
    }
    let cutoff = (n / 5).max(2);
    let mut df: HashMap<&str, usize> = HashMap::new();
    for s in stories {
        for p in &s.phrases {
            *df.entry(p.as_str()).or_insert(0) += 1;
        }
    }
    df.into_iter()
        .filter(|(_, c)| *c > cutoff)
        .map(|(p, _)| p.to_string())
        .collect()
}

/// Filler for THIS map: phrases common among the stories on screen, plus
/// phrases that are everyday language across the whole archive.
///
/// The second half matters as much as the first. A week's worth of stories is a
/// small sample — a phrase can be rare among the 60 nodes being drawn and still
/// be something every Vietnamese headline says. Judging it on the archive is
/// what stops the map wiring unrelated events together on filler.
pub fn map_filler(stories: &[StoryPhrases], corpus: &Corpus) -> BTreeSet<String> {
    let mut out = generic_phrases(stories);
    for s in stories {
        for p in &s.phrases {
            if corpus.is_common(p) {
                out.insert(p.clone());
            }
        }
    }
    out
}

/// Overlap between two phrase fingerprints, ignoring corpus-wide filler.
/// Returns (score, shared phrases strongest-first).
pub fn story_similarity(
    a: &BTreeSet<String>,
    b: &BTreeSet<String>,
    generic: &BTreeSet<String>,
) -> (f64, Vec<String>) {
    let ka: BTreeSet<&String> = a.iter().filter(|p| !generic.contains(p.as_str())).collect();
    let kb: BTreeSet<&String> = b.iter().filter(|p| !generic.contains(p.as_str())).collect();
    if ka.is_empty() || kb.is_empty() {
        return (0.0, Vec::new());
    }
    let shared: Vec<String> = ka.intersection(&kb).map(|p| (*p).clone()).collect();
    if shared.is_empty() {
        return (0.0, Vec::new());
    }
    let sim = shared.len() as f64 / ka.len().min(kb.len()) as f64;
    (sim, shared.into_iter().take(5).collect())
}

/// One edge of the story graph.
pub struct StoryLink {
    pub a: i64,
    pub b: i64,
    pub weight: f64,
    pub shared: Vec<String>,
}

/// Pairs of stories sharing real phrase overlap. Looser than the clustering
/// threshold — the graph shows "cùng mạch chuyện" links between events that
/// remain separate stories ("giá vàng lập đỉnh" ↔ "trung quốc gom vàng"),
/// which is exactly what clustering must not merge.
///
/// A single shared phrase is only enough when it dominates both fingerprints
/// (≥20%); otherwise two stories need at least two phrases in common. Without
/// that, one incidental phrase ("nhân viên", "thế nào") wires unrelated events
/// together and the map turns into noise.
pub fn story_links(stories: &[StoryPhrases], corpus: &Corpus) -> Vec<StoryLink> {
    let generic = map_filler(stories, corpus);
    let mut links = Vec::new();
    for i in 0..stories.len() {
        for j in (i + 1)..stories.len() {
            let (sim, shared) =
                story_similarity(&stories[i].phrases, &stories[j].phrases, &generic);
            if shared.len() >= 2 || (shared.len() == 1 && sim >= 0.2) {
                links.push(StoryLink {
                    a: stories[i].story_id,
                    b: stories[j].story_id,
                    weight: (sim * 100.0).round() / 100.0,
                    shared,
                });
            }
        }
    }
    links.sort_by(|x, y| {
        y.weight
            .partial_cmp(&x.weight)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(y.shared.len().cmp(&x.shared.len()))
    });
    links
}

/// Keep only each story's strongest links, dropping the rest.
///
/// Every pair above the threshold is a *true* statement, but drawing all of them
/// is not a map — 60 stories produced 467 lines, an average of 15 per node, and
/// at that density the layout collapses into one ball and no line can be
/// followed by eye. Keeping each story's `k` strongest neighbours leaves the
/// skeleton: the strongest link of every node survives (an edge is kept if
/// EITHER endpoint ranks it in its top k), so nothing is orphaned by pruning.
///
/// Returns `(kept, dropped)` — the count of dropped links is shown to the
/// reader rather than silently swallowed.
pub fn prune_links(links: Vec<StoryLink>, k: usize) -> (Vec<StoryLink>, usize) {
    if k == 0 {
        return (links, 0);
    }
    let mut rank: HashMap<i64, usize> = HashMap::new();
    let mut kept = Vec::with_capacity(links.len().min(k * 8));
    let mut dropped = 0usize;
    // `links` arrives strongest-first, so the first k an endpoint sees are its
    // best ones.
    for l in links {
        let ca = rank.get(&l.a).copied().unwrap_or(0);
        let cb = rank.get(&l.b).copied().unwrap_or(0);
        if ca < k || cb < k {
            *rank.entry(l.a).or_insert(0) += 1;
            *rank.entry(l.b).or_insert(0) += 1;
            kept.push(l);
        } else {
            dropped += 1;
        }
    }
    (kept, dropped)
}

// ---------------------------------------------------------------------------
// Trend detection
// ---------------------------------------------------------------------------

/// Seed phrases of a headline: its BIGRAMS and TRIGRAMS.
///
/// Not single tokens. A token here is one Vietnamese SYLLABLE — "phú", "bão",
/// "nghỉ", "bước", "lễ" name nothing on their own and are shared by unrelated
/// news all day long, and an English headline's lone words are no better as an
/// event name ("island", "grand", "us"). Word PAIRS are the smallest unit that
/// identifies an event, which is the same call `key_phrases` already makes for
/// clustering.
///
/// This is what a hand-written list of banned syllables used to approximate.
/// A one-word name an event really does go by — "cyprus", "niger", "manhunt" —
/// is not lost: it comes back in `label_event`, which measures words against
/// the event's OWN headlines instead of against the whole feed.
fn trend_phrases(title: &str) -> BTreeSet<String> {
    let toks = tokens(title);
    let mut out = BTreeSet::new();
    for w in toks.windows(2) {
        out.insert(w.join(" "));
    }
    for w in toks.windows(3) {
        out.insert(w.join(" "));
    }
    out
}

/// How many top-scoring phrases get folded into events. The tab only ever shows
/// `limit` cards, but scanning well past them is what lets the 20th-ranked way
/// of saying "the Cyprus ferry" be absorbed into that event instead of taking a
/// card of its own.
const TREND_SCAN: usize = 400;

/// Two phrases are the same event when this much of their combined article set
/// is shared (Jaccard). Measured on a live 5k-article window: the three phrases
/// for one ferry sinking sit at 0.45–0.56, unrelated events that merely share a
/// country word sit below 0.2.
const TREND_MERGE_JACCARD: f64 = 0.40;

/// …or when the candidate is mostly inside an event already on the board, which
/// is what a sub-plot looks like ("ferry carrying" inside the sinking).
const TREND_INSIDE: f64 = 0.60;

/// A candidate that swallows an event whole while being much bigger than it is
/// an umbrella word, not a trend: "phú" covers the Phú Thọ explosion AND a
/// walking event AND a court case. Dropped rather than merged, so it cannot
/// inflate a real event with articles that have nothing to do with it.
const TREND_UMBRELLA: f64 = 0.85;

/// Longest label allowed, in words.
const TREND_LABEL_WORDS: usize = 4;

/// A longer label must still name at least this share of what the shorter one
/// named. Loosening it produces run-ons ("iranian launchers iran larak island"),
/// tightening it leaves fragments ("phú" for Phú Thọ).
const TREND_LABEL_KEEP: f64 = 0.60;

/// Keep an event's name set bounded, the way a story's is: an ever-wider net of
/// names eventually matches everything and the veto stops vetoing. Trimming in
/// set order (not arrival order) keeps the result the same on every run.
fn cap_names(mut names: BTreeSet<String>) -> BTreeSet<String> {
    if names.len() > 40 {
        names = names.into_iter().take(40).collect();
    }
    names
}

/// One trending event: how many articles are about it now vs the prior window.
#[derive(Debug, Clone)]
pub struct Trend {
    pub phrase: String,
    pub count: u32,
    pub prev_count: u32,
    pub score: f64,
    pub article_ids: Vec<i64>,
}

/// A phrase in many articles now that was rare before scores highest; steady
/// background phrases score low.
fn trend_score(count: u32, prev_count: u32) -> f64 {
    count as f64 * (count as f64 + 1.0) / (prev_count as f64 + 1.0)
}

/// Name an event from the headlines it actually contains.
///
/// Start at the phrase naming the most of them, then keep extending it with an
/// adjacent word while the longer phrase still names `TREND_LABEL_KEEP` as many:
/// "island" → "larak island", "canyon" → "grand canyon", "nghỉ" → "nghỉ lễ".
///
/// Frequency is counted over THIS event only, never over the feed, so a word
/// that is everyday language everywhere else is still allowed to name the one
/// story that is genuinely about it. The result is always a phrase somebody
/// wrote, in whatever language they wrote it.
fn label_event(ids: &[i64], title_of: &HashMap<i64, &str>) -> String {
    let mut df: BTreeMap<String, u32> = BTreeMap::new();
    for id in ids {
        let Some(title) = title_of.get(id) else {
            continue;
        };
        let toks = tokens(title);
        let mut seen = BTreeSet::new();
        for n in 1..=TREND_LABEL_WORDS {
            for w in toks.windows(n) {
                seen.insert(w.join(" "));
            }
        }
        for g in seen {
            *df.entry(g).or_insert(0) += 1;
        }
    }
    // Ties resolve to the first phrase in sort order, so the label of an
    // unchanged window never flickers between two equally-supported phrases.
    let widest = |words: usize, df: &BTreeMap<String, u32>| -> Option<(String, u32)> {
        df.iter()
            .filter(|(g, _)| g.split(' ').count() == words)
            .max_by_key(|(g, n)| (**n, std::cmp::Reverse((*g).clone())))
            .map(|(g, n)| (g.clone(), *n))
    };
    // Start at a PHRASE — a lone token names nothing — and fall back to one word
    // only for an event whose headlines are all single-token after stopwords.
    let Some((mut best, mut best_df)) = widest(2, &df).or_else(|| widest(1, &df)) else {
        return String::new();
    };
    while best.split(' ').count() < TREND_LABEL_WORDS {
        let want = best.split(' ').count() + 1;
        let head = format!("{best} ");
        let tail = format!(" {best}");
        let next = df
            .iter()
            .filter(|(g, _)| {
                g.split(' ').count() == want && (g.starts_with(&head) || g.ends_with(&tail))
            })
            .max_by_key(|(g, n)| (**n, std::cmp::Reverse((*g).clone())));
        match next {
            Some((g, n)) if *n as f64 >= TREND_LABEL_KEEP * best_df as f64 => {
                best = g.clone();
                best_df = *n;
            }
            _ => break,
        }
    }
    best
}

/// Detect trending events: phrase doc-frequency in the current window scored
/// against the previous window, then folded so each EVENT gets one card.
///
/// `current` / `previous` are `(article_id, title)` rows. `background` is asked
/// once, for the top candidates only, how often each phrase occurs across the
/// whole archive — everyday language ("việt nam", "hà nội", "tỷ đồng") is not a
/// trend no matter how many articles carry it today, and which phrases those
/// are is measured from the feed rather than written down in a list.
pub fn detect_trends(
    current: &[(i64, String)],
    previous: &[(i64, String)],
    min_count: u32,
    limit: usize,
    background: &dyn Fn(&BTreeSet<String>) -> Corpus,
) -> Vec<Trend> {
    let mut cur: HashMap<String, Vec<i64>> = HashMap::new();
    for (id, title) in current {
        for g in trend_phrases(title) {
            cur.entry(g).or_default().push(*id);
        }
    }
    let mut prev: HashMap<String, u32> = HashMap::new();
    for (_, title) in previous {
        for g in trend_phrases(title) {
            *prev.entry(g).or_insert(0) += 1;
        }
    }
    let mut cands: Vec<Trend> = cur
        .into_iter()
        .filter(|(_, ids)| ids.len() as u32 >= min_count)
        .map(|(phrase, ids)| {
            let count = ids.len() as u32;
            let prev_count = *prev.get(&phrase).unwrap_or(&0);
            Trend {
                score: trend_score(count, prev_count),
                phrase,
                count,
                prev_count,
                article_ids: ids,
            }
        })
        .collect();
    cands.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(b.count.cmp(&a.count))
            .then(a.phrase.cmp(&b.phrase))
    });
    cands.truncate(TREND_SCAN);

    let corpus = background(&cands.iter().map(|t| t.phrase.clone()).collect());
    cands.retain(|t| !corpus.is_common(&t.phrase));

    // Overlapping wording alone is not enough to call two articles one event:
    // "vụ nổ" is the Phú Thọ blast AND the Kyiv one, "phà chở" is the Cyprus
    // sinking AND the Cát Bà holiday queue. What separates them is WHERE — the
    // same names test that already vetoes a bad story merge.
    let names_of: HashMap<i64, BTreeSet<String>> = current
        .iter()
        .map(|(id, title)| (*id, proper_names(title)))
        .collect();
    let names_for = |ids: &[i64]| -> BTreeSet<String> {
        let mut out = BTreeSet::new();
        for id in ids {
            if let Some(ns) = names_of.get(id) {
                out.extend(ns.iter().cloned());
            }
        }
        cap_names(out)
    };

    struct Event {
        ids: HashSet<i64>,
        names: BTreeSet<String>,
        prev_count: u32,
    }
    let mut events: Vec<Event> = Vec::new();
    // Fold by ARTICLE OVERLAP, not by spelling. "ferry", "capsizes off" and
    // "cyprus" share no substring and are one sinking; the containment test this
    // replaces could only ever merge a phrase with its own sub-phrase, so the
    // tab showed that one sinking three times.
    'outer: for t in cands {
        let cand: HashSet<i64> = t.article_ids.iter().copied().collect();
        let cand_names = names_for(&t.article_ids);
        for e in events.iter_mut() {
            let inter = cand.intersection(&e.ids).count();
            if inter == 0 {
                continue;
            }
            let union = cand.len() + e.ids.len() - inter;
            let jaccard = inter as f64 / union.max(1) as f64;
            let inside = inter as f64 / cand.len().max(1) as f64;
            if (jaccard >= TREND_MERGE_JACCARD || inside >= TREND_INSIDE)
                && !names_conflict(&cand_names, &e.names, &corpus)
            {
                // Absorb ARTICLE BY ARTICLE, not the whole phrase. "flash
                // flood" is the Grand Canyon one AND the Nepal one; letting the
                // phrase in wholesale files Nepal under Grand Canyon.
                for id in &t.article_ids {
                    let ns = names_of.get(id);
                    if ns.is_none_or(|ns| !names_conflict(ns, &e.names, &corpus)) {
                        e.ids.insert(*id);
                        if let Some(ns) = ns {
                            e.names.extend(ns.iter().cloned());
                        }
                    }
                }
                e.names = cap_names(std::mem::take(&mut e.names));
                e.prev_count = e.prev_count.max(t.prev_count);
                continue 'outer;
            }
            if inter as f64 / cand.len().min(e.ids.len()).max(1) as f64 >= TREND_UMBRELLA {
                continue 'outer; // umbrella word — drop, do not let it merge
            }
        }
        // Seed past `limit`: the event pass below collapses some of these into
        // each other, and the surplus is what keeps the tab full afterwards.
        if events.len() < limit * 2 {
            events.push(Event {
                ids: cand,
                names: cand_names,
                prev_count: t.prev_count,
            });
        }
    }

    // Folding a candidate compares it against the events that exist AT THAT
    // MOMENT, so a candidate skipped early can turn out to belong to an event
    // that only grew afterwards ("iceland rejects" vs "eu accession talks").
    // One more pass, now over whole events, settles those.
    loop {
        let mut merged = None;
        'pairs: for i in 0..events.len() {
            for j in (i + 1)..events.len() {
                let inter = events[i].ids.intersection(&events[j].ids).count();
                if inter == 0 {
                    continue;
                }
                let union = events[i].ids.len() + events[j].ids.len() - inter;
                let smaller = events[i].ids.len().min(events[j].ids.len());
                if (inter as f64 / union.max(1) as f64 >= TREND_MERGE_JACCARD
                    || inter as f64 / smaller.max(1) as f64 >= TREND_INSIDE)
                    && !names_conflict(&events[i].names, &events[j].names, &corpus)
                {
                    merged = Some((i, j));
                    break 'pairs;
                }
            }
        }
        let Some((i, j)) = merged else { break };
        let gone = events.remove(j);
        events[i].ids.extend(gone.ids);
        events[i].names.extend(gone.names);
        events[i].names = cap_names(std::mem::take(&mut events[i].names));
        events[i].prev_count = events[i].prev_count.max(gone.prev_count);
    }

    let title_of: HashMap<i64, &str> =
        current.iter().map(|(id, t)| (*id, t.as_str())).collect();
    let mut out: Vec<Trend> = events
        .into_iter()
        .map(|e| {
            let mut article_ids: Vec<i64> = e.ids.into_iter().collect();
            // Newest first. The card shows the first few as samples, and a
            // trend is read for what is happening now, not for how it opened.
            article_ids.sort_unstable_by(|a, b| b.cmp(a));
            let count = article_ids.len() as u32;
            Trend {
                phrase: label_event(&article_ids, &title_of),
                count,
                prev_count: e.prev_count,
                score: trend_score(count, e.prev_count),
                article_ids,
            }
        })
        .filter(|t| !t.phrase.is_empty())
        .collect();
    out.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(b.count.cmp(&a.count))
            .then(a.phrase.cmp(&b.phrase))
    });
    out.truncate(limit);
    out
}

/// JSON shape shared by REST + MCP for a trend list.
pub fn trends_to_json(trends: &[Trend]) -> Value {
    json!(trends
        .iter()
        .map(|t| {
            json!({
                "phrase": t.phrase,
                "count": t.count,
                "prev_count": t.prev_count,
                "article_ids": t.article_ids,
            })
        })
        .collect::<Vec<_>>())
}

// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh install: nothing indexed yet, so no phrase is background.
    fn no_background(_: &BTreeSet<String>) -> Corpus {
        Corpus::default()
    }

    #[test]
    fn normalize_keeps_diacritics_and_strips_punctuation() {
        assert_eq!(normalize("Giá vàng 'lập đỉnh'!"), "giá vàng lập đỉnh");
        assert_eq!(normalize("  Hello,   WORLD  "), "hello world");
    }

    #[test]
    fn tokens_drop_stopwords_and_numbers() {
        let t = tokens("Giá vàng đã tăng 5 phiên liên tiếp trong tuần");
        assert!(t.contains(&"vàng".to_string()));
        assert!(!t.contains(&"đã".to_string()));
        assert!(!t.contains(&"5".to_string()));
        assert!(!t.contains(&"trong".to_string()));
    }

    const T0: i64 = 1_750_000_000;

    fn story_at(id: i64, ts: i64, titles: &[&str]) -> StoryProfile {
        let mut profile = HashMap::new();
        let mut facts = StoryFacts::seed(&facts_of(titles.first().copied().unwrap_or("")));
        for t in titles {
            profile_merge(&mut profile, t);
            facts.absorb(&facts_of(t));
        }
        StoryProfile {
            story_id: id,
            first_at: ts,
            last_at: ts,
            article_count: titles.len() as u32,
            profile,
            facts,
        }
    }

    fn story(id: i64, titles: &[&str]) -> StoryProfile {
        story_at(id, T0, titles)
    }

    /// A stand-in archive, MEASURED the way production measures: ordinary
    /// headlines counted in, nothing declared filler by hand. Whatever these
    /// sentences keep repeating becomes this corpus's everyday language, which
    /// is the whole point — no phrase list is written anywhere.
    fn everyday() -> Corpus {
        let mut c = Corpus::default();
        let filler = [
            "Việt Nam và thế giới hôm nay có gì mới",
            "Người dân Việt Nam đón chờ tin vui hôm nay",
            "Lao động Việt Nam ra nước ngoài tăng mạnh",
            "Thị trường lao động Việt Nam hôm nay ổn định",
            "Người dân cả nước theo dõi thế giới hôm nay",
            "Việt Nam hôm nay đón đoàn khách quốc tế",
            "Lao động Việt Nam được hỗ trợ học nghề",
            "Người dân Việt Nam quan tâm thế giới việc làm",
        ];
        // Repeat so the everyday phrases clear the >2% / df>=5 bar while the
        // one-off headlines of the tests stay rare, exactly like a real archive.
        for _ in 0..5 {
            for t in filler {
                c.add(t);
            }
        }
        for i in 0..200 {
            c.add(&format!("Chuyện riêng lẻ số {i} không lặp lại lần nào nữa"));
        }
        c
    }

    fn fresh() -> Corpus {
        Corpus::default()
    }

    #[test]
    fn everyday_language_is_measured_not_listed() {
        let c = everyday();
        assert!(c.is_common("việt nam"), "cụm lặp khắp nơi phải bị coi là nền");
        assert!(!c.is_common("động đất"), "cụm chưa từng xuất hiện thì không");
    }

    #[test]
    fn same_event_from_two_sources_clusters() {
        let s = story(7, &["Bão số 3 đổ bộ Quảng Ninh, hàng nghìn hộ dân sơ tán"]);
        let got = assign_story(
            "Quảng Ninh sơ tán dân trước khi bão số 3 đổ bộ",
            T0 + 3600,
            &[s],
            &everyday(),
        );
        assert_eq!(got, Some(7));
    }

    #[test]
    fn unrelated_article_starts_new_story() {
        let s = story(7, &["Bão số 3 đổ bộ Quảng Ninh, hàng nghìn hộ dân sơ tán"]);
        let got = assign_story(
            "Giá vàng lập đỉnh mới, vượt 100 triệu đồng mỗi lượng",
            T0,
            &[s],
            &everyday(),
        );
        assert_eq!(got, None);
    }

    #[test]
    fn short_titles_never_cluster() {
        let s = story(1, &["Bão số 3 đổ bộ"]);
        assert_eq!(assign_story("Bão", T0, &[s], &fresh()), None);
    }

    #[test]
    fn best_of_multiple_stories_wins() {
        let s1 = story(1, &["Đội tuyển Việt Nam thắng Thái Lan tại chung kết AFF Cup"]);
        let s2 = story(2, &["Bão số 3 đổ bộ Quảng Ninh gây mưa lớn"]);
        let got = assign_story(
            "Việt Nam vô địch AFF Cup sau trận chung kết thắng Thái Lan",
            T0,
            &[s1, s2],
            &everyday(),
        );
        assert_eq!(got, Some(1));
    }

    #[test]
    fn everyday_phrases_alone_never_identify_an_event() {
        // "việt nam" là cốt lõi của rất nhiều dòng sự kiện nên không nhận dạng
        // được dòng nào — kể cả khi nó lặp lại trong hồ sơ.
        let s = story(
            5,
            &[
                "Việt Nam và Lào ký hiệp định thương mại mới",
                "Việt Nam đón đoàn doanh nghiệp Lào sang khảo sát",
            ],
        );
        assert_eq!(
            assign_story(
                "Người dân Việt Nam hôm nay đi bầu cử",
                T0,
                std::slice::from_ref(&s),
                &everyday()
            ),
            None
        );
    }

    #[test]
    fn profile_merge_caps_size() {
        let mut p = HashMap::new();
        for i in 0..200 {
            profile_merge(&mut p, &format!("từkhóa{} sựkiện{} diễnbiến{}", i, i, i));
        }
        assert!(p.len() <= 80);
    }

    // -- the bug this module was rewritten for -------------------------------

    #[test]
    fn syllable_overlap_alone_never_joins_a_story() {
        // Đúng ca lỗi thật: dòng "động đất Nhật Bản" đã phình to, hồ sơ của nó
        // đầy âm tiết phổ thông (thế, giới, công, nam, động, hàng…). Bài về
        // chuyện hoàn toàn khác vẫn trùng 5-6 âm tiết như vậy.
        let big = story_at(
            8,
            T0,
            &[
                "Khoảnh khắc vụ nổ ở trung tâm thương mại Aeon Mall sau động đất Nhật Bản",
                "Cụ bà Nhật Bản được lao động Việt Nam cứu sống trong động đất",
                "Chưa ghi nhận thêm trường hợp công dân Việt Nam thương vong do động đất",
                "Nhật Bản khẩn trương cứu hộ sau động đất tại Kumamoto",
            ],
        );
        for off_topic in [
            "Mỹ và UAE thành lập lực lượng đặc nhiệm AI quân sự",
            "115 nhà dân bị nứt do thi công cao tốc Hoài Nhơn - Quy Nhơn",
            "Bí thư Thành ủy đặt hàng các trí thức, nhà khoa học giải quyết điểm nghẽn của Thủ đô",
            "Lao động Việt Nam có cơ hội đi làm việc tại Albania",
        ] {
            assert_eq!(
                assign_story(off_topic, T0 + 3600, std::slice::from_ref(&big), &everyday()),
                None,
                "bài không liên quan vẫn lọt vào dòng sự kiện: {off_topic}"
            );
        }
        // …còn diễn biến thật của chính sự kiện đó thì vẫn phải vào.
        assert_eq!(
            assign_story(
                "Dư chấn động đất Nhật Bản kéo dài bao lâu, có ảnh hưởng đến Việt Nam?",
                T0 + 3600,
                std::slice::from_ref(&big),
                &everyday(),
            ),
            Some(8)
        );
    }

    #[test]
    fn swollen_story_stops_absorbing() {
        // 150 bài, mỗi cụm từ chỉ xuất hiện vài lần → không cụm nào là cốt lõi.
        let mut profile = HashMap::new();
        for i in 0..150 {
            profile_merge(&mut profile, &format!("tin tức số {i} về chuyện gì đó"));
        }
        let bloated = StoryProfile {
            story_id: 9,
            first_at: T0,
            last_at: T0,
            article_count: 150,
            profile,
            facts: StoryFacts::default(),
        };
        assert_eq!(
            assign_story("Chuyện gì đó vừa xảy ra hôm nay", T0, &[bloated], &fresh()),
            None
        );
    }

    /// Ca lỗi thật trên ảnh: động đất Ý 4,7 độ ở Napoli bị nhét vào dòng động
    /// đất Nhật Bản 7,1 độ ở Kumamoto. Cùng từ vựng, khác hẳn sự việc.
    #[test]
    fn a_second_earthquake_is_not_the_same_earthquake() {
        let mut sf = StoryFacts::seed(&facts_of(
            "Động đất 7,1 độ khiến Nhật Bản phát cảnh báo sóng thần",
        ));
        sf.absorb(&facts_of("Nhật Bản nỗ lực khắc phục hậu quả động đất"));
        sf.absorb(&facts_of(
            "Nhật Bản khắc phục tình trạng thiếu nước sau động đất tại Kumamoto",
        ));
        let corpus = everyday();

        for t in [
            "Động đất mạnh 4,7 độ Richter làm rung chuyển miền nam Ý",
            "Italia xảy ra động đất mạnh nhất trong 40 năm qua tại Napoli",
        ] {
            assert!(
                facts_conflict(&facts_of(t), &sf, &corpus),
                "phải nhận ra là trận động đất khác: {t}"
            );
        }

        // …còn diễn biến thật của chính trận Nhật Bản thì vẫn vào được, kể cả
        // khi số người chết đã đổi từ 13 lên 34 (con số đó KHÔNG phải mâu thuẫn).
        for t in [
            "Động đất Nhật Bản: Số người chết tăng lên 34, chạy đua cứu người",
            "Ít nhất 13 người thiệt mạng trong trận động đất ở Nhật Bản",
            "Tổ chức an táng công dân Việt Nam tử vong trong trận động đất tại Nhật Bản",
            "Khoảnh khắc vụ nổ ở trung tâm thương mại Aeon Mall sau động đất Nhật Bản",
        ] {
            assert!(
                !facts_conflict(&facts_of(t), &sf, &corpus),
                "diễn biến thật của sự kiện bị chặn nhầm: {t}"
            );
        }
    }

    /// Vì sao chỉ đọc TIÊU ĐỀ chứ không đọc cả đoạn mở bài — đo trên bài thật.
    #[test]
    fn the_lead_paragraph_names_events_the_article_is_not_about() {
        let lead = "Vào lúc 19 giờ 46 phút tối 31/7 theo giờ địa phương (rạng sáng 1/8 giờ \
                    Việt Nam), một trận động đất có độ lớn 4,7 đã làm rung chuyển thành phố \
                    Napoli. Trước đó là các trận ở Kamchatka Nga, Myanmar, Đài Loan, Trung \
                    Quốc và Nhật Bản.";
        assert!(
            proper_names(lead).contains("nhật bản"),
            "đoạn mở bài nhắc Nhật Bản dù bài viết về động đất ở Ý"
        );
        assert!(
            !facts_of("Italia xảy ra động đất mạnh nhất trong 40 năm qua tại Napoli")
                .names
                .contains("nhật bản"),
            "tiêu đề thì không — nên chỉ tiêu đề mới được dùng để nhận dạng sự việc"
        );
    }

    #[test]
    fn facts_stay_silent_when_they_know_nothing() {
        // Không nêu tên riêng nào thì không được phép phủ quyết — im lặng không
        // phải là mâu thuẫn.
        let sf = StoryFacts::seed(&facts_of("Giá vàng lập đỉnh tại Hà Nội"));
        assert!(!facts_conflict(&facts_of("Giá vàng tiếp tục tăng"), &sf, &fresh()));
        assert!(!facts_conflict(&Facts::default(), &sf, &fresh()));
    }

    #[test]
    fn proper_names_skip_the_first_word_and_shouting() {
        let n = proper_names("Động đất mạnh 4,7 độ Richter làm rung chuyển miền nam Ý");
        assert!(n.contains("ý"), "tên riêng cuối câu: {n:?}");
        assert!(!n.contains("động"), "chữ đầu câu viết hoa do ngữ pháp: {n:?}");
        let n2 = proper_names("NÓNG: Nổ tại siêu thị Aeon Nhật Bản sau trận động đất");
        assert!(n2.contains("aeon nhật bản"), "cụm tên riêng liền nhau: {n2:?}");
        assert!(!n2.contains("nóng"), "chữ in hoa toàn bộ không phải tên: {n2:?}");
    }

    #[test]
    fn measures_read_number_plus_unit() {
        let m = measures("Động đất 7,1 độ khiến 150.000 người phải sơ tán");
        assert!(m.contains(&("độ".into(), "7.1".into())));
        assert!(m.contains(&("người".into(), "150.000".into())));
    }

    #[test]
    fn digest_pages_are_not_events() {
        for t in [
            "Điểm tin 6h: TP.HCM đầu tư hơn 7.000 tỷ đồng cho loạt dự án thoát nước | VN-Index giành lại mốc 1.700 điểm",
            "Tin tức thế giới 28-7: Iran nói 'không xin đàm phán' với Mỹ",
            "Toàn cảnh 17h: Hé lộ vụ phá tường gây sập nhà, chết người",
            "NHỊP SỐNG 24: Dư chấn động đất ở Nhật Bản kéo dài bao lâu?",
            "Bản tin VietNamNet 1/8: Hà Nội áp dụng mô hình thành phố bọt biển",
        ] {
            assert!(is_digest_title(t), "phải nhận ra là trang điểm tin: {t}");
            assert_eq!(assign_story(t, T0, &[], &fresh()), None);
        }
        // Tin thường không được nhận nhầm là điểm tin.
        for t in [
            "Lạ kỳ đám mây 'nấm khổng lồ' sau động đất ở Nhật Bản",
            "Giá vàng hôm nay giảm ngày thứ 2 liên tiếp",
            "Tin vui cho người dân vùng lũ: đường vào xã đã thông",
        ] {
            assert!(!is_digest_title(t), "nhận nhầm tin thường: {t}");
        }
    }

    #[test]
    fn story_does_not_stretch_across_months() {
        let s = story_at(3, T0, &["Giá vàng hôm nay tăng mạnh, lập đỉnh mới"]);
        // Bài y hệt nhưng của hai tháng sau: cùng chủ đề, khác sự kiện.
        assert_eq!(
            assign_story(
                "Giá vàng hôm nay tăng mạnh, lập đỉnh mới",
                T0 + 60 * 86400,
                std::slice::from_ref(&s),
                &everyday(),
            ),
            None
        );
        assert_eq!(
            assign_story(
                "Giá vàng hôm nay tăng mạnh, lập đỉnh mới",
                T0 + 3600,
                std::slice::from_ref(&s),
                &everyday(),
            ),
            Some(3)
        );
    }

    #[test]
    fn quiet_story_does_not_reopen() {
        let s = story_at(4, T0, &["Bão số 3 đổ bộ Quảng Ninh, hàng nghìn hộ dân sơ tán"]);
        assert_eq!(
            assign_story(
                "Quảng Ninh sơ tán dân trước khi bão số 3 đổ bộ",
                T0 + 10 * 86400,
                &[s],
                &everyday(),
            ),
            None,
            "im lặng 10 ngày thì không còn là cùng một diễn biến"
        );
    }

    #[test]
    fn markup_fragments_are_not_words() {
        assert!(!tokens("Toàn cảnh rạch &apos;hồi sinh&apos; sau cải tạo")
            .contains(&"apos".to_string()));
    }

    #[test]
    fn trends_detect_spike_over_background() {
        let current: Vec<(i64, String)> = vec![
            (1, "Giá vàng lập đỉnh lịch sử".into()),
            (2, "Giá vàng tăng mạnh phiên sáng".into()),
            (3, "Người dân xếp hàng mua khi giá vàng tăng".into()),
            (4, "Thời tiết Hà Nội hôm nay".into()),
        ];
        let previous: Vec<(i64, String)> = vec![
            (90, "Thời tiết Hà Nội se lạnh".into()),
            (91, "Thời tiết Hà Nội có mưa".into()),
        ];
        let trends = detect_trends(&current, &previous, 2, 10, &no_background);
        assert!(!trends.is_empty());
        let top = &trends[0];
        assert!(
            top.phrase.contains("vàng"),
            "top trend should be about vàng, got {}",
            top.phrase
        );
        assert_eq!(top.count, 3);
        // "thời tiết hà nội" appears once now and twice before — not a spike.
        assert!(trends
            .iter()
            .all(|t| !t.phrase.contains("thời tiết") || t.score < top.score));
    }

    #[test]
    fn trends_fold_subphrases_with_same_support() {
        let current: Vec<(i64, String)> = vec![
            (1, "Giá vàng lập đỉnh".into()),
            (2, "Giá vàng tăng tiếp".into()),
            (3, "Giá vàng vượt mốc".into()),
        ];
        let trends = detect_trends(&current, &[], 2, 10, &no_background);
        let phrases: Vec<&str> = trends.iter().map(|t| t.phrase.as_str()).collect();
        // "giá vàng" kept; bare "giá"/"vàng" with identical support folded away.
        assert!(phrases.contains(&"giá vàng"));
        assert!(!phrases.contains(&"vàng"));
        assert!(!phrases.contains(&"giá"));
    }

    #[test]
    fn trends_prefer_specific_phrase_over_higher_count_unigram() {
        // 1 bài lạc chỉ có "vàng" không được kéo vào xu hướng "giá vàng", và
        // nhãn phải là cụm từ, không phải âm tiết trần.
        let current: Vec<(i64, String)> = vec![
            (1, "Giá vàng lập đỉnh".into()),
            (2, "Giá vàng tăng tiếp".into()),
            (3, "Giá vàng vượt mốc".into()),
            (4, "Đeo vàng đi bơi bị mất trộm".into()),
        ];
        let trends = detect_trends(&current, &[], 2, 10, &no_background);
        let phrases: Vec<&str> = trends.iter().map(|t| t.phrase.as_str()).collect();
        assert!(phrases.contains(&"giá vàng"), "got {phrases:?}");
        assert!(
            !phrases.contains(&"vàng"),
            "bare unigram must be folded, got {phrases:?}"
        );
    }

    #[test]
    fn trends_merge_one_event_named_several_ways() {
        // One sinking, three vocabularies. No phrase here CONTAINS another, so
        // folding on spelling gave this event a card per wording; folding on
        // the articles themselves gives it one.
        let current: Vec<(i64, String)> = vec![
            (1, "Ferry capsizes off northern Cyprus, eight dead".into()),
            (2, "At least 8 dead as ferry capsizes off northern Cyprus".into()),
            (3, "Ferry carrying 270 capsizes off Cyprus coast".into()),
        ];
        let trends = detect_trends(&current, &[], 2, 10, &no_background);
        let phrases: Vec<&str> = trends.iter().map(|t| t.phrase.as_str()).collect();
        assert_eq!(trends.len(), 1, "one event, one card — got {phrases:?}");
        assert_eq!(trends[0].count, 3);
        // Label grows out of the headlines instead of stopping at the seed.
        assert!(
            trends[0].phrase.contains("cyprus"),
            "label should name the event, got {phrases:?}"
        );
    }

    #[test]
    fn trends_drop_umbrella_phrase_over_unrelated_stories() {
        // "hà nội" covers the fire AND eight stories that share nothing with it.
        // Merging would file all nine under one trend; it is dropped instead.
        let mut current: Vec<(i64, String)> = vec![
            (1, "Cháy chung cư Hà Nội trong đêm".into()),
            (2, "Cứu hộ vụ cháy chung cư Hà Nội".into()),
            (3, "Cháy chung cư Hà Nội: 2 người bị thương".into()),
        ];
        for (i, rest) in [
            "khai trương tuyến buýt nhanh",
            "tuyển sinh lớp 10 sớm hơn",
            "giải chạy đêm thu hút vạn người",
            "sửa mặt đường vành đai 3",
            "chợ hoa Quảng An họp trở lại",
            "triển lãm sách mở cửa miễn phí",
            "thu phí đỗ ô tô không tiền mặt",
            "công viên Thống Nhất bỏ hàng rào",
        ]
        .iter()
        .enumerate()
        {
            current.push((10 + i as i64, format!("Hà Nội {rest}")));
        }
        // Steady background before, so the umbrella never outscores the fire.
        let previous: Vec<(i64, String)> = (0..20)
            .map(|i| (100 + i, format!("Hà Nội thời tiết ngày {i}")))
            .collect();
        let trends = detect_trends(&current, &previous, 2, 10, &no_background);
        let phrases: Vec<&str> = trends.iter().map(|t| t.phrase.as_str()).collect();
        let fire = trends
            .iter()
            .find(|t| t.phrase.contains("chung cư"))
            .unwrap_or_else(|| panic!("fire trend missing from {phrases:?}"));
        assert_eq!(fire.count, 3, "umbrella must not inflate it: {phrases:?}");
        assert!(
            !phrases.iter().any(|p| *p == "hà nội"),
            "umbrella phrase must not get its own card, got {phrases:?}"
        );
    }

    #[test]
    fn trends_skip_everyday_background_phrases() {
        // "việt nam" leads on raw count in any Vietnamese feed. What makes it
        // background is not a stoplist entry but its share of the archive.
        let current: Vec<(i64, String)> = vec![
            (1, "Việt Nam và Lào ký hiệp định".into()),
            (2, "Việt Nam dự hội nghị khí hậu".into()),
            (3, "Việt Nam xuất siêu quý III".into()),
            (4, "Động đất Kon Tum 4,2 độ".into()),
            (5, "Động đất Kon Tum khiến nhà rung lắc".into()),
        ];
        let archive = |_: &BTreeSet<String>| Corpus {
            total: 1000,
            df: [("việt nam".to_string(), 50)].into_iter().collect(),
        };
        let trends = detect_trends(&current, &[], 2, 10, &archive);
        let phrases: Vec<&str> = trends.iter().map(|t| t.phrase.as_str()).collect();
        assert!(
            !phrases.iter().any(|p| p.contains("việt nam")),
            "5% of the archive is background, not a trend: {phrases:?}"
        );
        assert!(
            phrases.iter().any(|p| p.contains("động đất")),
            "the real event must survive: {phrases:?}"
        );
    }

    fn story_of(id: i64, titles: &[&str]) -> StoryPhrases {
        StoryPhrases {
            story_id: id,
            phrases: phrases_of(&titles.iter().map(|t| t.to_string()).collect::<Vec<_>>()),
        }
    }

    #[test]
    fn story_links_connect_related_events_only() {
        let vang1 = story_of(
            1,
            &[
                "Giá vàng lập đỉnh lịch sử mới",
                "Giá vàng trong nước lập đỉnh",
            ],
        );
        let vang2 = story_of(
            2,
            &[
                "Trung Quốc gom hàng khi giá vàng lập đỉnh",
                "Ngân hàng trung ương mua vào lúc giá vàng lập đỉnh",
            ],
        );
        let bao = story_of(
            3,
            &[
                "Bão số 3 đổ bộ Quảng Ninh",
                "Quảng Ninh sơ tán dân tránh bão",
            ],
        );
        let links = story_links(&[vang1, vang2, bao], &fresh());
        assert!(
            links.iter().any(|l| (l.a, l.b) == (1, 2)),
            "gold stories must link"
        );
        assert!(
            links.iter().all(|l| l.a != 3 && l.b != 3),
            "storm story must stay isolated"
        );
        let l = links.iter().find(|l| (l.a, l.b) == (1, 2)).unwrap();
        assert!(
            l.shared.iter().any(|p| p == "giá vàng"),
            "shared phrase, got {:?}",
            l.shared
        );
        assert!(l.weight > 0.0 && l.weight <= 1.0);
    }

    #[test]
    fn pruning_keeps_a_readable_skeleton_without_orphaning_anyone() {
        // 12 sự kiện nối chằng chịt (mô phỏng đúng cảnh 60 node / 467 cạnh).
        let mut links = Vec::new();
        for a in 0..12i64 {
            for b in (a + 1)..12 {
                links.push(StoryLink {
                    a,
                    b,
                    // Cạnh của node nhỏ mạnh hơn, để kiểm tra thứ tự được tôn trọng.
                    weight: 1.0 - (a + b) as f64 / 30.0,
                    shared: vec!["x".into(), "y".into()],
                });
            }
        }
        links.sort_by(|x, y| y.weight.partial_cmp(&x.weight).unwrap());
        let total = links.len();
        let (kept, dropped) = prune_links(links, 3);

        assert_eq!(kept.len() + dropped, total, "không được nuốt mất cạnh nào");
        assert!(kept.len() < total / 2, "phải thưa hẳn đi, còn {}", kept.len());
        // Mọi sự kiện vẫn còn ít nhất một liên kết — cắt bớt không được bỏ rơi ai.
        let mut seen: HashSet<i64> = HashSet::new();
        for l in &kept {
            seen.insert(l.a);
            seen.insert(l.b);
        }
        assert_eq!(seen.len(), 12, "có node bị cắt trụi hết liên kết");
        // Cạnh mạnh nhất luôn được giữ.
        assert!(kept.iter().any(|l| (l.a, l.b) == (0, 1)));
    }

    #[test]
    fn prune_zero_keeps_everything() {
        let links = vec![StoryLink { a: 1, b: 2, weight: 0.5, shared: vec!["x".into()] }];
        let (kept, dropped) = prune_links(links, 0);
        assert_eq!((kept.len(), dropped), (1, 0));
    }

    #[test]
    fn map_filler_uses_the_archive_not_just_the_screen() {
        // "việt nam" chỉ xuất hiện ở 1 trong 5 sự kiện trên bản đồ nên bộ lọc
        // cục bộ tha, nhưng cả kho thì nó là ngôn ngữ hằng ngày.
        let mut stories: Vec<StoryPhrases> = (0..5)
            .map(|i| story_of(i, &[&format!("Chuyện riêng lẻ số {i} chẳng giống ai")]))
            .collect();
        stories.push(story_of(9, &["Hội nghị lớn khai mạc tại Việt Nam"]));
        let local = generic_phrases(&stories);
        assert!(
            !local.contains("việt nam"),
            "chỉ 1/6 sự kiện nhắc tới nên bộ lọc cục bộ không thấy gì bất thường"
        );
        let with_archive = map_filler(&stories, &everyday());
        assert!(
            with_archive.contains("việt nam"),
            "nhưng cả kho thì đây là ngôn ngữ hằng ngày"
        );
    }

    #[test]
    fn story_similarity_empty_profiles_are_zero() {
        let empty = BTreeSet::new();
        let full = phrases_of(&["Giá vàng tăng mạnh".to_string()]);
        let none = BTreeSet::new();
        assert_eq!(story_similarity(&empty, &full, &none).0, 0.0);
        assert_eq!(story_similarity(&full, &empty, &none).0, 0.0);
    }

    #[test]
    fn single_syllable_overlap_does_not_link_unrelated_events() {
        // "thế giới"/"mạnh nhất" là cụm phổ thông, phải bị lọc; hai sự kiện này
        // không chung chuyện gì.
        let stories = vec![
            story_of(
                1,
                &[
                    "Giá dầu thế giới lao dốc mạnh nhất",
                    "Dầu thế giới giảm mạnh nhất phiên cuối",
                ],
            ),
            story_of(
                2,
                &[
                    "Tên lửa mạnh nhất thế giới phóng thử",
                    "Starship mạnh nhất thế giới hạ cánh",
                ],
            ),
            story_of(
                3,
                &[
                    "Chứng khoán thế giới đỏ lửa mạnh nhất",
                    "Cổ phiếu thế giới lao dốc mạnh nhất",
                ],
            ),
            story_of(
                4,
                &[
                    "Bóng đá thế giới chấn động mạnh nhất",
                    "Làng bóng đá thế giới rúng động mạnh nhất",
                ],
            ),
            story_of(
                5,
                &[
                    "Xe điện thế giới bán chậm mạnh nhất",
                    "Doanh số xe điện thế giới giảm mạnh nhất",
                ],
            ),
        ];
        let generic = generic_phrases(&stories);
        assert!(
            generic.contains("thế giới"),
            "'thế giới' phải là cụm phổ thông: {generic:?}"
        );

        let links = story_links(&stories, &fresh());
        assert!(
            links.iter().all(|l| !(l.a == 1 && l.b == 2)),
            "dầu ↔ tên lửa chỉ chung cụm phổ thông, không được nối: {:?}",
            links
                .iter()
                .map(|l| (l.a, l.b, l.shared.clone()))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn profile_json_roundtrip() {
        let p = story(1, &["Bão số 3 đổ bộ Quảng Ninh"]).profile;
        let s = profile_to_json(&p);
        let back = profile_from_json(&s);
        assert_eq!(p, back);
    }
}
