//! AI layer of the News app — every call goes through the SenClaw daemon
//! bridge (`llm_request_full`), never a provider directly. Four jobs:
//!   * đánh giá MỘT bài (structured JSON: tóm tắt, cảm xúc, tầm quan trọng,
//!     giật tít, độ tin cậy, tags),
//!   * tóm tắt một DÒNG SỰ KIỆN theo timeline,
//!   * ĐIỂM TIN một khoảng thời gian,
//!   * nhận định XU HƯỚNG từ số liệu trend đã tính sẵn.
//!
//! Bridge constraints respected here (learned repo-wide):
//!   * no `temperature` knob — determinism comes from prompts + validation
//!     ([[space-app-llm-bridge-no-temperature]]);
//!   * the output cap is the real ceiling and it must be sized for reasoning
//!     models ([[space-app-llm-bridge-output-ceiling]]) — see [`budget`];
//!   * `finish` is NOT trustworthy. Measured against the gateway model
//!     `ag/gemini-pro-agent`: a reply visibly chopped mid-word came back with
//!     `finish == "stop"`. So truncation is detected from the *shape* of the
//!     reply as well ([`looks_truncated`]), and prose answers are handed back
//!     WITH a truncated flag rather than thrown away — a cut answer is still
//!     worth reading, an error message is not.

use crate::fetch::clip;
use anyhow::{anyhow, Result};
use app_space_sdk::SpaceClient;
use serde::Deserialize;
use serde_json::Value;

/// Max chars of article/evidence text pushed into one prompt.
const INPUT_CAP: usize = 16_000;

/// Output cap for a completion that should show roughly `visible` tokens.
///
/// Reasoning models bill their hidden trace against the same cap, and
/// Vietnamese tokenizes far worse than English, so the cap that "looks" right
/// for the visible answer runs out mid-sentence. Measured on the bridge with a
/// 500-word Vietnamese prompt: `maxTokens = 2000` returned 1 397 chars cut
/// mid-word, `8000` returned a complete 2 809-char answer. Hence the
/// multiplier — it costs nothing when the model stops early.
const fn budget(visible: u32) -> u32 {
    visible * 6
}

/// Does this reply look cut off mid-thought?
///
/// Used because `finish` lies (see module docs). Deliberately conservative: a
/// complete answer ends on sentence-final punctuation, a closing quote or
/// bracket, or a markdown marker — anything else (a bare word, a comma, a
/// dangling dash) means the model was still writing. False negatives are fine;
/// a false positive would slap a warning on a perfectly good answer.
pub fn looks_truncated(text: &str) -> bool {
    match text.trim_end().chars().last() {
        None => true,
        Some(c) => !matches!(
            c,
            '.' | '!'
                | '?'
                | '…'
                | ':'
                | ';'
                | '"'
                | '”'
                | '\''
                | '’'
                | ')'
                | ']'
                | '}'
                | '>'
                | '*'
                | '`'
                | '%'
                | '_'
        ),
    }
}

/// One completion. Returns `(text, model, truncated)`.
/// Language every answer must come back in, from the `display_language`
/// setting. Empty (or the Vietnamese default) leaves the prompts as written.
static OUTPUT_LANGUAGE: std::sync::RwLock<String> = std::sync::RwLock::new(String::new());

pub fn set_output_language(lang: &str) {
    if let Ok(mut g) = OUTPUT_LANGUAGE.write() {
        *g = lang.trim().to_string();
    }
}

fn output_language() -> String {
    OUTPUT_LANGUAGE.read().map(|g| g.clone()).unwrap_or_default()
}

/// Append the output-language rule to a system prompt.
///
/// The prompts below are written in Vietnamese and say so; this overrides them
/// LAST so it wins, and it deliberately separates the reading language from the
/// writing one — sources stay in whatever language they publish in, and only
/// the answer is pinned.
fn with_language(system: &str) -> String {
    let lang = output_language();
    if lang.is_empty() || lang.eq_ignore_ascii_case("Tiếng Việt") || lang.eq_ignore_ascii_case("vi")
    {
        return system.to_string();
    }
    format!(
        "{system}\n\nNGÔN NGỮ ĐẦU RA: viết TOÀN BỘ câu trả lời bằng {lang}, kể cả tiêu đề mục. \
         Nguồn có thể ở bất kỳ ngôn ngữ nào — hãy đọc hiểu rồi diễn đạt lại bằng {lang}. \
         Giữ nguyên tên riêng, tên nguồn và số liệu; khi dịch một tiêu đề bài báo, đặt bản gốc \
         trong ngoặc đơn ngay sau đó."
    )
}

async fn ask(
    sc: &SpaceClient,
    system: &str,
    prompt: &str,
    max_tokens: u32,
) -> Result<(String, String, bool)> {
    ask_in(sc, system, prompt, max_tokens, true).await
}

