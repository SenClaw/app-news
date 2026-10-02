---
name: news-reporter
description: >-
  Đọc và phân tích tin tức qua app Tin Tức: thu thập bài từ nhiều nguồn RSS/Atom hoặc
  quét thẳng nội dung trang với trang không có RSS, tìm
  kiếm kho tin (gõ không dấu vẫn khớp), tin mới nhất theo chủ đề/nguồn, cụm từ đang
  tăng nhiệt (trends), dòng sự kiện với timeline diễn biến từ nhiều nguồn, và AI:
  đánh giá độ tin cậy/giật tít từng bài, tóm tắt diễn biến sự kiện, nhận định xu
  hướng, viết bản điểm tin. Dùng khi người dùng hỏi "hôm nay có tin gì", nhờ điểm
  tin, hỏi xu hướng/trending, muốn theo dõi một chủ đề, hỏi diễn biến một sự kiện,
  hay nhờ đánh giá một bài báo. Mọi tin đưa ra phải dẫn nguồn + thời gian từ tool —
  không kể tin từ trí nhớ của model.
triggers:
  - tin tức
  - tin mới
  - đọc báo
  - điểm tin
  - bản tin
  - tin hôm nay
  - có tin gì
  - xu hướng tin
  - trending
  - tin nóng
  - dòng sự kiện
  - diễn biến sự kiện
  - timeline tin
  - tóm tắt tin
  - đánh giá tin
  - tin giật tít
  - nguồn tin
  - tìm nguồn
  - thêm nguồn
  - bản đồ tin
  - liên kết sự kiện
  - sự kiện liên quan
  - rss
  - theo dõi chủ đề
  - news
  - headline
  - breaking news
---

# news-reporter

Dùng MCP server `news-mcp` của app **Tin Tức**. Kho tin là các bài đã THU THẬP từ
nguồn của người dùng — không phải toàn bộ internet. Nguồn có hai kiểu: feed
RSS/Atom (`kind='feed'`), hoặc một trang chuyên mục thường được quét link bài
viết ngay trong HTML (`kind='scrape'`, dành cho trang không phát hành RSS). App
chỉ **đọc** tin; không có tool nào đăng bài hay gửi tin đi đâu.

## Nguyên tắc bắt buộc

- **Tin nào cũng phải có nguồn.** Mọi tin đưa cho người dùng lấy từ tool, kèm tên
  nguồn + thời gian (+ URL khi hữu ích). Không bao giờ kể tin từ trí nhớ của model —
  trí nhớ vừa cũ vừa không kiểm chứng được.
- **Kho rỗng thì thu thập trước.** `news_status` báo 0 bài hoặc dữ liệu cũ →
  gọi `mcp__news-mcp__news_fetch` rồi mới trả lời. Feed lỗi thì báo tên nguồn + lỗi.
- **Số liệu trend là máy đếm, AI chỉ diễn giải.** Không tự bịa "xu hướng" ngoài
  danh sách `news_trends` trả về.

## Chọn công cụ

- **`mcp__news-mcp__news_dashboard`** — LUÔN gọi trước khi trả lời câu hỏi tổng quan
  ("dạo này có tin gì", "tình hình tin tức"): trả về bài theo ngày, chủ đề nóng,
  cụm từ tăng nhiệt, dòng sự kiện nóng, bài mới nhất.
- **`news_latest`** — tin mới nhất (mặc định 24h/20 bài), lọc `topic_id` /
  `source_id` / `category` / `section`. Nhanh nhất khi cần tin cho nền tảng khác.
- **`news_sections`** — chuyên mục mà System One (Laya) đã xếp bài vào, kèm số
  bài ("có tin pháp luật gì không", "tin thể thao hôm nay"). Lấy tên chuyên mục
  rồi truyền làm `section` cho `news_latest`/`news_search`. Nếu trạng thái báo
  System One chưa sẵn sàng thì chuyên mục chưa có — dùng `topic_id` thay.
- **`news_search`** — tìm trong kho tin theo từ khóa (FTS, không dấu vẫn khớp),
  lọc thêm nguồn/chủ đề/sự kiện/khoảng giờ.
- **`news_article_get`** → chi tiết + tin liên quan cùng sự kiện;
  **`news_article_content`** → tải toàn văn bài gốc (làm trước khi phân tích sâu
  bài chỉ có mô tả ngắn).
- **`news_analyze_article`** — AI đánh giá một bài: tóm tắt, cảm xúc, tầm quan trọng
  1-5, nghi giật tít, nhận xét độ tin cậy, tags. Kết quả cache; `force=true` chấm lại.
- **`news_trends`** — cụm từ tăng nhiệt (so với kỳ liền trước, kèm bài mẫu);
  **`news_analyze_trends`** — thuê AI diễn giải các xu hướng đó.
- **`news_story_list`** → dòng sự kiện đang nóng; **`news_story_get`** → TIMELINE
  diễn biến (bài MỚI NHẤT TRƯỚC, nhiều nguồn); **`news_story_brief`** — AI tóm
  tắt diễn biến (tổng thể → mốc thời gian → điểm bỏ ngỏ). Mỗi lần tóm tắt được
  giữ lại thành lịch sử (`summaries` trong `news_story_get`, kèm thời điểm và
  lúc đó dòng có bao nhiêu bài) — dùng khi được hỏi "hồi đó tin này ra sao".
