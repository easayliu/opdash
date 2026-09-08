//! 用 rust-embed 把 `ui/dist` 前端构建产物编进二进制并提供静态服务，SPA fallback。
//! 和 luban 的 admin_ui.rs 同一套做法。

use axum::{
    body::Body,
    http::{Response, StatusCode, Uri, header},
    response::{IntoResponse, Redirect},
};
use rust_embed::Embed;
use tower_http::compression::{
    CompressionLayer, DefaultPredicate, Predicate, predicate::NotForContentType,
};

/// 编译期从 `ui/dist` 读取。仓库里留了个 `.gitkeep`，没构建前端也能 `cargo check` / `cargo test`。
#[derive(Embed)]
#[folder = "ui/dist"]
struct Asset;

/// 响应压缩：前端 bundle 和 JSON 结果都压。日志导出是 CSV 文本，压完体积只剩几分之一。
/// 字体（woff2 自带 brotli）不压。
pub fn compression() -> CompressionLayer<impl Predicate> {
    CompressionLayer::new()
        .gzip(true)
        .br(true)
        .compress_when(DefaultPredicate::new().and(NotForContentType::new("font/")))
}

/// 误发到首页的 POST 文档导航转成 GET，免得浏览器刷新时要求重新提交表单。
pub async fn redirect_root_post() -> Redirect {
    Redirect::to("/")
}

/// 整个应用的 fallback：命中静态资源则返回，否则 SPA fallback 到 index.html。
/// `/api/*` 由主路由先匹配，不会走到这里。
pub async fn fallback(uri: Uri) -> impl IntoResponse {
    let path = uri.path().trim_start_matches('/');

    if path.contains("..") {
        return Response::builder()
            .status(StatusCode::BAD_REQUEST)
            .body(Body::from("Invalid path"))
            .expect("build response");
    }

    if let Some(content) = Asset::get(path) {
        let mime = mime_guess::from_path(path).first_or_octet_stream().to_string();
        return Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, mime)
            .header(header::CACHE_CONTROL, cache_control(path))
            .body(Body::from(content.data.into_owned()))
            .expect("build response");
    }

    // 没有扩展名的路径 → 前端路由，交给 index.html
    if !is_asset_path(path) {
        return serve_index();
    }

    Response::builder()
        .status(StatusCode::NOT_FOUND)
        .body(Body::from("Not found"))
        .expect("build response")
}

fn serve_index() -> Response<Body> {
    match Asset::get("index.html") {
        Some(content) => Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
            .header(header::CACHE_CONTROL, "no-cache")
            .body(Body::from(content.data.into_owned()))
            .expect("build response"),
        None => Response::builder()
            .status(StatusCode::NOT_FOUND)
            .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
            .body(Body::from(
                "前端还没构建：在 ui/ 目录执行 `pnpm install && pnpm build`，然后重新 cargo build。\n\
                 API 不受影响，可以直接访问 /api/health。",
            ))
            .expect("build response"),
    }
}

fn cache_control(path: &str) -> &'static str {
    if path.ends_with(".html") {
        "no-cache"
    } else if path.starts_with("assets/") {
        // vite 产物带内容哈希，可以永久缓存
        "public, max-age=31536000, immutable"
    } else {
        "public, max-age=3600"
    }
}

fn is_asset_path(path: &str) -> bool {
    path.rsplit('/').next().map(|f| f.contains('.')).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        Router,
        http::{Method, Request},
        routing::get,
    };
    use tower::ServiceExt;

    fn app() -> Router {
        Router::new()
            .route("/", get(fallback).post(redirect_root_post))
            .fallback_service(get(fallback))
    }

    #[tokio::test]
    async fn unknown_route_gets_index_or_not_built_message() {
        let response = app()
            .oneshot(Request::builder().uri("/traces/abc").body(Body::empty()).unwrap())
            .await
            .unwrap();
        // 有 dist 时是 200 + html；没构建时是 404 + 提示。两种都不该是 500。
        assert!(matches!(response.status(), StatusCode::OK | StatusCode::NOT_FOUND));
        let ct = response.headers().get(header::CONTENT_TYPE).unwrap().to_str().unwrap();
        assert!(ct.starts_with("text/"), "{ct}");
    }

    #[tokio::test]
    async fn missing_asset_is_404_and_dotdot_is_400() {
        let response = app()
            .oneshot(Request::builder().uri("/assets/missing.js").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let response = app()
            .oneshot(Request::builder().uri("/..%2Fetc/passwd").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert!(matches!(response.status(), StatusCode::BAD_REQUEST | StatusCode::NOT_FOUND));
    }

    #[tokio::test]
    async fn root_post_redirects_to_get() {
        let response = app()
            .oneshot(Request::builder().method(Method::POST).uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
    }
}
