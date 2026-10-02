//! MCP server (HTTP + SSE) exposing the news archive to SenClaw agents and
//! other platforms. Tool prefix `news_` (registered as `news-mcp` → full names
//! `mcp__news-mcp__news_*`); every tool calls the SAME `crate::api::*_value`
//! helpers the REST UI uses, so agents and humans see identical behavior.
//! Read-mostly: the only side effects are managing sources/topics, triggering
//! a fetch, and caching AI analyses — nothing is ever posted anywhere.

use axum::{
    extract::State,
    response::sse::{Event, Sse},
    Json,
};
use futures_util::stream::Stream;
use serde::Deserialize;
use serde_json::{json, Value};
use std::convert::Infallible;

use crate::api::{self, AppState};

#[derive(Deserialize, Debug)]
pub struct JsonRpcRequest {
    #[allow(dead_code)]
    pub jsonrpc: String,
    pub id: Option<Value>,
    pub method: String,
    pub params: Option<Value>,
}

pub async fn mcp_sse(
    State(state): State<AppState>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let mut rx = state.mcp_tx.subscribe();
    let stream = async_stream::stream! {
        yield Ok(Event::default().event("endpoint").data("/api/mcp/message".to_string()));
        while let Ok(msg) = rx.recv().await {
            yield Ok(Event::default().event("message").data(msg));
        }
    };
    Sse::new(stream).keep_alive(axum::response::sse::KeepAlive::default())
}

fn text_result(text: String) -> Value {
    json!({ "content": [{ "type": "text", "text": text }] })
}
fn json_result(v: &Value) -> Value {
    text_result(serde_json::to_string_pretty(v).unwrap_or_default())
}
fn error_result(text: String) -> Value {
    json!({ "isError": true, "content": [{ "type": "text", "text": text }] })
}

pub async fn mcp_message(
    State(state): State<AppState>,
    Json(req): Json<JsonRpcRequest>,
) -> Json<Value> {
    let reply = |result: Value| -> Json<Value> {
        let resp = json!({ "jsonrpc": "2.0", "id": req.id, "result": result });
        let _ = state.mcp_tx.send(resp.to_string());
        Json(resp)
    };
    match req.method.as_str() {
        "initialize" => reply(json!({
            "protocolVersion": "2024-11-05",
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "news-mcp", "version": "1.0.0" }
        })),
        "ping" => reply(json!({})),
        "notifications/initialized" => {
            Json(json!({ "jsonrpc": "2.0", "id": req.id, "result": {} }))
        }
        "tools/list" => reply(json!({ "tools": tools_list() })),
        "tools/call" => {
            let params = req.params.clone().unwrap_or_default();
            let name = params["name"].as_str().unwrap_or("").to_string();
            let args = params["arguments"].clone();
            reply(call_tool(&state, &name, &args).await)
        }
        _ => Json(json!("ok")),
    }
}