- **`news_story_translate`** — dịch tiêu đề + mô tả cả dòng sự kiện sang ngôn ngữ
  hiển thị đã cài; bản gốc luôn giữ. Dùng khi dòng có nguồn tiếng nước ngoài.
- **`news_stories_rebuild`** — gom lại toàn bộ kho thành dòng sự kiện. App tự
  chạy theo chu kỳ, nên CHỈ gọi khi người dùng chỉ ra một dòng đang lẫn bài
  không liên quan và muốn sửa ngay (thao tác này xoá lịch sử tóm tắt cũ).
- **`news_story_graph`** → bản đồ liên kết: node = sự kiện, cạnh = trùng cụm từ
  khóa (máy thống kê, chặt chẽ nên thường ít cạnh).
  **`news_analyze_graph`** → AI đọc bản đồ đó rồi map lại: gom mạch chuyện, NỐI
  THÊM quan hệ máy bỏ sót (nguyên nhân → hệ quả, cùng chủ thể), chỉ ra liên kết
  nhiễu. Dùng khi được hỏi "các sự kiện liên quan nhau thế nào", "bức tranh chung".
- **`news_digest`** — bản điểm tin (Tin chính / Đáng chú ý / Xu hướng) cho N giờ,
  `focus` = mối quan tâm của người dùng, `topic_id` giới hạn một chủ đề. Dùng khi
  được nhờ "điểm tin hôm nay". Mỗi lần chạy đều được LƯU LẠI.
- **`news_digest_history`** — các bản điểm tin đã chạy (50 bản gần nhất). Người
  dùng hỏi "bản điểm tin lúc sáng", "xem lại điểm tin hôm qua" thì ĐỌC LẠI bằng
  tool này (`digest_id` để lấy nguyên văn) thay vì bắt AI viết lại — vừa nhanh
  vừa đúng nội dung họ đã đọc.
- **`news_source_discover`** — TỰ TÌM nguồn mới: `query` là chủ đề ("tin công nghệ
  tiếng Việt" → AI gợi ý feed) hoặc URL trang web (tự dò feed qua thẻ `<link>`).
  Trang không có feed nào thì app tự thử QUÉT NỘI DUNG TRANG và trả kết quả với
  `kind: "scrape"` — báo rõ điều đó cho người dùng khi liệt kê. Mọi gợi ý đều
  được app TẢI THỬ THẬT, chỉ nguồn thực sự ra bài mới trả về. Mặc định chỉ liệt
  kê để người dùng chọn; `auto_add=true` khi họ nói rõ "thêm luôn".
- **`news_source_add` / `news_source_list` / `news_source_update` /
  `news_source_delete`** — quản lý nguồn. `kind='feed'` (mặc định): URL phải là
  feed, không phải trang chủ. `kind='scrape'`: URL là trang chuyên mục/danh sách
  bài của trang KHÔNG có RSS — trỏ vào trang chuyên mục cụ thể cho kết quả tốt
  hơn trang chủ, và trang chỉ render bài bằng JavaScript sẽ quét ra rỗng (app
  báo lỗi rõ ở `last_error`). Ưu tiên feed khi trang có cả hai: nó rẻ hơn, có
  ngày đăng chuẩn và không phải mở từng bài. Ngừng theo dõi mà muốn giữ bài cũ →
  `status='paused'`, đừng xoá.
- **`news_topic_add` / `news_topic_update` / `news_topic_list` /
  `news_topic_delete`** — chủ đề theo dõi (từ khóa phẩy ngăn cách). Khi System One
  sẵn sàng, Laya quyết định bài nào thuộc chủ đề (tên + từ khóa là mô tả cho
  model) và phân loại lại bài 7 ngày gần đây sau mỗi lần sửa; không có System One
  thì gán theo khớp từ khóa trên bài 30 ngày gần đây.

## Mẫu tình huống

- "Điểm tin sáng nay cho tôi" → `news_fetch` (nếu lần quét cuối > 1h) →
  `news_digest {hours: 12}` → trả nguyên bản điểm tin kèm chú thích nguồn.
- "Vụ X diễn biến thế nào rồi?" → `news_search {q: "X"}` → lấy `story_id` →
  `news_story_get` (+ `news_story_brief` nếu người dùng muốn tóm tắt) → kể theo
  timeline, mỗi mốc kèm nguồn.
- "Bài này đáng tin không?" (kèm link đã có trong kho) → `news_search` tìm bài →
  `news_analyze_article {with_content: true}` → nêu đánh giá + nhấn mạnh đây là
  nhận định tham khảo.
- "Theo dõi tin về bán dẫn giúp tôi" → `news_topic_add {name: "Bán dẫn",
  keywords: "bán dẫn, chip, semiconductor, TSMC"}` → xác nhận số bài đã khớp.
- "Thêm nguồn tin thể thao đi" → `news_source_discover {query: "tin thể thao
  tiếng Việt"}` → đọc danh sách nguồn ĐÃ kiểm chứng (kèm số bài, tiêu đề mẫu) cho
  người dùng chọn, rồi `news_source_add` cái họ chọn.
- "Mấy vụ này liên quan gì nhau?" → `news_analyze_graph` → kể theo mạch chuyện,
  nói rõ đâu là liên kết máy đếm được và đâu là suy luận của AI.
