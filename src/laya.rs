//! Phân loại bài bằng System One (Laya) — chuyên mục + chủ đề.
//!
//! Laya là model quyết định chạy local qua runtime `sen-sysone`; daemon chuyển
//! tiếp `POST /api/decision/ask` sang nó. App hỏi MỘT câu `choice` cho mỗi bài:
//! "bài này thuộc mục nào?", các lựa chọn là chủ đề của người dùng cộng một bộ
//! chuyên mục dựng sẵn. Mục được chọn là CHUYÊN MỤC của bài.
//!
//! Hình dạng câu hỏi là thứ đã được đo trên dữ liệu thật, không phải sở thích:
//!
//! - **Phải là `choice` giữa các phương án cụ thể.** Hỏi từng chủ đề theo kiểu
//!   có/không (dù là `noul` hay `choice` "chủ đề X / chủ đề khác") thì Laya gần
//!   như luôn nói có: chủ đề "Vàng" dính 45/66 bài, kể cả tin bóng đá. Chỉ khi
//!   chủ đề phải thắng các chuyên mục cụ thể khác thì xác suất mới có nghĩa.
//! - **Bộ chuyên mục dựng sẵn là đối trọng.** Chỉ có chủ đề người dùng + một ô
//!   "Khác" chung chung thì ô "Khác" quá rộng, bài lừa đảo bị đẩy sang "Kinh tế"
//!   với 0.9. Chuyên mục nào trùng tên chủ đề người dùng thì bỏ, để hai phương án
//!   gần như nhau không chia đôi xác suất.
//! - **Chủ đề lồng nhau cần câu hỏi thứ hai.** "Vàng" nằm trong "Kinh tế": một
//!   câu `choice` chỉ chọn được một, và tin giá vàng luôn về "Kinh tế". Khi mục
//!   thắng là một chủ đề người dùng, hỏi lại lần nữa với mục đó bị loại; chủ đề
//!   thắng lần hai chỉ được gán khi rất chắc ([`FOLLOWUP_CONFIDENT`]).
//! - **Laya đã trả lời thì Laya quyết định chủ đề.** Chủ đề khớp từ khóa lúc
//!   nhập bài chỉ là tạm thời, và là đường lui khi System One chưa sẵn sàng.
//!   Giữ lại nó cho những bài Laya chưa chắc thì nhiễu của khớp chuỗi con quay
//!   lại: từ khóa "AI" khớp cả "tài", "hai", "Taiwan" — đo trên 2.040 bài thật,
//!   429/638 thẻ "Công nghệ & AI" đến từ đó.
//!
//! Gom bài thành dòng sự kiện KHÔNG dùng Laya: hỏi "hai bài có cùng sự kiện
//! không" cho ra 0.29 với một cặp đúng là cùng sự kiện và 0.69 với một cặp
//! không phải — không dựa vào đó để nhập dòng sự kiện được.

use std::sync::Mutex;
use std::time::Duration;

use serde_json::{json, Value};

use crate::db::{now_ts, Db};

/// Model đa ngôn ngữ — model duy nhất trong catalog đọc được tiếng Việt.
pub const MODEL: &str = "multilingual";
/// Dưới ngưỡng này bài không có chuyên mục (để trống = chưa rõ) và không được
/// gán chủ đề nào.
pub const SECTION_MIN: f64 = 0.5;
/// Từ ngưỡng này mới hỏi lại để tìm chủ đề lồng bên trong mục vừa thắng.
pub const CONFIDENT: f64 = 0.7;
/// Ngưỡng cho chủ đề thứ hai (câu hỏi lại). Cao hơn vì câu hỏi lại đã loại
/// phương án mạnh nhất, nên phương án gần kề dễ được đẩy lên: "Fitch nâng tín
/// nhiệm EVNNPC" ra "Vàng" 0.78, còn tin giá vàng thật ra 0.95–1.00.
pub const FOLLOWUP_CONFIDENT: f64 = 0.85;
/// Chỉ phân loại bài trong cửa sổ này — không cày lại cả kho lưu trữ.
pub const WINDOW_SECS: i64 = 7 * 86400;
/// Số bài tối đa mỗi lượt quét nền.
pub const BATCH: i64 = 200;
/// Phần mô tả đưa vào câu hỏi, tính theo ký tự.
const DESC_CHARS: usize = 300;

