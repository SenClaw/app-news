//! SenClaw News Space App — thu thập tin tức từ nhiều nguồn RSS/Atom, gán chủ
//! đề theo keyword rồi phân loại lại bằng System One (Laya), phát hiện xu hướng, gom bài thành DÒNG SỰ KIỆN với timeline,
//! và AI phân tích / đánh giá / điểm tin qua bridge SenClaw. Dữ liệu nằm local
//! (SQLite); outbound duy nhất là chính các feed người dùng khai + bridge LLM.

mod api;
mod cluster;
mod db;
mod fetch;
mod laya;
mod llm;
mod mcp;

use axum::extract::Request;
use axum::http::{header, HeaderValue};
use axum::middleware::{self, Next};
use axum::response::Response;
use axum::Router;
use tower_http::cors::CorsLayer;
use tower_http::services::{ServeDir, ServeFile};

/// Cache policy for the web bundle.
///
/// `index.html` is the file that NAMES the hashed asset it should load, so a
/// browser allowed to cache it keeps running the previous build's JavaScript
/// after an upgrade — the app looks untouched until someone thinks to hard
/// refresh, and every install lands on a user who cannot see what changed.
/// Served without a Cache-Control header it was cached heuristically, which is
/// exactly that failure. It must be revalidated on every load.
///
/// The files under `/assets/` are the opposite case: their name contains a hash
/// of their content, so a given URL can never mean anything else and may be
/// kept forever.
async fn cache_headers(req: Request, next: Next) -> Response {
    let hashed = req.uri().path().starts_with("/assets/");
    let mut res = next.run(req).await;
    res.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static(if hashed {
            "public, max-age=31536000, immutable"
        } else {
            "no-cache"
        }),
    );
    res
}

#[tokio::main]
async fn main() {
    dotenvy::dotenv().ok();

    let port = std::env::var("PORT").unwrap_or_else(|_| "4660".to_string());
    // Mặc định chỉ nghe loopback. Bind 0.0.0.0 phơi API ra cả LAN mà app không
    // có lớp xác thực nào — muốn truy cập từ máy khác thì phải khai tường minh.
    let host = std::env::var("SENCLAW_BIND_HOST").unwrap_or_else(|_| "127.0.0.1".to_string());
    let state = api::make_state();

    state.db.apply_digest_markers();
    llm::set_output_language(&state.db.display_language());

    // System One classifier: walks the articles of the last week that have no
    // section yet. Separate from the collector so a slow or missing decision
    // runtime never delays fetching — ingest has already keyword-tagged every
    // article, so until Laya answers the app behaves exactly as before.
    {
        let state = state.clone();
        tokio::spawn(async move {
            let client = laya::Client::new(&state.sc.base_url);
            let mut was_unavailable = false;
            tokio::time::sleep(std::time::Duration::from_secs(20)).await;
            loop {
                let mut wait = 60;
                if laya::enabled(&state.db) {
                    match laya::classify_pending(&state.db, &client, laya::BATCH).await {
                        laya::Sweep::Done(n) => {
                            was_unavailable = false;
                            if n > 0 {
                                println!("[news] System One: phân loại {n} bài");
                                // More may be waiting (a backfill or a requeue
                                // after a topic edit) — keep going promptly.
                                wait = 2;
                            }
                        }
                        laya::Sweep::Unavailable(n) => {
                            if !was_unavailable {
                                println!(
                                    "[news] System One chưa sẵn sàng ({n} bài đã phân loại) — giữ chủ đề theo từ khóa, thử lại sau"
                                );
                            }
                            was_unavailable = true;
                            wait = 300;
                        }
                    }
                }
                tokio::time::sleep(std::time::Duration::from_secs(wait)).await;
            }
        });
    }

    // Background collector: first sweep shortly after boot, then on the
    // user-configurable interval. `auto_fetch=0` pauses it without restart.
    {
        let state = state.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(10)).await;
            loop {
                if state.db.setting("auto_fetch", "1") == "1" {
                    let r = api::fetch_all_value(&state).await;
                    let new = r["new"].as_i64().unwrap_or(0);
                    if new > 0 {
                        println!("[news] auto-fetch: {new} bài mới");
                    }
                }
                // Regroup after collecting, never during: the incremental
                // clusterer places each new article against the stories that
                // exist at that moment, so periodically re-deriving the whole
                // archive is what keeps early guesses from setting like
                // concrete. Also fires once when the rules themselves change.
                if state.db.regroup_due() {
                    match state.db.rebuild_stories() {
                        Ok(v) => println!(
                            "[news] gom lại dòng sự kiện: {} dòng / {} bài",
                            v["stories"], v["articles"]
                        ),
                        Err(e) => eprintln!("[news] gom lại thất bại: {e}"),
                    }
                }
                let mins: u64 = state
                    .db
                    .setting("fetch_interval_min", "30")
                    .parse()
                    .unwrap_or(30);
                tokio::time::sleep(std::time::Duration::from_secs(mins.clamp(5, 24 * 60) * 60))
                    .await;
            }
        });
    }

    let api_router = api::api_router(state);

    let exe_dir = std::env::current_exe()
        .map(|p| p.parent().unwrap().to_path_buf())
        .unwrap_or_default();
    // Packaged (`web_dist`) paths first; the dev build at `web/dist` last, for
    // `cargo run` from the repo root.
    let candidates = [
        std::path::PathBuf::from("web_dist"),
        exe_dir.join("web_dist"),
        exe_dir.join("web").join("dist"),
        std::path::PathBuf::from("web/dist"),
    ];
    let dist_path = candidates
        .iter()
        .find(|c| c.join("index.html").exists())
        .cloned()
        .unwrap_or_else(|| std::path::PathBuf::from("web/dist"));

    let serve_dir =
        ServeDir::new(&dist_path).not_found_service(ServeFile::new(dist_path.join("index.html")));

    // The cache policy is scoped to the static bundle; API responses are not
    // cached by anyone and must not gain a header saying otherwise.
    let web = Router::new()
        .fallback_service(serve_dir)
        .layer(middleware::from_fn(cache_headers));

    let app = Router::new()
        .nest("/api", api_router)
        .fallback_service(web)
        .layer(CorsLayer::permissive());

    let listener = tokio::net::TcpListener::bind(format!("{host}:{port}"))
        .await
        .unwrap();
    println!("SenClaw News running on http://{host}:{port}");
    axum::serve(listener, app).await.unwrap();
}
