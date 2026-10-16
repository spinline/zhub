//! Web UI and JSON API. Live updates flow over Server-Sent Events.

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::{Stream, StreamExt};
use serde::Deserialize;
use serde_json::json;
use tokio_stream::wrappers::BroadcastStream;

use crate::hub::Hub;

const INDEX_HTML: &str = include_str!("../assets/index.html");

pub fn router(hub: Arc<Hub>) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/api/state", get(state))
        .route("/api/events", get(events))
        .route("/api/permit_join", post(permit_join))
        .route("/api/hue_reset", post(hue_reset))
        .route("/api/hub", get(hub_info))
        .route("/api/hub/settings", post(settings))
        .route("/api/hub/tx_power", post(tx_power))
        .route("/api/hub/restart", post(restart))
        .route("/api/logs", get(logs))
        .route("/api/devices/{ieee}", axum::routing::delete(remove))
        .route("/api/devices/{ieee}/set", post(set))
        .route("/api/devices/{ieee}/rename", post(rename))
        .route("/api/devices/{ieee}/interview", post(interview))
        .with_state(hub)
}

struct ApiError(anyhow::Error);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": format!("{:#}", self.0) })),
        )
            .into_response()
    }
}

impl<E: Into<anyhow::Error>> From<E> for ApiError {
    fn from(e: E) -> Self {
        Self(e.into())
    }
}

type ApiResult = Result<Json<serde_json::Value>, ApiError>;

fn ok() -> ApiResult {
    Ok(Json(json!({ "ok": true })))
}

async fn index() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            // The page ships with the binary; the browser must not show a stale copy
            (header::CACHE_CONTROL, "no-cache"),
        ],
        INDEX_HTML,
    )
}

async fn state(State(hub): State<Arc<Hub>>) -> ApiResult {
    Ok(Json(json!({
        "devices": hub.devices(),
        "permit_join": hub.permit_remaining(),
        "settings": hub.settings(),
    })))
}

async fn hub_info(State(hub): State<Arc<Hub>>) -> ApiResult {
    Ok(Json(json!(hub.info())))
}

async fn settings(State(hub): State<Arc<Hub>>, Json(body): Json<crate::store::Settings>) -> ApiResult {
    hub.update_settings(body)?;
    ok()
}

#[derive(Deserialize)]
struct TxPower {
    dbm: i8,
}

async fn tx_power(State(hub): State<Arc<Hub>>, Json(body): Json<TxPower>) -> ApiResult {
    let actual = hub.set_tx_power(body.dbm).await?;
    Ok(Json(json!({ "ok": true, "tx_power": actual })))
}

async fn restart(State(hub): State<Arc<Hub>>) -> ApiResult {
    hub.request_restart();
    ok()
}

#[derive(Deserialize)]
struct LogsQuery {
    #[serde(default)]
    after: u64,
}

async fn logs(State(hub): State<Arc<Hub>>, Query(query): Query<LogsQuery>) -> ApiResult {
    Ok(Json(json!({ "lines": hub.log_lines(query.after) })))
}

async fn events(
    State(hub): State<Arc<Hub>>,
) -> Sse<impl Stream<Item = Result<SseEvent, Infallible>>> {
    let stream = BroadcastStream::new(hub.subscribe()).filter_map(|event| async move {
        // A lagging client misses events; the UI refreshes its state when it reconnects
        let event = event.ok()?;
        SseEvent::default().json_data(event).ok().map(Ok)
    });
    Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(20)))
}

#[derive(Deserialize)]
struct HueReset {
    serials: Vec<String>,
}

async fn hue_reset(State(hub): State<Arc<Hub>>, Json(body): Json<HueReset>) -> ApiResult {
    hub.hue_factory_reset(&body.serials).await?;
    ok()
}

#[derive(Deserialize)]
struct PermitJoin {
    seconds: u8,
}

async fn permit_join(State(hub): State<Arc<Hub>>, Json(body): Json<PermitJoin>) -> ApiResult {
    hub.permit_join(body.seconds)?;
    ok()
}

#[derive(Deserialize)]
struct SetBody {
    key: String,
    value: serde_json::Value,
}

async fn set(State(hub): State<Arc<Hub>>, Path(ieee): Path<String>, Json(body): Json<SetBody>) -> ApiResult {
    let pending = hub.set(&ieee, &body.key, body.value).await?;
    Ok(Json(json!({ "ok": true, "pending": pending })))
}

#[derive(Deserialize)]
struct Rename {
    name: String,
}

async fn rename(
    State(hub): State<Arc<Hub>>,
    Path(ieee): Path<String>,
    Json(body): Json<Rename>,
) -> ApiResult {
    hub.rename(&ieee, &body.name)?;
    ok()
}

async fn interview(State(hub): State<Arc<Hub>>, Path(ieee): Path<String>) -> ApiResult {
    let waiting = hub.reinterview(&ieee)?;
    Ok(Json(json!({ "ok": true, "waiting_for_wake": waiting })))
}

async fn remove(State(hub): State<Arc<Hub>>, Path(ieee): Path<String>) -> ApiResult {
    hub.remove(&ieee).await?;
    ok()
}