/// Chuyên mục dựng sẵn: (tên, mô tả cho model).
pub const BUILTIN_SECTIONS: &[(&str, &str)] = &[
    ("Thời sự - Chính trị", "Chính trị, nhà nước, đảng, chính sách, hành chính"),
    ("Kinh tế", "Kinh doanh, tài chính, thị trường, doanh nghiệp"),
    ("Thế giới", "Tin quốc tế, ngoại giao, xung đột ở nước ngoài"),
    ("Pháp luật", "Tội phạm, lừa đảo, khởi tố, bắt giữ, xét xử"),
    ("Thể thao", "Bóng đá, giải đấu, vận động viên"),
    ("Giải trí", "Phim, ca nhạc, nghệ sĩ, showbiz, truyền hình"),
    ("Công nghệ", "Công nghệ, khoa học, internet, thiết bị"),
    ("Sức khỏe", "Y tế, bệnh tật, dinh dưỡng"),
    ("Giáo dục", "Trường học, thi cử, học sinh, đại học"),
    (
        "Đời sống - Xã hội",
        "Đời sống, gia đình, nhà ở, tai nạn, cháy nổ, thời tiết, môi trường, giao thông",
    ),
];

/// Một chủ đề người dùng, như model thấy nó.
#[derive(Clone, Debug, PartialEq)]
pub struct TopicDef {
    pub id: i64,
    pub name: String,
    pub keywords: String,
}

/// Một phương án trong câu hỏi `choice`.
#[derive(Clone, Debug, PartialEq)]
pub struct Opt {
    pub label: String,
    pub description: String,
    /// `Some` khi phương án là một chủ đề người dùng.
    pub topic_id: Option<i64>,
}

/// Bộ phương án: chủ đề người dùng trước, rồi các chuyên mục dựng sẵn không
/// trùng tên với chủ đề nào. `exclude` loại một nhãn (dùng cho câu hỏi lại).
pub fn options(topics: &[TopicDef], exclude: Option<&str>) -> Vec<Opt> {
    let mut out: Vec<Opt> = Vec::new();
    let taken = |out: &Vec<Opt>, label: &str| out.iter().any(|o| o.label == label);
    for t in topics {
        let label = t.name.trim();
        if label.is_empty() || Some(label) == exclude || taken(&out, label) {
            continue;
        }
        let kw = t.keywords.trim();
        out.push(Opt {
            label: label.to_string(),
            description: if kw.is_empty() { label.to_string() } else { format!("{label}: {kw}") },
            topic_id: Some(t.id),
        });
    }
    for (name, desc) in BUILTIN_SECTIONS {
        if Some(*name) == exclude || taken(&out, name) || overlaps_topic(name, topics) {
            continue;
        }
        out.push(Opt {
            label: (*name).to_string(),
            description: format!("{name}: {desc}"),
            topic_id: None,
        });
    }
    out
}

/// Chuyên mục dựng sẵn bị bỏ khi một chủ đề người dùng đã nói đúng điều đó:
/// "Công nghệ" nhường "Công nghệ & AI", "Kinh tế" nhường "Kinh tế".
fn overlaps_topic(section: &str, topics: &[TopicDef]) -> bool {
    let s = crate::cluster::normalize(section);
    topics.iter().any(|t| {
        let n = crate::cluster::normalize(&t.name);
        !n.is_empty() && (n.contains(&s) || s.contains(&n))
    })
}

/// Câu trả lời của model cho một câu `choice`.
#[derive(Clone, Debug, PartialEq)]
pub struct Choice {
    pub label: String,
    pub p: f64,
}