fn tools_list() -> Value {
    json!([
        {
            "name": "news_status",
            "description": "Trạng thái nhanh của app Tin Tức: tổng số bài, bài 24h qua, số nguồn đang hoạt động / đang lỗi, lần thu thập gần nhất.",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "news_dashboard",
            "description": "Toàn cảnh tin tức: số bài theo ngày (14 ngày), chủ đề nhiều bài nhất (7 ngày), cụm từ đang tăng nhiệt (48h), dòng sự kiện nóng, bài mới nhất. Dùng tool này TRƯỚC khi trả lời câu hỏi tổng quan kiểu 'dạo này có tin gì'.",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "news_fetch",
            "description": "Thu thập tin NGAY từ các nguồn RSS/Atom đang hoạt động (source_id để quét một nguồn duy nhất). Trả về số bài mới, số nguồn lỗi. Bài mới tự động được gán chủ đề và gom vào dòng sự kiện.",
            "inputSchema": { "type": "object", "properties": {
                "source_id": { "type": "number", "description": "Chỉ quét nguồn này; bỏ trống = quét tất cả." }
            } }
        },
        {
            "name": "news_search",
            "description": "Tìm bài trong kho tin đã thu thập (FTS — gõ không dấu vẫn khớp, ví dụ 'gia vang' tìm được 'giá vàng'). Lọc thêm: source_id, topic_id, story_id, category (chuyên mục feed tự khai), section (chuyên mục System One phân loại), hours (chỉ bài trong N giờ), limit/offset.",
            "inputSchema": { "type": "object", "properties": {
                "q":         { "type": "string", "description": "Từ khóa tìm kiếm." },
                "source_id": { "type": "number" },
                "topic_id":  { "type": "number" },
                "story_id":  { "type": "number" },
                "category":  { "type": "string" },
                "section":   { "type": "string", "description": "Chuyên mục do System One (Laya) phân loại, đúng tên như news_sections trả về (vd 'Pháp luật', 'Thể thao')." },
                "hours":     { "type": "number", "description": "Chỉ bài đăng trong N giờ gần đây." },
                "limit":     { "type": "number", "description": "Mặc định 50, tối đa 500." },
                "offset":    { "type": "number" }
            } }
        },
        {
            "name": "news_latest",
            "description": "Bài mới nhất (mặc định 24h / 20 bài). Lọc theo topic_id, source_id, category hoặc section (chuyên mục System One phân loại). Là cách nhanh nhất để lấy tin cho nền tảng khác (đăng bản tin, trả lời chat…).",
            "inputSchema": { "type": "object", "properties": {
                "hours":     { "type": "number", "description": "Cửa sổ thời gian, mặc định 24." },
                "topic_id":  { "type": "number" },
                "source_id": { "type": "number" },
                "category":  { "type": "string" },
                "section":   { "type": "string", "description": "Chuyên mục do System One (Laya) phân loại, đúng tên như news_sections trả về (vd 'Pháp luật', 'Thể thao')." },
                "limit":     { "type": "number", "description": "Mặc định 20." }
            } }
        },
        {
            "name": "news_article_get",
            "description": "Chi tiết một bài: đầy đủ mô tả, nội dung (nếu đã tải toàn văn), chủ đề, dòng sự kiện, tin liên quan cùng sự kiện, và kết quả AI đánh giá (nếu đã chạy).",
            "inputSchema": { "type": "object", "properties": {
                "article_id": { "type": "number" }
            }, "required": ["article_id"] }
        },
        {
            "name": "news_article_content",
            "description": "Tải TOÀN VĂN bài báo từ trang gốc (trích xuất phần nội dung chính, bỏ menu/quảng cáo) và lưu lại. Dùng trước khi phân tích sâu một bài chỉ có mô tả ngắn. Trang cần JavaScript có thể không trích được — khi đó tool trả lỗi rõ ràng.",
            "inputSchema": { "type": "object", "properties": {
                "article_id": { "type": "number" }
            }, "required": ["article_id"] }
        },
        {
            "name": "news_analyze_article",
            "description": "AI đánh giá MỘT bài (qua bridge SenClaw): tóm tắt, cảm xúc (positive/negative/neutral/mixed), tầm quan trọng 1-5, có giật tít không, nhận xét độ tin cậy, tags. Kết quả được cache — force=true để chấm lại. with_content=true sẽ tải toàn văn trước khi đánh giá.",
            "inputSchema": { "type": "object", "properties": {
                "article_id":   { "type": "number" },
                "force":        { "type": "boolean" },
                "with_content": { "type": "boolean" }
            }, "required": ["article_id"] }
        },
        {
            "name": "news_trends",
            "description": "Cụm từ đang TĂNG NHIỆT trong tiêu đề tin: so số bài chứa cụm từ trong N giờ (mặc định 48) với N giờ liền trước, kèm 3 bài mẫu mỗi cụm. Thuần thống kê, máy đếm — không phải AI đoán.",
            "inputSchema": { "type": "object", "properties": {
                "hours": { "type": "number", "description": "Cửa sổ so sánh, 6–336, mặc định 48." }
            } }
        },
        {
            "name": "news_analyze_trends",
            "description": "AI nhận định các xu hướng đang tăng nhiệt (chạy news_trends trước rồi thuê AI diễn giải: chuyện gì, vì sao nóng, cần theo dõi gì). Luôn kèm lưu ý 'nhận định tham khảo'.",
            "inputSchema": { "type": "object", "properties": {
                "hours": { "type": "number", "description": "Cửa sổ phân tích, mặc định 48." }
            } }
        },
        {
            "name": "news_trend_summary",
            "description": "AI tóm tắt MỘT xu hướng: đọc các bài thuộc cụm từ đó rồi nói chuyện gì, diễn biến chính (kèm nguồn) và cần theo dõi tiếp gì. Lấy `phrase` đúng như news_trends trả về, cùng `hours` đã dùng ở đó.",
            "inputSchema": { "type": "object", "properties": {
                "phrase": { "type": "string", "description": "Cụm từ xu hướng, đúng như news_trends trả về." },
                "hours": { "type": "number", "description": "Cửa sổ đã dùng khi lấy xu hướng, mặc định 48." }
            }, "required": ["phrase"] }
        },
        {
            "name": "news_story_list",
            "description": "Các DÒNG SỰ KIỆN (chuỗi bài về cùng một sự kiện, gom tự động từ nhiều nguồn) trong N ngày gần đây, xếp theo độ nóng (nhiều bài nhất trước). min_articles mặc định 2 để ẩn 'sự kiện' chỉ có một bài.",
            "inputSchema": { "type": "object", "properties": {
                "days":         { "type": "number", "description": "Mặc định 7." },
                "min_articles": { "type": "number", "description": "Mặc định 2." },
                "limit":        { "type": "number", "description": "Mặc định 30." }
            } }
        },
        {
            "name": "news_story_get",
            "description": "TIMELINE một dòng sự kiện: các bài xếp theo thời gian (diễn biến từ đầu đến mới nhất, kèm nguồn), cùng bản tóm tắt AI nếu đã tạo. Đây là dữ liệu cho hiển thị timeline ở nền tảng khác.",
            "inputSchema": { "type": "object", "properties": {
                "story_id": { "type": "number" }
            }, "required": ["story_id"] }
        },
        {
            "name": "news_story_graph",
            "description": "BIỂU ĐỒ LIÊN KẾT giữa các dòng sự kiện: nodes = dòng sự kiện (kèm số bài, thời gian), edges = hai sự kiện cùng mạch chuyện (trùng từ khóa chủ chốt, kèm danh sách từ chung và trọng số). Dùng để trả lời 'các sự kiện đang liên quan nhau thế nào' hoặc dựng bản đồ tin cho nền tảng khác. Thuần thống kê, không phải AI đoán.",
            "inputSchema": { "type": "object", "properties": {
                "days":         { "type": "number", "description": "Cửa sổ thời gian, mặc định 7 ngày." },
                "min_articles": { "type": "number", "description": "Chỉ lấy sự kiện có ≥ N bài, mặc định 2." },
                "limit":        { "type": "number", "description": "Tối đa số node, mặc định 60." },
                "links_per_story": { "type": "number", "description": "Chỉ giữ N liên kết mạnh nhất của mỗi sự kiện (mặc định 3) để bản đồ đọc được; đặt 0 để lấy TOÀN BỘ liên kết khi cần phân tích đầy đủ. Kết quả kèm edges_total/edges_hidden." }
            } }
        },
        {
            "name": "news_analyze_graph",
            "description": "AI ĐỌC BẢN ĐỒ liên kết sự kiện rồi map lại theo ngữ nghĩa: gom các sự kiện cùng một câu chuyện thành 'mạch chuyện' có tên, NỐI THÊM cặp sự kiện liên quan mà máy bỏ sót (nguyên nhân → hệ quả, cùng chủ thể…), và chỉ ra liên kết máy nối nhầm vì chỉ trùng từ phổ thông. Trả về summary + clusters + ai_links + noise (mọi id đã được kiểm tra có thật) kèm nguyên bản graph thống kê. Dùng khi được hỏi 'các sự kiện đang liên quan nhau thế nào'.",
            "inputSchema": { "type": "object", "properties": {
                "days":         { "type": "number", "description": "Cửa sổ thời gian, mặc định 7 ngày." },
                "min_articles": { "type": "number", "description": "Chỉ lấy sự kiện có ≥ N bài, mặc định 2." },
                "limit":        { "type": "number", "description": "Tối đa số sự kiện đưa vào phân tích, mặc định 40." },
                "question":     { "type": "string", "description": "Trọng tâm quan tâm, tuỳ chọn." }
            } }
        },
        {
            "name": "news_source_discover",
            "description": "TỰ TÌM nguồn tin mới: query là CHỦ ĐỀ ('tin công nghệ tiếng Việt' — AI gợi ý feed của các trang uy tín) hoặc URL MỘT TRANG WEB (tự dò feed RSS/Atom qua thẻ <link> + các đường dẫn phổ biến). Mọi gợi ý đều được TẢI THỬ THẬT — chỉ nguồn parse được mới tính là hợp lệ (kèm tên feed, số bài, tiêu đề mẫu). auto_add=true để thêm luôn các nguồn hợp lệ; mặc định chỉ trả danh sách để chọn.",
            "inputSchema": { "type": "object", "properties": {
                "query":    { "type": "string", "description": "Chủ đề muốn theo dõi HOẶC URL trang web." },
                "auto_add": { "type": "boolean", "description": "true = thêm luôn nguồn hợp lệ. Mặc định false." }
            }, "required": ["query"] }
        },
        {
            "name": "news_story_brief",
            "description": "AI tóm tắt DIỄN BIẾN một dòng sự kiện theo timeline (tổng thể → diễn biến theo mốc thời gian → điểm còn bỏ ngỏ). Kết quả cache trong story và tự hết hạn khi có bài mới; force=true để viết lại.",
            "inputSchema": { "type": "object", "properties": {
                "story_id": { "type": "number" },
                "force":    { "type": "boolean" }
            }, "required": ["story_id"] }
        },
        {
            "name": "news_story_translate",
            "description": "Dịch tiêu đề + mô tả của mọi bài trong một dòng sự kiện sang NGÔN NGỮ HIỂN THỊ đã đặt trong cài đặt. Bản dịch được cache theo từng ngôn ngữ nên gọi lại không tốn thêm; bản gốc luôn giữ nguyên. Dùng khi dòng sự kiện có nguồn tiếng nước ngoài.",
            "inputSchema": { "type": "object", "properties": {
                "story_id": { "type": "number" }
            }, "required": ["story_id"] }
        },
        {
            "name": "news_stories_rebuild",
            "description": "Gom lại TOÀN BỘ kho bài thành dòng sự kiện bằng thuật toán hiện tại (bỏ hết cách gom cũ, kể cả lịch sử tóm tắt của các dòng cũ). Dùng khi thấy dòng sự kiện lẫn bài không liên quan. Chạy vài giây trên kho hai chục nghìn bài; bình thường app tự chạy theo chu kỳ nên chỉ gọi khi cần ngay.",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "news_digest",
            "description": "AI viết BẢN ĐIỂM TIN từ các bài trong N giờ (mặc định 24): Tin chính / Đáng chú ý / Xu hướng, ưu tiên sự kiện nhiều nguồn đưa. focus = trọng tâm người đọc ('công nghệ', 'kinh tế'…), topic_id để giới hạn một chủ đề. Dùng khi được nhờ 'điểm tin hôm nay'.",
            "inputSchema": { "type": "object", "properties": {
                "hours":    { "type": "number", "description": "Mặc định 24, tối đa 168." },
                "focus":    { "type": "string" },
                "topic_id": { "type": "number" }
            } }
        },
        {
            "name": "news_digest_history",
            "description": "LỊCH SỬ các bản điểm tin đã chạy (50 bản gần nhất, mới trước): thời điểm, khoảng thời gian, chủ đề/trọng tâm, số bài, model và đoạn mở đầu. Có digest_id → trả nguyên văn bản điểm tin đó. Dùng khi người dùng hỏi 'bản điểm tin lúc sáng', 'xem lại điểm tin hôm qua' — đọc lại rẻ hơn và ổn định hơn là bắt AI viết lại.",
            "inputSchema": { "type": "object", "properties": {
                "digest_id": { "type": "number", "description": "Xem nguyên văn một bản; bỏ trống = liệt kê." },
                "limit":     { "type": "number", "description": "Số bản liệt kê, mặc định 30, tối đa 50." }
            } }
        },
        {
            "name": "news_source_add",
            "description": "Thêm một nguồn tin. kind='feed' (mặc định): url là feed RSS/Atom. kind='scrape': url là một trang chuyên mục/danh sách bình thường của trang KHÔNG có RSS — app sẽ quét link bài viết ngay trong HTML rồi mở từng bài mới để lấy tiêu đề/tóm tắt/ngày đăng. category để nhóm nguồn ('Công nghệ', 'Kinh doanh'…), lang ('vi'/'en').",
            "inputSchema": { "type": "object", "properties": {
                "name":     { "type": "string" },
                "url":      { "type": "string", "description": "URL feed RSS/Atom (kind=feed) hoặc trang chuyên mục cần quét (kind=scrape), bắt đầu bằng http(s)://." },
                "kind":     { "type": "string", "enum": ["feed","scrape"], "description": "feed = RSS/Atom (mặc định); scrape = quét link bài viết từ nội dung trang." },
                "category": { "type": "string" },
                "lang":     { "type": "string" },
                "note":     { "type": "string" }
            }, "required": ["url"] }
        },
        {
            "name": "news_source_list",
            "description": "Liệt kê nguồn tin kèm sức khỏe: lần quét gần nhất, trạng thái ok/error + thông báo lỗi, số bài đã thu thập. Lọc theo status: active|paused.",
            "inputSchema": { "type": "object", "properties": {
                "status": { "type": "string", "enum": ["active","paused"] }
            } }
        },
        {
            "name": "news_source_update",
            "description": "Sửa nguồn tin (patch — chỉ trường truyền vào mới đổi). Tạm dừng quét bằng status='paused', bật lại bằng 'active'.",
            "inputSchema": { "type": "object", "properties": {
                "source_id": { "type": "number" },
                "name":      { "type": "string" },
                "url":       { "type": "string" },
                "category":  { "type": "string" },
                "lang":      { "type": "string" },
                "status":    { "type": "string", "enum": ["active","paused"] },
                "note":      { "type": "string" }
            }, "required": ["source_id"] }
        },
        {
            "name": "news_source_delete",
            "description": "Xoá một nguồn tin VÀ toàn bộ bài đã thu thập từ nguồn đó (không khôi phục được). Muốn giữ bài cũ mà ngừng quét thì dùng news_source_update status='paused' thay vì xoá.",
            "inputSchema": { "type": "object", "properties": {
                "source_id": { "type": "number" }
            }, "required": ["source_id"] }
        },
        {
            "name": "news_topic_add",
            "description": "Thêm CHỦ ĐỀ theo dõi: keywords là danh sách từ khóa cách nhau dấu phẩy ('AI, trí tuệ nhân tạo, chip'). Bài chứa từ khóa trong tiêu đề/mô tả sẽ tự gán vào chủ đề, kể cả bài đã thu thập 30 ngày gần đây (backfill).",
            "inputSchema": { "type": "object", "properties": {
                "name":     { "type": "string" },
                "keywords": { "type": "string" },
                "color":    { "type": "string", "description": "Màu hiển thị (blue/gold/green/red/purple…), tuỳ chọn." }
            }, "required": ["name"] }
        },
        {
            "name": "news_sections",
            "description": "Các chuyên mục mà System One (Laya, model quyết định chạy local) đã phân loại bài vào, kèm số bài, trong N giờ (mặc định 168). Kèm trạng thái bộ phân loại: bật/tắt, số bài đã/đang chờ phân loại, lỗi gần nhất (vd chưa cài sen-sysone). Dùng tên chuyên mục làm tham số `section` cho news_latest/news_search.",
            "inputSchema": { "type": "object", "properties": {
                "hours": { "type": "number", "description": "Cửa sổ thời gian, mặc định 168 (7 ngày)." }
            } }
        },
        {
            "name": "news_topic_list",
            "description": "Liệt kê các chủ đề đang theo dõi kèm số bài đã gán.",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "news_topic_update",
            "description": "Sửa chủ đề (patch). Đổi keywords sẽ tự tính lại bài khớp trong 30 ngày gần đây.",
            "inputSchema": { "type": "object", "properties": {
                "topic_id": { "type": "number" },
                "name":     { "type": "string" },
                "keywords": { "type": "string" },
                "color":    { "type": "string" }
            }, "required": ["topic_id"] }
        },
        {
            "name": "news_topic_delete",
            "description": "Xoá một chủ đề (bài viết vẫn giữ nguyên, chỉ mất nhãn gán).",
            "inputSchema": { "type": "object", "properties": {
                "topic_id": { "type": "number" }
            }, "required": ["topic_id"] }
        },
        {
            "name": "news_activity",
            "description": "Nhật ký hoạt động gần đây của app (thu thập, thêm nguồn/chủ đề, các lần chạy AI).",
            "inputSchema": { "type": "object", "properties": {} }
        }
    ])
}

