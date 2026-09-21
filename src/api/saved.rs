//! `/api/saved`：收藏的查询，按用户区分。存的是什么、为什么存地址不存筛选结构，见 [`crate::saved`]。
//!
//! 这几条路由在认证中间件里面：中间件认出人之后把 [`Identity`] 放进请求的 extensions，这里取出来
//! 当归属；没开认证的部署没有中间件、也没有身份，所有人共用 [`SHARED_OWNER`] 名下的那一份。

use std::convert::Infallible;

use axum::{
    Json, Router,
    body::Bytes,
    extract::{FromRequestParts, Path, State},
    http::{StatusCode, request::Parts},
    response::{IntoResponse, Response},
    routing::get,
};
use serde::{Deserialize, Serialize};

use super::AppState;
use crate::auth::Identity;
use crate::error::Error;
use crate::saved::{Patch, SHARED_OWNER, SaveError, SavedQuery};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/saved", get(list).post(create))
        .route("/api/saved/{id}", axum::routing::put(update).delete(delete))
}

/// 这个请求归哪个账号：认证中间件放进来的身份，没有就是共用的那一个。
pub struct Owner(pub String);

impl<S: Send + Sync> FromRequestParts<S> for Owner {
    type Rejection = Infallible;

    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, Infallible> {
        let user = parts.extensions.get::<Identity>().map(|who| who.user().to_owned());
        Ok(Owner(user.unwrap_or_else(|| SHARED_OWNER.to_owned())))
    }
}

/// 一条收藏给页面看的形状。`user` 不返回：列表里全是本人的。
#[derive(Serialize)]
struct View {
    id: String,
    name: String,
    path: String,
    query: String,
    /// RFC3339
    created_at: String,
    updated_at: String,
}

impl From<SavedQuery> for View {
    fn from(q: SavedQuery) -> Self {
        Self {
            id: q.id,
            name: q.name,
            path: q.path,
            query: q.query,
            created_at: rfc3339(q.created_at),
            updated_at: rfc3339(q.updated_at),
        }
    }
}

fn rfc3339(secs: i64) -> String {
    chrono::DateTime::from_timestamp(secs, 0).map(|t| t.to_rfc3339()).unwrap_or_default()
}

#[derive(Serialize)]
struct List {
    queries: Vec<View>,
    /// 每个人最多多少条，页面上快满了提示一句
    max: usize,
}

/// 我的收藏，新的在前。
async fn list(State(state): State<AppState>, Owner(user): Owner) -> Response {
    let queries = state.saved.list(&user).into_iter().map(View::from).collect();
    Json(List { queries, max: crate::saved::PER_USER_MAX }).into_response()
}

#[derive(Deserialize, Default)]
struct Body {
    name: Option<String>,
    path: Option<String>,
    query: Option<String>,
}

fn parse_body(body: &Bytes) -> Result<Body, Error> {
    if body.iter().all(u8::is_ascii_whitespace) {
        return Ok(Body::default());
    }
    serde_json::from_slice(body).map_err(|e| {
        Error::bad_request(format!(
            "请求体应是 JSON，如 {{\"name\": \"订单超时\", \"path\": \"/logs\", \"query\": \"q=timeout\"}}: {e}"
        ))
    })
}

fn save_error(e: SaveError) -> Response {
    match e {
        SaveError::Invalid(_) => Error::bad_request(e.to_string()).into_response(),
        SaveError::Duplicate(ref id) => (
            StatusCode::CONFLICT,
            Json(serde_json::json!({ "error": e.to_string(), "kind": "conflict", "existing": id })),
        )
            .into_response(),
        SaveError::TooMany => Error::bad_request(e.to_string()).into_response(),
        SaveError::Store(_) => Error::internal(e).into_response(),
    }
}

/// 收藏一条。body：`{"name": "订单超时", "path": "/logs", "query": "q=timeout&level=ERROR"}`，
/// `name` 可省（用地址当名字），`query` 可空。同一个人同一个地址只存一条，重复是 409。
async fn create(State(state): State<AppState>, Owner(user): Owner, body: Bytes) -> Response {
    let req = match parse_body(&body) {
        Ok(r) => r,
        Err(e) => return e.into_response(),
    };
    let Some(path) = req.path.as_deref() else {
        return Error::bad_request("缺 path：要收藏的页面路径，如 /logs").into_response();
    };
    match state.saved.create(
        &user,
        req.name.as_deref().unwrap_or(""),
        path,
        req.query.as_deref().unwrap_or(""),
    ) {
        Ok(q) => {
            tracing::info!(user = %q.user, id = %q.id, path = %q.path, "收藏查询");
            (StatusCode::CREATED, Json(View::from(q))).into_response()
        }
        Err(e) => save_error(e),
    }
}

/// 改名，或换成新的地址（`path` + `query` 一起给）。别人的 / 不存在的一律 404。
async fn update(
    State(state): State<AppState>,
    Owner(user): Owner,
    Path(id): Path<String>,
    body: Bytes,
) -> Response {
    let req = match parse_body(&body) {
        Ok(r) => r,
        Err(e) => return e.into_response(),
    };
    let patch = Patch { name: req.name, path: req.path, query: req.query };
    match state.saved.update(&user, id.trim(), patch) {
        Ok(Some(q)) => Json(View::from(q)).into_response(),
        Ok(None) => not_found(),
        Err(e) => save_error(e),
    }
}

/// 删掉我的一条。别人的 / 不存在的一律 404，不区分。
async fn delete(
    State(state): State<AppState>,
    Owner(user): Owner,
    Path(id): Path<String>,
) -> Response {
    match state.saved.delete(&user, id.trim()) {
        Ok(true) => {
            tracing::info!(user = %user, id = %id, "删除收藏");
            StatusCode::NO_CONTENT.into_response()
        }
        Ok(false) => not_found(),
        Err(e) => Error::internal(format!("收藏文件读写失败: {e}")).into_response(),
    }
}

fn not_found() -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(serde_json::json!({ "error": "没有这条收藏（或者它不是你的）", "kind": "not_found" })),
    )
        .into_response()
}