/// Kết luận cho một bài.
#[derive(Clone, Debug, PartialEq)]
pub struct Verdict {
    /// Chuyên mục, rỗng khi model không đủ chắc ([`SECTION_MIN`]).
    pub section: String,
    pub conf: f64,
    /// Toàn bộ chủ đề của bài — thay cho chủ đề khớp từ khóa, kể cả khi rỗng.
    pub topics: Vec<i64>,
}

fn topic_of(opts: &[Opt], label: &str) -> Option<i64> {
    opts.iter().find(|o| o.label == label).and_then(|o| o.topic_id)
}

/// Có hỏi lại không: chỉ khi mục thắng là một chủ đề người dùng và đủ chắc.
pub fn wants_followup(first: &Choice, opts: &[Opt]) -> bool {
    first.p >= CONFIDENT && topic_of(opts, &first.label).is_some()
}

/// Gộp câu trả lời thành kết luận. `followup` là câu trả lời cho bộ phương án
/// đã loại `first.label` (`followup_opts`).
pub fn verdict(
    first: &Choice,
    opts: &[Opt],
    followup: Option<(&Choice, &[Opt])>,
) -> Verdict {
    if first.p < SECTION_MIN {
        return Verdict { section: String::new(), conf: first.p, topics: vec![] };
    }
    let mut topics: Vec<i64> = topic_of(opts, &first.label).into_iter().collect();
    if let Some((c, fopts)) = followup.filter(|_| first.p >= CONFIDENT) {
        if c.p >= FOLLOWUP_CONFIDENT {
            if let Some(id) = topic_of(fopts, &c.label) {
                if !topics.contains(&id) {
                    topics.push(id);
                }
            }
        }
    }
    Verdict { section: first.label.clone(), conf: first.p, topics }
}

/// Văn bản đưa cho model: tiêu đề + đầu mô tả.
pub fn state_text(title: &str, description: &str) -> String {
    let desc: String = description.chars().take(DESC_CHARS).collect();
    if desc.trim().is_empty() {
        title.to_string()
    } else {
        format!("{title}\n{desc}")
    }
}

/// Vì sao một lần hỏi không ra kết quả.
#[derive(Debug)]
pub enum AskError {
    /// System One chưa sẵn sàng (chưa cài sen-sysone, chưa tải model, daemon
    /// tắt). Không phải lỗi của bài này — thử lại sau.
    Unavailable(String),
    /// Model trả lời không dùng được cho riêng bài này.
    Failed(String),
}

/// Client gọi System One qua daemon.
#[derive(Clone)]
pub struct Client {
    http: reqwest::Client,
    url: String,
}

impl Client {
    /// `base_url` là địa chỉ daemon (`SENCLAW_BASE_URL`).
    pub fn new(base_url: &str) -> Self {
        use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
        // Cùng danh tính mà SDK đóng lên mọi lời gọi bridge: route này nằm ngoài
        // /api/space/apps/<id>/ nên daemon không bắt buộc, nhưng gửi đúng danh
        // tính thì nhật ký phía daemon mới biết ai hỏi.
        let mut headers = HeaderMap::new();
        if let Some(t) = app_space_sdk::app_token_from_env() {
            if let (Ok(n), Ok(mut v)) = (
                HeaderName::from_bytes(app_space_sdk::HEADER_APP_TOKEN.as_bytes()),
                HeaderValue::from_str(&t),
            ) {
                v.set_sensitive(true);
                headers.insert(n, v);
            }
        }
        if let (Ok(n), Ok(v)) = (
            HeaderName::from_bytes(app_space_sdk::HEADER_API_VERSION.as_bytes()),
            HeaderValue::from_str(&app_space_sdk::api_version_from_env().to_string()),
        ) {
            headers.insert(n, v);
        }
        let http = reqwest::Client::builder()
            .default_headers(headers)
            .timeout(Duration::from_secs(30))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self { http, url: format!("{}/api/decision/ask", base_url.trim_end_matches('/')) }
    }