async fn call_tool(s: &AppState, name: &str, args: &Value) -> Value {
    let i64_arg = |k: &str| args.get(k).and_then(|x| x.as_i64());
    let str_arg = |k: &str| {
        args.get(k)
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string()
    };
    let opt_str = |k: &str| args.get(k).and_then(|x| x.as_str()).map(|v| v.to_string());
    let bool_arg = |k: &str| args.get(k).and_then(|x| x.as_bool()).unwrap_or(false);
    match name {
        "news_status" => json_result(&api::status_value(s)),
        "news_dashboard" => json_result(&api::dashboard_value(s)),
        "news_fetch" => match i64_arg("source_id") {
            Some(id) => json_result(&api::fetch_source_value(s, id).await),
            None => json_result(&api::fetch_all_value(s).await),
        },
        "news_search" | "news_latest" => {
            let q = api::ArticleQuery {
                q: opt_str("q"),
                source_id: i64_arg("source_id"),
                topic_id: i64_arg("topic_id"),
                story_id: i64_arg("story_id"),
                category: opt_str("category"),
                section: opt_str("section"),
                hours: i64_arg("hours").or(if name == "news_latest" {
                    Some(24)
                } else {
                    None
                }),
                limit: i64_arg("limit").or(if name == "news_latest" {
                    Some(20)
                } else {
                    None
                }),
                offset: i64_arg("offset"),
            };
            json_result(&api::list_articles_value(s, &q))
        }
        "news_article_get" => {
            let Some(id) = i64_arg("article_id") else {
                return error_result("thiếu 'article_id'".into());
            };
            json_result(&api::get_article_value(s, id))
        }
        "news_article_content" => {
            let Some(id) = i64_arg("article_id") else {
                return error_result("thiếu 'article_id'".into());
            };
            json_result(&api::fetch_content_value(s, id).await)
        }
        "news_analyze_article" => {
            let Some(id) = i64_arg("article_id") else {
                return error_result("thiếu 'article_id'".into());
            };
            let b = api::AnalyzeIn {
                force: bool_arg("force"),
                with_content: bool_arg("with_content"),
            };
            json_result(&api::analyze_article_value(s, id, &b).await)
        }
        "news_trends" => json_result(&api::trends_value(s, i64_arg("hours").unwrap_or(48))),
        "news_analyze_trends" => {
            json_result(&api::analyze_trends_value(s, i64_arg("hours").unwrap_or(48)).await)
        }
        "news_trend_summary" => json_result(
            &api::trend_summary_value(s, &str_arg("phrase"), i64_arg("hours").unwrap_or(48)).await,
        ),
        "news_story_list" => json_result(&api::list_stories_value(
            s,
            i64_arg("days").unwrap_or(7),
            i64_arg("min_articles").unwrap_or(2),
            i64_arg("limit").unwrap_or(30),
        )),
        "news_story_get" => {
            let Some(id) = i64_arg("story_id") else {
                return error_result("thiếu 'story_id'".into());
            };
            json_result(&api::get_story_value(s, id))
        }
        "news_story_graph" => json_result(&api::story_graph_value(
            s,
            i64_arg("days").unwrap_or(7),
            i64_arg("min_articles").unwrap_or(2),
            i64_arg("limit").unwrap_or(60),
            i64_arg("links_per_story").unwrap_or(3).clamp(0, 20) as usize,
        )),
        "news_analyze_graph" => {
            let b = api::GraphAnalyzeIn {
                days: i64_arg("days"),
                min_articles: i64_arg("min_articles"),
                limit: i64_arg("limit"),
                question: str_arg("question"),
            };
            json_result(&api::analyze_graph_value(s, &b).await)
        }
        "news_source_discover" => {
            let q = str_arg("query");
            if q.is_empty() {
                return error_result("thiếu 'query'".into());
            }
            let b = api::DiscoverIn {
                query: q,
                auto_add: bool_arg("auto_add"),
            };
            json_result(&api::discover_sources_value(s, &b).await)
        }
        "news_story_brief" => {
            let Some(id) = i64_arg("story_id") else {
                return error_result("thiếu 'story_id'".into());
            };
            json_result(&api::story_brief_value(s, id, bool_arg("force")).await)
        }
        "news_story_translate" => {
            let Some(id) = i64_arg("story_id") else {
                return error_result("thiếu 'story_id'".into());
            };
            json_result(&api::translate_story_value(s, id).await)
        }
        "news_stories_rebuild" => json_result(&api::rebuild_stories_value(s)),
        "news_digest" => {
            let b = api::DigestIn {
                hours: i64_arg("hours"),
                focus: str_arg("focus"),
                topic_id: i64_arg("topic_id"),
            };
            json_result(&api::digest_value(s, &b).await)
        }
        "news_digest_history" => match i64_arg("digest_id") {
            Some(id) => json_result(&api::get_digest_value(s, id)),
            None => json_result(&api::digest_history_value(s, i64_arg("limit").unwrap_or(30))),
        },
        "news_source_add" => {
            let b = api::SourceIn {
                name: str_arg("name"),
                url: str_arg("url"),
                category: str_arg("category"),
                lang: str_arg("lang"),
                note: str_arg("note"),
                kind: str_arg("kind"),
            };
            if b.url.is_empty() {
                return error_result("thiếu 'url'".into());
            }
            json_result(&api::add_source_value(s, &b))
        }
        "news_source_list" => {
            let st = opt_str("status");
            json_result(&api::list_sources_value(s, st.as_deref()))
        }
        "news_source_update" => {
            let Some(id) = i64_arg("source_id") else {
                return error_result("thiếu 'source_id'".into());
            };
            json_result(&api::update_source_value(s, id, args))
        }
        "news_source_delete" => {
            let Some(id) = i64_arg("source_id") else {
                return error_result("thiếu 'source_id'".into());
            };
            json_result(&api::delete_source_value(s, id))
        }
        "news_topic_add" => {
            let b = api::TopicIn {
                name: str_arg("name"),
                keywords: str_arg("keywords"),
                color: str_arg("color"),
            };
            if b.name.is_empty() {
                return error_result("thiếu 'name'".into());
            }
            json_result(&api::add_topic_value(s, &b))
        }
        "news_topic_list" => json_result(&api::list_topics_value(s)),
        "news_sections" => json_result(&api::sections_value(s, i64_arg("hours"))),
        "news_topic_update" => {
            let Some(id) = i64_arg("topic_id") else {
                return error_result("thiếu 'topic_id'".into());
            };
            json_result(&api::update_topic_value(s, id, args))
        }
        "news_topic_delete" => {
            let Some(id) = i64_arg("topic_id") else {
                return error_result("thiếu 'topic_id'".into());
            };
            json_result(&api::delete_topic_value(s, id))
        }
        "news_activity" => json_result(&json!({ "activity": s.db.recent_activity(50) })),
        other => error_result(format!("tool không tồn tại: {other}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tools_have_unique_prefixed_names() {
        let tools = tools_list();
        let names: Vec<&str> = tools
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert_eq!(names.len(), 31);
        let unique: std::collections::HashSet<_> = names.iter().collect();
        assert_eq!(unique.len(), names.len(), "tool names must be unique");
        assert!(
            names.iter().all(|n| n.starts_with("news_")),
            "all tools use the news_ prefix"
        );
    }

    #[test]
    fn every_tool_has_schema_and_description() {
        for t in tools_list().as_array().unwrap() {
            assert!(
                t["description"].as_str().unwrap().len() > 20,
                "{} needs a real description",
                t["name"]
            );
            assert_eq!(t["inputSchema"]["type"], "object");
        }
    }

    #[test]
    fn required_fields_declared_for_detail_tools() {
        let tools = tools_list();
        for (tool, field) in [
            ("news_article_get", "article_id"),
            ("news_story_get", "story_id"),
            ("news_story_brief", "story_id"),
            ("news_story_translate", "story_id"),
            ("news_analyze_article", "article_id"),
            ("news_source_update", "source_id"),
            ("news_topic_update", "topic_id"),
            ("news_source_discover", "query"),
        ] {
            let t = tools
                .as_array()
                .unwrap()
                .iter()
                .find(|t| t["name"] == tool)
                .unwrap();
            let req = t["inputSchema"]["required"].as_array().unwrap();
            assert!(
                req.iter().any(|r| r == field),
                "{tool} must require {field}"
            );
        }
    }
}