/// `localize=false` for calls that state their own target language (the
/// translator) — appending the global rule there would fight the prompt.
async fn ask_in(
    sc: &SpaceClient,
    system: &str,
    prompt: &str,
    max_tokens: u32,
    localize: bool,
) -> Result<(String, String, bool)> {
    let localized;
    let system = if localize {
        localized = with_language(system);
        localized.as_str()
    } else {
        system
    };
    let (text, model, finish) = sc
        .llm_request_full(system, prompt, max_tokens, None)
        .await?;
    let text = text.trim().to_string();
    let truncated = finish == "length" || looks_truncated(&text);
    Ok((text, model, truncated))
}

/// A completion whose reply must parse as JSON. Here truncation IS fatal —
/// half a JSON object cannot be shown to anyone — but only when it actually
/// damaged the JSON: a model that closes its object and then gets cut off
/// mid-epilogue has still answered.
async fn ask_json(
    sc: &SpaceClient,
    system: &str,
    prompt: &str,
    max_tokens: u32,
) -> Result<(String, String)> {
    ask_json_in(sc, system, prompt, max_tokens, true).await
}

async fn ask_json_in(
    sc: &SpaceClient,
    system: &str,
    prompt: &str,
    max_tokens: u32,
    localize: bool,
) -> Result<(String, String)> {
    let (text, model, truncated) = ask_in(sc, system, prompt, max_tokens, localize).await?;
    if truncated && extract_json(&text).is_none() {
        return Err(anyhow!(
            "AI trả lời bị cắt giữa chừng (trần {max_tokens} token) — thử lại hoặc giảm bớt dữ liệu đầu vào"
        ));
    }
    Ok((text, model))
}