    /// Hỏi một câu `choice`: "bài này thuộc mục nào?".
    pub async fn choose(&self, state: &str, opts: &[Opt]) -> Result<Choice, AskError> {
        let criteria: serde_json::Map<String, Value> = opts
            .iter()
            .map(|o| (o.label.clone(), Value::String(o.description.clone())))
            .collect();
        let body = json!({
            "model": MODEL,
            "state": state,
            "questions": { "section": {
                "type": "choice",
                "instructions": "Bài báo này thuộc chuyên mục nào?",
                "criteria": criteria,
            }},
        });
        let res = self
            .http
            .post(&self.url)
            .json(&body)
            .send()
            .await
            .map_err(|e| AskError::Unavailable(format!("không gọi được daemon: {e}")))?;
        let status = res.status();
        let text = res.text().await.unwrap_or_default();
        if !status.is_success() {
            let msg = format!("HTTP {}: {}", status.as_u16(), snippet(&text));
            // 503 = runtime chưa cài/chưa chọn/không khởi động được; 409 = model
            // chưa nạp; 404 = daemon cũ chưa có route này. Đều là "chưa sẵn sàng".
            return Err(match status.as_u16() {
                404 | 409 | 502 | 503 | 504 => AskError::Unavailable(msg),
                _ => AskError::Failed(msg),
            });
        }
        parse_choice(&text).ok_or_else(|| AskError::Failed(format!("trả lời lạ: {}", snippet(&text))))
    }
}

fn snippet(s: &str) -> String {
    s.chars().take(200).collect()
}

/// Đọc `answers.section` từ phản hồi `/api/decision/ask`.
pub fn parse_choice(body: &str) -> Option<Choice> {
    let v: Value = serde_json::from_str(body).ok()?;
    let a = &v["answers"]["section"];
    let label = a["choice"].as_str()?.to_string();
    let p = a["probabilities"][&label].as_f64().or_else(|| a["confidence"].as_f64())?;
    Some(Choice { label, p })
}

// ---------------------------------------------------------------------------
// Quét nền
// ---------------------------------------------------------------------------

/// Trạng thái lượt quét gần nhất, cho `/api/status` và màn hình cài đặt.
static LAST: Mutex<Option<Value>> = Mutex::new(None);

fn record(state: &str, classified: usize, error: Option<&str>) {
    if let Ok(mut g) = LAST.lock() {
        *g = Some(json!({
            "state": state,
            "classified": classified,
            "error": error,
            "at": now_ts(),
        }));
    }
}

/// Kết quả một lượt quét.
#[derive(Debug, PartialEq)]
pub enum Sweep {
    /// Đã xử lý hết bài chờ (hoặc hết hạn mức của lượt).
    Done(usize),
    /// System One chưa sẵn sàng — dừng, thử lại lượt sau.
    Unavailable(usize),
}

/// Phân loại tối đa `limit` bài đang chờ trong cửa sổ [`WINDOW_SECS`].
pub async fn classify_pending(db: &Db, client: &Client, limit: i64) -> Sweep {
    let since = now_ts() - WINDOW_SECS;
    let pending = db.pending_sections(since, limit);
    if pending.is_empty() {
        record("ok", 0, None);
        return Sweep::Done(0);
    }
    let topics = db.topic_defs();
    let opts = options(&topics, None);
    let mut done = 0usize;
    for (id, title, desc) in pending {
        // A topic edit mid-batch changes the options and requeues the window;
        // answering the rest with the old options would mark them done with a
        // verdict the requeue meant to discard. Stop; the next pass starts over.
        if db.topic_defs() != topics {
            break;
        }
        let state = state_text(&title, &desc);
        let first = match client.choose(&state, &opts).await {
            Ok(c) => c,
            Err(AskError::Unavailable(e)) => {
                record("unavailable", done, Some(&e));
                return Sweep::Unavailable(done);
            }
            Err(AskError::Failed(e)) => {
                // Lỗi của riêng bài này: đánh dấu đã thử để không hỏi lại mãi,
                // chủ đề khớp từ khóa giữ nguyên.
                eprintln!("[news] laya: bài #{id}: {e}");
                let _ = db.apply_section(id, "", 0.0, None);
                continue;
            }
        };
        let v = if wants_followup(&first, &opts) {
            let fopts = options(&topics, Some(&first.label));
            match client.choose(&state, &fopts).await {
                Ok(c) => verdict(&first, &opts, Some((&c, &fopts))),
                Err(AskError::Unavailable(e)) => {
                    record("unavailable", done, Some(&e));
                    return Sweep::Unavailable(done);
                }
                Err(AskError::Failed(_)) => verdict(&first, &opts, None),
            }
        } else {
            verdict(&first, &opts, None)
        };
        if db.apply_section(id, &v.section, v.conf, Some(&v.topics)).is_ok() {
            done += 1;
        }
    }
    record("ok", done, None);
    Sweep::Done(done)
}

/// Trạng thái cho UI/agent: bật/tắt, lượt gần nhất, tiến độ trong cửa sổ.
pub fn status_json(db: &Db) -> Value {
    let (classified, pending) = db.section_progress(now_ts() - WINDOW_SECS);
    json!({
        "enabled": enabled(db),
        "model": MODEL,
        "window_days": WINDOW_SECS / 86400,
        "classified": classified,
        "pending": pending,
        "last": LAST.lock().ok().and_then(|g| g.clone()),
    })
}

pub fn enabled(db: &Db) -> bool {
    db.setting("laya_classify", "1") == "1"
}

#[cfg(test)]
mod tests {
    use super::*;

    fn topics() -> Vec<TopicDef> {
        vec![
            TopicDef { id: 1, name: "Công nghệ & AI".into(), keywords: "AI, chip".into() },
            TopicDef { id: 2, name: "Kinh tế".into(), keywords: "lạm phát".into() },
            TopicDef { id: 4, name: "Vàng".into(), keywords: "giá vàng".into() },
        ]
    }

    fn labels(o: &[Opt]) -> Vec<&str> {
        o.iter().map(|o| o.label.as_str()).collect()
    }

    #[test]
    fn user_topics_come_first_and_shadow_overlapping_sections() {
        let o = options(&topics(), None);
        let l = labels(&o);
        assert_eq!(&l[..3], ["Công nghệ & AI", "Kinh tế", "Vàng"]);
        // "Công nghệ" yields to "Công nghệ & AI", "Kinh tế" to the user's own.
        assert!(!l.contains(&"Công nghệ"));
        assert_eq!(l.iter().filter(|x| **x == "Kinh tế").count(), 1);
        assert!(l.contains(&"Pháp luật") && l.contains(&"Thể thao"));
        assert_eq!(o[0].description, "Công nghệ & AI: AI, chip");
        assert_eq!(o[0].topic_id, Some(1));
        assert_eq!(o.iter().find(|x| x.label == "Pháp luật").unwrap().topic_id, None);
    }

    #[test]
    fn excluding_the_winner_removes_it_from_the_followup() {
        let o = options(&topics(), Some("Kinh tế"));
        let l = labels(&o);
        assert!(!l.contains(&"Kinh tế"));
        // The built-in "Kinh tế" must not sneak back in as a stand-in.
        assert!(l.contains(&"Vàng"));
    }

    #[test]
    fn no_topics_means_only_builtin_sections() {
        let o = options(&[], None);
        assert_eq!(o.len(), BUILTIN_SECTIONS.len());
        assert!(o.iter().all(|x| x.topic_id.is_none()));
    }