/// Pull the first top-level JSON object out of a model reply that may carry
/// prose or code fences around it.
pub fn extract_json(text: &str) -> Option<&str> {
    let start = text.find('{')?;
    let bytes = text.as_bytes();
    let mut depth = 0usize;
    let mut in_str = false;
    let mut esc = false;
    for (i, &b) in bytes.iter().enumerate().skip(start) {
        if esc {
            esc = false;
            continue;
        }
        match b {
            b'\\' if in_str => esc = true,
            b'"' => in_str = !in_str,
            b'{' if !in_str => depth += 1,
            b'}' if !in_str => {
                depth -= 1;
                if depth == 0 {
                    return Some(&text[start..=i]);
                }
            }
            _ => {}
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Per-article đánh giá
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct ArticleVerdict {
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub sentiment: String,
    #[serde(default)]
    pub importance: i64,
    #[serde(default)]
    pub clickbait: bool,
    #[serde(default)]
    pub reliability: String,
    #[serde(default)]
    pub tags: Vec<String>,
}

// ---------------------------------------------------------------------------
// Dịch sang ngôn ngữ hiển thị
// ---------------------------------------------------------------------------

const TRANSLATE_SYSTEM: &str = "Bạn là biên dịch viên tin tức. Bạn nhận một danh sách tiêu đề và \
mô tả bài báo, có thể ở BẤT KỲ ngôn ngữ nào, và dịch sang ngôn ngữ được yêu cầu. \
NGUYÊN TẮC: dịch sát nghĩa, giữ giọng tin tức, không tóm tắt, không thêm bình luận; \
giữ nguyên tên riêng, tên tổ chức, số liệu và đơn vị; nếu một mục đã đúng ngôn ngữ đích thì \
chép lại nguyên văn. Chỉ trả về ĐÚNG một khối JSON dạng \
{\"items\":[{\"id\":<số>,\"title\":\"…\",\"description\":\"…\"}]} theo đúng thứ tự nhận được.";

#[derive(Deserialize)]
pub struct TranslatedItem {
    pub id: i64,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub description: String,
}

#[derive(Deserialize)]
struct TranslatedBatch {
    #[serde(default)]
    items: Vec<TranslatedItem>,
}

pub fn translate_prompt(items: &[(i64, String, String)], lang: &str) -> String {
    let mut out = format!("Ngôn ngữ đích: {lang}\n\nCác mục cần dịch:\n");
    let mut budget = INPUT_CAP;
    for (id, title, desc) in items {
        let line = format!("- id {id}\n  title: {title}\n  description: {}\n", clip(desc, 400));
        if line.chars().count() > budget {
            break;
        }
        budget -= line.chars().count();
        out.push_str(&line);
    }
    out.push_str("\nTrả về JSON đúng định dạng đã nêu.");
    out
}

/// Translate a batch of `(id, title, description)` into `lang`.
pub async fn translate(
    sc: &SpaceClient,
    items: &[(i64, String, String)],
    lang: &str,
) -> Result<Vec<TranslatedItem>> {
    if items.is_empty() {
        return Ok(Vec::new());
    }
    // Translation must NOT inherit the global output-language rule — the target
    // is stated in the prompt, and the batch JSON shape has to survive intact.
    let (raw, _model) = ask_json_in(
        sc,
        TRANSLATE_SYSTEM,
        &translate_prompt(items, lang),
        budget(items.len() as u32 * 120),
        false,
    )
    .await?;
    // `ask_json` hands back the reply verbatim — models wrap the object in prose
    // or a code fence, so the object has to be cut out before parsing.
    let json = extract_json(&raw)
        .ok_or_else(|| anyhow!("AI không trả về JSON: {}", clip(&raw, 200)))?;
    let parsed: TranslatedBatch =
        serde_json::from_str(json).map_err(|e| anyhow!("JSON dịch không hợp lệ: {e}"))?;
    let known: std::collections::HashSet<i64> = items.iter().map(|(id, _, _)| *id).collect();
    Ok(parsed
        .items
        .into_iter()
        .filter(|t| known.contains(&t.id) && !t.title.trim().is_empty())
        .collect())
}

const ARTICLE_SYSTEM: &str = "Bạn là biên tập viên thẩm định tin tức. Bạn nhận MỘT bài báo \
(tiêu đề, nguồn, thời gian, mô tả, và nội dung nếu có) và đánh giá nó. \
NGUYÊN TẮC: chỉ dựa trên văn bản được cung cấp — không dùng kiến thức ngoài để 'sửa' nội dung bài; \
nếu bài chỉ có tiêu đề + mô tả ngắn thì đánh giá thận trọng và nói rõ dữ liệu mỏng trong phần reliability. \
Chỉ trả về ĐÚNG một khối JSON, không giải thích gì thêm.";

fn article_prompt(article: &Value) -> String {
    let title = article["title"].as_str().unwrap_or("");
    let source = article["source_name"].as_str().unwrap_or("");
    let time = article["published_at"].as_str().unwrap_or("");
    let desc = article["description"].as_str().unwrap_or("");
    let content = article["content"].as_str().unwrap_or("");
    let body = if content.trim().is_empty() {
        desc.to_string()
    } else {
        clip(content, INPUT_CAP)
    };
    format!(
        "Bài báo:\n- Tiêu đề: {title}\n- Nguồn: {source}\n- Thời gian: {time}\n- Mô tả: {desc}\n- Nội dung: {body}\n\n\
         Trả về JSON đúng cấu trúc:\n\
         {{\"summary\":\"tóm tắt 2-3 câu tiếng Việt\",\
         \"sentiment\":\"positive|negative|neutral|mixed\",\
         \"importance\":1,\
         \"clickbait\":false,\
         \"reliability\":\"1 câu nhận xét độ tin cậy dựa trên nguồn + cách viết (dữ liệu mỏng thì nói rõ)\",\
         \"tags\":[\"tối đa 5 tag ngắn tiếng Việt\"]}}\n\
         - importance: 1-5 (1 = tin vặt, 5 = ảnh hưởng rộng).\n\
         - clickbait: true nếu tiêu đề giật gân/phóng đại so với nội dung."
    )
}

/// Đánh giá một bài. Returns the parsed verdict + model id.
pub async fn analyze_article(
    sc: &SpaceClient,
    article: &Value,
) -> Result<(ArticleVerdict, String)> {
    let (text, model) =
        ask_json(sc, ARTICLE_SYSTEM, &article_prompt(article), budget(1200)).await?;
    let json_str =
        extract_json(&text).ok_or_else(|| anyhow!("AI không trả về JSON: {}", clip(&text, 200)))?;
    let mut v: ArticleVerdict =
        serde_json::from_str(json_str).map_err(|e| anyhow!("JSON đánh giá không hợp lệ: {e}"))?;
    v.importance = v.importance.clamp(1, 5);
    if !["positive", "negative", "neutral", "mixed"].contains(&v.sentiment.as_str()) {
        v.sentiment = "neutral".into();
    }
    v.tags.truncate(5);
    Ok((v, model))
}

// ---------------------------------------------------------------------------
// Story brief (tóm tắt dòng sự kiện)
// ---------------------------------------------------------------------------

const STORY_SYSTEM: &str = "Bạn là biên tập viên tổng hợp. Bạn nhận TIMELINE các bài báo về cùng một \
sự kiện (đã sắp theo thời gian) và viết bản tóm tắt diễn biến bằng tiếng Việt. \
NGUYÊN TẮC: chỉ dùng thông tin trong các bài được cung cấp, TUYỆT ĐỐI không bịa thêm chi tiết; \
mâu thuẫn giữa các nguồn thì nêu rõ cả hai phía. Kết cấu: 1 đoạn tóm tắt tổng thể, \
sau đó mục '## Diễn biến' dạng gạch đầu dòng theo mốc thời gian (mỗi dòng: `- [thời gian] sự việc (nguồn)`), \
cuối cùng '## Điểm còn bỏ ngỏ' nếu có. Ngắn gọn, không lặp lại nguyên văn tiêu đề.";

/// Build the story prompt from `Db::get_story` output.
///
/// The stored timeline is newest-first (that's how the UI reads it); a
/// narrative of "diễn biến" has to run forward in time, so it is reversed here
/// rather than in the DB.
pub fn story_prompt(story: &Value) -> String {
    let mut out = format!(
        "Sự kiện: {}\n\nCác bài theo thời gian:\n",
        story["title"].as_str().unwrap_or("")
    );
    let empty = Vec::new();
    let mut timeline: Vec<&Value> = story["timeline"]
        .as_array()
        .unwrap_or(&empty)
        .iter()
        .collect();
    timeline.reverse();
    let mut budget = INPUT_CAP;
    for a in timeline.iter().take(40) {
        let line = format!(
            "- [{}] ({}) {} — {}\n",
            a["published_at"].as_str().unwrap_or(""),
            a["source_name"].as_str().unwrap_or(""),
            a["title"].as_str().unwrap_or(""),
            clip(a["description"].as_str().unwrap_or(""), 300),
        );
        if line.chars().count() > budget {
            break;
        }
        budget -= line.chars().count();
        out.push_str(&line);
    }
    out.push_str("\nViết bản tóm tắt diễn biến theo đúng kết cấu đã nêu.");
    out
}

/// Returns `(text, model, truncated)`.
pub async fn story_brief(sc: &SpaceClient, story: &Value) -> Result<(String, String, bool)> {
    ask(sc, STORY_SYSTEM, &story_prompt(story), budget(2000)).await
}

// ---------------------------------------------------------------------------
// Điểm tin (digest)
// ---------------------------------------------------------------------------

const DIGEST_SYSTEM: &str = "Bạn là biên tập viên bản tin. Bạn nhận danh sách bài báo gần đây \
(kèm nguồn, thời gian) và các dòng sự kiện nóng, rồi viết BẢN ĐIỂM TIN tiếng Việt. \
NGUYÊN TẮC: chỉ dùng các bài được cung cấp, không bịa; ưu tiên sự kiện có nhiều nguồn đưa; \
gộp các bài trùng sự kiện thành một mục. Kết cấu: '## Tin chính' (3-6 mục, mỗi mục 1-2 câu kèm tên nguồn), \
'## Đáng chú ý' (gạch đầu dòng ngắn), và nếu danh sách có nhiều bài cùng chủ đề thì thêm '## Xu hướng'. \
Không thêm lời chào hay rào đón.";

pub fn digest_prompt(articles: &[Value], stories: &[Value], focus: &str) -> String {
    let mut out = String::new();
    if !focus.trim().is_empty() {
        out.push_str(&format!(
            "Trọng tâm người đọc quan tâm: {}\n\n",
            focus.trim()
        ));
    }
    out.push_str("Dòng sự kiện nóng (nhiều bài cùng đưa):\n");
    for s in stories.iter().take(8) {
        out.push_str(&format!(
            "- {} ({} bài, cập nhật {})\n",
            s["title"].as_str().unwrap_or(""),
            s["article_count"].as_i64().unwrap_or(0),
            s["last_at"].as_str().unwrap_or(""),
        ));
    }
    out.push_str("\nCác bài gần đây:\n");
    let mut budget = INPUT_CAP;
    for a in articles.iter().take(60) {
        let line = format!(
            "- [{}] ({}) {}\n",
            a["published_at"].as_str().unwrap_or(""),
            a["source_name"].as_str().unwrap_or(""),
            a["title"].as_str().unwrap_or(""),
        );
        if line.chars().count() > budget {
            break;
        }
        budget -= line.chars().count();
        out.push_str(&line);
    }
    out.push_str("\nViết bản điểm tin theo đúng kết cấu.");
    out
}

/// Returns `(text, model, truncated)`.
pub async fn digest(
    sc: &SpaceClient,
    articles: &[Value],
    stories: &[Value],
    focus: &str,
) -> Result<(String, String, bool)> {
    ask(
        sc,
        DIGEST_SYSTEM,
        &digest_prompt(articles, stories, focus),
        budget(2500),
    )
    .await
}

// ---------------------------------------------------------------------------
// Nhận định xu hướng
// ---------------------------------------------------------------------------

const TREND_SYSTEM: &str = "Bạn là chuyên viên phân tích truyền thông. Bạn nhận danh sách CỤM TỪ \
đang tăng nhiệt trong các tiêu đề tin (kèm số bài hiện tại, số bài kỳ trước, và vài tiêu đề mẫu) — \
số liệu do máy đếm sẵn, TUYỆT ĐỐI không bịa số hay thêm xu hướng ngoài danh sách. \
Với mỗi xu hướng đáng nói: nó là chuyện gì, vì sao nóng lên, và đáng theo dõi tiếp điều gì. \
Trả lời tiếng Việt, gọn, kết luận trước chi tiết sau. \
Kết thúc bằng đúng một dòng: \"Lưu ý: nhận định tham khảo dựa trên tiêu đề đã thu thập.\"";

pub fn trends_prompt(trends: &Value, samples: &Value) -> String {
    format!(
        "Cụm từ tăng nhiệt (JSON):\n{}\n\nTiêu đề mẫu cho từng cụm (JSON):\n{}\n\nPhân tích các xu hướng trên.",
        serde_json::to_string_pretty(trends).unwrap_or_default(),
        clip(&serde_json::to_string_pretty(samples).unwrap_or_default(), 8_000),
    )
}

/// Returns `(text, model, truncated)`.
pub async fn analyze_trends(
    sc: &SpaceClient,
    trends: &Value,
    samples: &Value,
) -> Result<(String, String, bool)> {
    ask(
        sc,
        TREND_SYSTEM,
        &trends_prompt(trends, samples),
        budget(2000),
    )
    .await
}

// ---------------------------------------------------------------------------
// Tóm tắt MỘT xu hướng
// ---------------------------------------------------------------------------

/// The whole-tab analysis above reads fifteen trends at once and can only say a
/// sentence about each. This one reads a single trend's articles and says what
/// actually happened in it.
const TREND_ONE_SYSTEM: &str = "Bạn là chuyên viên phân tích truyền thông. Bạn nhận MỘT cụm từ đang tăng nhiệt (kèm số bài trong kỳ này và kỳ trước) cùng danh sách tiêu đề các bài thuộc cụm đó (kèm nguồn, thời gian) — số liệu do máy đếm sẵn, TUYỆT ĐỐI không bịa số và chỉ dùng các bài được cung cấp. Viết tiếng Việt, gọn, theo đúng ba mục: '**Chuyện gì**' (1-2 câu nói rõ sự việc), '**Diễn biến chính**' (2-4 gạch đầu dòng, mỗi dòng kèm tên nguồn), '**Theo dõi tiếp**' (1-2 gạch đầu dòng). Nếu các bài thực ra nói về nhiều sự việc khác nhau chứ không phải một, hãy nói thẳng điều đó ngay ở mục đầu thay vì gộp chúng làm một. Không thêm lời chào hay rào đón. Kết thúc bằng đúng một dòng: \"Lưu ý: nhận định tham khảo dựa trên tiêu đề đã thu thập.\"";

pub fn trend_summary_prompt(trend: &Value, articles: &[Value]) -> String {
    let mut out = format!(
        "Cụm từ: {}\nSố bài kỳ này: {}\nSố bài kỳ trước: {}\n\nCác bài trong cụm:\n",
        trend["phrase"].as_str().unwrap_or(""),
        trend["count"].as_i64().unwrap_or(0),
        trend["prev_count"].as_i64().unwrap_or(0),
    );
    let mut budget = INPUT_CAP;
    for a in articles {
        let line = format!(
            "- [{}] ({}) {}\n",
            a["published_at"].as_str().unwrap_or(""),
            a["source"].as_str().unwrap_or(""),
            a["title"].as_str().unwrap_or(""),
        );
        if line.chars().count() > budget {
            break;
        }
        budget -= line.chars().count();
        out.push_str(&line);
    }
    out.push_str("\nTóm tắt xu hướng này theo đúng kết cấu đã nêu.");
    out
}

/// Returns `(text, model, truncated)`.
pub async fn trend_summary(
    sc: &SpaceClient,
    trend: &Value,
    articles: &[Value],
) -> Result<(String, String, bool)> {
    ask(
        sc,
        TREND_ONE_SYSTEM,
        &trend_summary_prompt(trend, articles),
        budget(900),
    )
    .await
}

// ---------------------------------------------------------------------------
// Nhận định bản đồ liên kết sự kiện
// ---------------------------------------------------------------------------

const GRAPH_SYSTEM: &str = "Bạn là chuyên viên phân tích thời sự. Bạn nhận danh sách DÒNG SỰ KIỆN \
(mỗi node: id, tiêu đề, số bài, thời gian) và các LIÊN KẾT MÁY tự tính bằng trùng cụm từ khóa \
(kèm cụm chung + mức trùng) — liên kết máy chỉ là thống kê, KHÔNG phải quan hệ nhân quả. \
Việc của bạn: MAP LẠI bản đồ theo hiểu biết ngữ nghĩa — \
(1) gom các sự kiện thực sự cùng một câu chuyện thành 'mạch chuyện' có tên gọi ngắn; \
(2) NỐI THÊM những cặp sự kiện liên quan mà máy bỏ sót vì không trùng từ (ví dụ nguyên nhân → hệ quả), \
mỗi liên kết nêu rõ quan hệ; \
(3) chỉ ra liên kết máy nối NHẦM vì chỉ chung từ phổ thông. \
NGUYÊN TẮC: chỉ dùng các sự kiện có trong danh sách và ĐÚNG id của chúng, \
TUYỆT ĐỐI không bịa sự kiện mới, không bịa id, không bịa số liệu. \
Chỉ trả về ĐÚNG một khối JSON, không giải thích ngoài JSON.";

/// AI's semantic re-map of the story graph.
#[derive(Debug, Deserialize, Default)]
pub struct GraphMap {
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub clusters: Vec<GraphCluster>,
    #[serde(default)]
    pub links: Vec<GraphLink>,
    #[serde(default)]
    pub noise: Vec<GraphLink>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct GraphCluster {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub story_ids: Vec<i64>,
    #[serde(default)]
    pub why: String,
}

#[derive(Debug, Deserialize, Clone)]
pub struct GraphLink {
    pub a: i64,
    pub b: i64,
    #[serde(default)]
    pub relation: String,
    #[serde(default)]
    pub why: String,
}

/// Drop anything referring to ids the model invented — a hallucinated node id
/// would draw an edge to nowhere, and silently dropping the edge is better
/// than rendering a lie.
pub fn sanitize_map(mut m: GraphMap, valid: &std::collections::HashSet<i64>) -> GraphMap {
    let ok = |l: &GraphLink| l.a != l.b && valid.contains(&l.a) && valid.contains(&l.b);
    m.links.retain(ok);
    m.noise.retain(ok);
    m.clusters.retain_mut(|c| {
        c.story_ids.retain(|id| valid.contains(id));
        c.story_ids.len() >= 2 && !c.name.trim().is_empty()
    });
    m.links.truncate(40);
    m.noise.truncate(20);
    m.clusters.truncate(12);
    m
}

/// Prompt from the `story_graph_value` payload (nodes + edges).
pub fn graph_prompt(nodes: &Value, edges: &Value, question: &str) -> String {
    let empty = Vec::new();
    let nodes_arr = nodes.as_array().unwrap_or(&empty);
    let edges_arr = edges.as_array().unwrap_or(&empty);
    let mut out = String::from("Các dòng sự kiện (node):\n");
    for n in nodes_arr.iter().take(60) {
        out.push_str(&format!(
            "- #{} \"{}\" ({} bài, {} → {})\n",
            n["id"].as_i64().unwrap_or(0),
            n["title"].as_str().unwrap_or(""),
            n["article_count"].as_i64().unwrap_or(0),
            n["first_at"].as_str().unwrap_or(""),
            n["last_at"].as_str().unwrap_or(""),
        ));
    }
    out.push_str("\nLiên kết (cạnh — cụm từ dùng chung):\n");
    for e in edges_arr.iter().take(80) {
        let shared: Vec<&str> = e["shared"]
            .as_array()
            .map(|a| a.iter().filter_map(|x| x.as_str()).collect())
            .unwrap_or_default();
        out.push_str(&format!(
            "- #{} ↔ #{} · trùng {}% · cụm chung: {}\n",
            e["a"].as_i64().unwrap_or(0),
            e["b"].as_i64().unwrap_or(0),
            (e["weight"].as_f64().unwrap_or(0.0) * 100.0).round(),
            shared.join(", "),
        ));
    }
    let q = question.trim();
    if !q.is_empty() {
        out.push_str(&format!("\nNgười dùng quan tâm: {q}\n"));
    }
    out.push_str(
        "\nTrả về ĐÚNG JSON sau, không kèm gì khác:\n\
         {\"summary\":\"2-3 câu tiếng Việt về bức tranh chung\",\
         \"clusters\":[{\"name\":\"tên mạch chuyện ngắn\",\"story_ids\":[1,2],\"why\":\"tối đa 15 từ\"}],\
         \"links\":[{\"a\":1,\"b\":2,\"relation\":\"nguyên nhân|hệ quả|cùng chủ thể|diễn biến tiếp\",\
         \"why\":\"tối đa 15 từ\"}],\
         \"noise\":[{\"a\":3,\"b\":4,\"relation\":\"trùng từ phổ thông\",\"why\":\"tối đa 12 từ\"}]}\n\
         - story_ids / a / b PHẢI là id có trong danh sách trên.\n\
         - clusters: mỗi mạch ít nhất 2 sự kiện; TỐI ĐA 5 mạch.\n\
         - links: CHỈ những cặp liên quan thật mà danh sách liên kết máy CHƯA có; TỐI ĐA 8.\n\
         - noise: TỐI ĐA 3; bỏ trống nếu không có.\n\
         - Viết thật ngắn gọn: JSON phải kết thúc đầy đủ, không được cắt giữa chừng.",
    );
    clip(&out, INPUT_CAP)
}

/// Ask the AI to re-map the graph. Returns the sanitized map + model id.
pub async fn map_graph(
    sc: &SpaceClient,
    nodes: &Value,
    edges: &Value,
    question: &str,
    valid_ids: &std::collections::HashSet<i64>,
) -> Result<(GraphMap, String)> {
    let (text, model) = ask_json(
        sc,
        GRAPH_SYSTEM,
        &graph_prompt(nodes, edges, question),
        budget(4000),
    )
    .await?;
    let json_str = extract_json(&text).ok_or_else(|| {
        // A reply that opens with '{' but never closes it was cut off, which is
        // a different failure from "the model ignored the JSON contract".
        if text.trim_start().starts_with('{') {
            anyhow!("AI trả lời bị cắt giữa chừng — thử lại với ít sự kiện hơn (giảm limit)")
        } else {
            anyhow!("AI không trả về JSON: {}", clip(&text, 200))
        }
    })?;
    let raw: GraphMap =
        serde_json::from_str(json_str).map_err(|e| anyhow!("JSON bản đồ không hợp lệ: {e}"))?;
    Ok((sanitize_map(raw, valid_ids), model))
}

// ---------------------------------------------------------------------------
// Gợi ý nguồn tin (AI suggests feeds; the caller VALIDATES every URL by
// actually fetching it — an unreachable suggestion is dropped, never added)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct CandidateSource {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub category: String,
    #[serde(default)]
    pub lang: String,
}

#[derive(Debug, Deserialize)]
struct SuggestRaw {
    #[serde(default)]
    sources: Vec<CandidateSource>,
}

const SUGGEST_SYSTEM: &str =
    "Bạn là thủ thư tin tức. Người dùng mô tả chủ đề/khu vực họ muốn theo dõi; \
bạn đề xuất các feed RSS/Atom CÓ THẬT của các trang tin uy tín, phù hợp chủ đề đó. \
Chỉ đề xuất URL feed bạn thực sự biết (đường dẫn feed phổ biến của trang lớn); \
KHÔNG bịa đường dẫn lạ. Ưu tiên nguồn đúng ngôn ngữ người dùng dùng trong mô tả. \
Chỉ trả về ĐÚNG một khối JSON, không giải thích.";

pub fn suggest_prompt(query: &str, existing_urls: &[String]) -> String {
    let mut out = format!(
        "Chủ đề cần theo dõi: {}\n\nTrả về JSON đúng cấu trúc:\n\
         {{\"sources\":[{{\"name\":\"tên trang\",\"url\":\"https://…feed RSS/Atom…\",\
         \"category\":\"nhóm ngắn tiếng Việt\",\"lang\":\"vi|en\"}}]}}\n\
         - Tối đa 8 nguồn, mỗi trang một feed phù hợp nhất với chủ đề.\n",
        query.trim()
    );
    if !existing_urls.is_empty() {
        out.push_str("- ĐÃ CÓ các nguồn sau, không đề xuất lại:\n");
        for u in existing_urls.iter().take(40) {
            out.push_str(&format!("  {u}\n"));
        }
    }
    out
}

/// Ask the bridge for candidate feeds. Returns raw, UNVALIDATED candidates.
pub async fn suggest_sources(
    sc: &SpaceClient,
    query: &str,
    existing_urls: &[String],
) -> Result<(Vec<CandidateSource>, String)> {
    let (text, model) = ask_json(
        sc,
        SUGGEST_SYSTEM,
        &suggest_prompt(query, existing_urls),
        budget(1500),
    )
    .await?;
    let json_str =
        extract_json(&text).ok_or_else(|| anyhow!("AI không trả về JSON: {}", clip(&text, 200)))?;
    let raw: SuggestRaw = serde_json::from_str(json_str)
        .map_err(|e| anyhow!("JSON gợi ý nguồn không hợp lệ: {e}"))?;
    let mut out: Vec<CandidateSource> = raw
        .sources
        .into_iter()
        .filter(|s| s.url.starts_with("http://") || s.url.starts_with("https://"))
        .collect();
    out.truncate(8);
    Ok((out, model))
}

// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn extract_json_from_fenced_reply() {
        let reply = "Đây là kết quả:\n```json\n{\"summary\":\"ok\",\"tags\":[\"a{b}\"]}\n```xong";
        let j = extract_json(reply).unwrap();
        let v: Value = serde_json::from_str(j).unwrap();
        assert_eq!(v["summary"], "ok");
    }

    #[test]
    fn extract_json_handles_braces_inside_strings() {
        let reply = r#"{"a":"x } y","b":{"c":1}} trailing"#;
        let j = extract_json(reply).unwrap();
        assert_eq!(j, r#"{"a":"x } y","b":{"c":1}}"#);
    }

    #[test]
    fn extract_json_none_when_absent() {
        assert!(extract_json("không có gì").is_none());
        assert!(extract_json("mở { mà không đóng").is_none());
    }

    #[test]
    fn looks_truncated_flags_mid_thought_endings() {
        // Cut mid-word / mid-clause — what the gateway reported as finish="stop".
        assert!(looks_truncated("Chi tiết: Các cụm từ"));
        assert!(looks_truncated(
            "- **Kết luận:** tin thảm họa đang chiếm sóng,"
        ));
        assert!(looks_truncated(
            "giao thông đô thị bị ảnh hưởng nặng nề bởi mưa bão -"
        ));
        assert!(looks_truncated("   "));
    }

    #[test]
    fn looks_truncated_accepts_complete_replies() {
        assert!(!looks_truncated(
            "Lưu ý: nhận định tham khảo dựa trên tiêu đề đã thu thập."
        ));
        assert!(!looks_truncated("Còn bỏ ngỏ: số người mất tích?"));
        assert!(!looks_truncated("Nguồn nói đây là \"sự cố kỹ thuật\""));
        assert!(!looks_truncated("{\"summary\":\"ok\"}\n"));
        assert!(!looks_truncated("Mức tăng đạt 12%"));
        assert!(!looks_truncated("**Tin chính:**"));
    }

    #[test]
    fn budget_leaves_room_for_hidden_reasoning() {
        // Measured floor: 2000 truncated a 500-word Vietnamese answer, 8000 did not.
        assert!(budget(2000) >= 8000);
        assert_eq!(budget(1200), 7200);
    }

    #[test]
    fn verdict_parses_and_clamps() {
        let raw = r#"{"summary":"s","sentiment":"hồ hởi","importance":9,"clickbait":true,"reliability":"r","tags":["a","b","c","d","e","f","g"]}"#;
        let mut v: ArticleVerdict = serde_json::from_str(raw).unwrap();
        v.importance = v.importance.clamp(1, 5);
        if !["positive", "negative", "neutral", "mixed"].contains(&v.sentiment.as_str()) {
            v.sentiment = "neutral".into();
        }
        v.tags.truncate(5);
        assert_eq!(v.importance, 5);
        assert_eq!(v.sentiment, "neutral");
        assert_eq!(v.tags.len(), 5);
    }

    #[test]
    fn article_prompt_prefers_content_over_description() {
        let a = json!({
            "title": "T", "source_name": "S", "published_at": "2026-07-28T00:00:00Z",
            "description": "mô tả ngắn", "content": "nội dung đầy đủ của bài"
        });
        let p = article_prompt(&a);
        assert!(p.contains("nội dung đầy đủ"));
        let b = json!({ "title": "T", "description": "chỉ có mô tả", "content": "" });
        assert!(article_prompt(&b).contains("chỉ có mô tả"));
    }

    #[test]
    fn story_prompt_reverses_newest_first_timeline_for_narrative() {
        // Stored newest-first (như UI); prompt phải kể xuôi thời gian.
        let story = json!({
            "title": "Bão số 3",
            "timeline": [
                {"published_at": "2026-07-27T02:00:00Z", "source_name": "B", "title": "Sơ tán", "description": "d2"},
                {"published_at": "2026-07-27T01:00:00Z", "source_name": "A", "title": "Bão vào", "description": "d1"}
            ]
        });
        let p = story_prompt(&story);
        let i1 = p.find("Bão vào").unwrap();
        let i2 = p.find("Sơ tán").unwrap();
        assert!(i1 < i2, "sự kiện cũ phải đứng trước trong prompt");
    }

    #[test]
    fn graph_prompt_lists_nodes_edges_and_contract() {
        let nodes = json!([{ "id": 7, "title": "Giá vàng lập đỉnh", "article_count": 3, "first_at": "t1", "last_at": "t2" }]);
        let edges = json!([{ "a": 7, "b": 9, "weight": 0.5, "shared": ["giá vàng"] }]);
        let p = graph_prompt(&nodes, &edges, "kinh tế");
        assert!(p.contains("#7 \"Giá vàng lập đỉnh\" (3 bài"));
        assert!(p.contains("#7 ↔ #9 · trùng 50% · cụm chung: giá vàng"));
        assert!(p.contains("kinh tế"));
        assert!(p.contains("\"clusters\""));
    }

    #[test]
    fn sanitize_map_drops_hallucinated_ids() {
        let raw: GraphMap = serde_json::from_str(
            r#"{"summary":"s",
                "clusters":[{"name":"Vàng","story_ids":[1,2,999],"why":"w"},
                            {"name":"Lẻ","story_ids":[1],"why":"w"},
                            {"name":"","story_ids":[1,2],"why":"w"}],
                "links":[{"a":1,"b":2,"relation":"r","why":"w"},
                         {"a":1,"b":999,"relation":"r","why":"w"},
                         {"a":3,"b":3,"relation":"r","why":"w"}],
                "noise":[{"a":2,"b":3,"relation":"n","why":"w"}]}"#,
        )
        .unwrap();
        let valid: std::collections::HashSet<i64> = [1i64, 2, 3].into_iter().collect();
        let m = sanitize_map(raw, &valid);
        assert_eq!(m.links.len(), 1, "chỉ giữ cặp id hợp lệ, khác nhau");
        assert_eq!((m.links[0].a, m.links[0].b), (1, 2));
        assert_eq!(m.noise.len(), 1);
        assert_eq!(m.clusters.len(), 1, "bỏ cụm 1 phần tử và cụm không tên");
        assert_eq!(
            m.clusters[0].story_ids,
            vec![1, 2],
            "id bịa bị loại khỏi cụm"
        );
    }

    #[test]
    fn suggest_prompt_lists_existing_and_parses_reply() {
        let p = suggest_prompt(
            "tin công nghệ tiếng Việt",
            &["https://vnexpress.net/rss/so-hoa.rss".into()],
        );
        assert!(p.contains("không đề xuất lại"));
        assert!(p.contains("so-hoa.rss"));

        let reply = r#"Đây là gợi ý: {"sources":[
            {"name":"Genk","url":"https://genk.vn/rss/home.rss","category":"Công nghệ","lang":"vi"},
            {"name":"Bịa","url":"ftp://bad","category":"","lang":""}]}"#;
        let raw: SuggestRaw = serde_json::from_str(extract_json(reply).unwrap()).unwrap();
        let valid: Vec<_> = raw
            .sources
            .into_iter()
            .filter(|s| s.url.starts_with("http"))
            .collect();
        assert_eq!(valid.len(), 1);
        assert_eq!(valid[0].name, "Genk");
    }

    #[test]
    fn digest_prompt_includes_focus_and_stories() {
        let arts = vec![json!({"published_at":"t","source_name":"S","title":"Tin 1"})];
        let stories = vec![json!({"title":"Sự kiện X","article_count":3,"last_at":"t"})];
        let p = digest_prompt(&arts, &stories, "công nghệ");
        assert!(p.contains("công nghệ"));
        assert!(p.contains("Sự kiện X"));
        assert!(p.contains("Tin 1"));
    }
}