    #[test]
    fn duplicate_topic_names_become_one_option() {
        let mut t = topics();
        t.push(TopicDef { id: 9, name: "Vàng".into(), keywords: "".into() });
        let o = options(&t, None);
        assert_eq!(labels(&o).iter().filter(|x| **x == "Vàng").count(), 1);
    }

    #[test]
    fn a_lukewarm_answer_still_decides_the_topics() {
        let o = options(&topics(), None);
        // Above SECTION_MIN: the section is kept, and being a user topic, tagged.
        let v = verdict(&Choice { label: "Kinh tế".into(), p: 0.62 }, &o, None);
        assert_eq!((v.section.as_str(), v.topics), ("Kinh tế", vec![2]));
        // Below it: no section and no topics — keyword tags do not survive.
        let v = verdict(&Choice { label: "Công nghệ & AI".into(), p: 0.4 }, &o, None);
        assert_eq!((v.section.as_str(), v.topics), ("", vec![]));
    }

    #[test]
    fn no_followup_topic_below_the_confidence_gate() {
        let o = options(&topics(), None);
        let first = Choice { label: "Kinh tế".into(), p: 0.62 };
        assert!(!wants_followup(&first, &o));
        let f = options(&topics(), Some("Kinh tế"));
        let gold = Choice { label: "Vàng".into(), p: 0.99 };
        assert_eq!(verdict(&first, &o, Some((&gold, &f))).topics, vec![2]);
    }

    #[test]
    fn a_confident_builtin_section_clears_keyword_topics() {
        let o = options(&topics(), None);
        let first = Choice { label: "Pháp luật".into(), p: 0.9 };
        assert!(!wants_followup(&first, &o));
        let v = verdict(&first, &o, None);
        assert_eq!(v.section, "Pháp luật");
        assert!(v.topics.is_empty());
    }

    #[test]
    fn a_nested_topic_is_added_only_when_the_followup_is_sure() {
        let o = options(&topics(), None);
        let first = Choice { label: "Kinh tế".into(), p: 0.95 };
        assert!(wants_followup(&first, &o));
        let f = options(&topics(), Some("Kinh tế"));
        let gold = Choice { label: "Vàng".into(), p: 0.97 };
        let v = verdict(&first, &o, Some((&gold, &f)));
        assert_eq!(v.topics, vec![2, 4]);
        // "Fitch nâng tín nhiệm" came back "Vàng" 0.78 — not enough.
        let weak = Choice { label: "Vàng".into(), p: 0.78 };
        assert_eq!(verdict(&first, &o, Some((&weak, &f))).topics, vec![2]);
        // A sure follow-up that lands on a built-in section adds nothing.
        let sec = Choice { label: "Pháp luật".into(), p: 0.95 };
        assert_eq!(verdict(&first, &o, Some((&sec, &f))).topics, vec![2]);
    }

    #[test]
    fn parses_the_decision_wire() {
        let body = r#"{"model":"multilingual","answers":{"section":{"type":"choice",
            "choice":"Thể thao","probabilities":{"Thể thao":0.98,"Kinh tế":0.02},"confidence":0.98}}}"#;
        assert_eq!(
            parse_choice(body),
            Some(Choice { label: "Thể thao".into(), p: 0.98 })
        );
        assert_eq!(parse_choice(r#"{"error":"model not loaded"}"#), None);
    }

    #[test]
    fn state_text_trims_the_description() {
        let long = "x".repeat(1000);
        let s = state_text("Tiêu đề", &long);
        assert!(s.starts_with("Tiêu đề\n"));
        assert_eq!(s.chars().count(), "Tiêu đề\n".chars().count() + DESC_CHARS);
        assert_eq!(state_text("Tiêu đề", "  "), "Tiêu đề");
    }

    #[tokio::test]
    async fn an_unreachable_daemon_is_unavailable_not_a_failure() {
        let c = Client::new("http://127.0.0.1:1");
        let o = options(&topics(), None);
        assert!(matches!(c.choose("x", &o).await, Err(AskError::Unavailable(_))));
    }
}
