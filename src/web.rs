//! Web arayüzü ve JSON API. Canlı güncellemeler Server-Sent Events ile akar.

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Path, State};
use axum::http::{StatusCode, header};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::{Stream, StreamExt};
use serde::Deserialize;
use serde_json::json;
use tokio_stream::wrappers::BroadcastStream;

use crate::hub::{Action, Hub};

const INDEX_HTML: &str = include_str!("../assets/index.html");

pub fn router(hub: Arc<Hub>) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/api/state", get(state))
        .route("/api/events", get(events))
        .route("/api/permit_join", post(permit_join))
        .route("/api/hue_reset", post(hue_reset))
        .route("/api/devices/{ieee}", axum::routing::delete(remove))
        .route("/api/devices/{ieee}/command", post(command))
        .route("/api/devices/{ieee}/rename", post(rename))
        .route("/api/devices/{ieee}/setting", post(setting))
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
            // Sayfa ikiliyle birlikte güncelleniyor; tarayıcı eskisini göstermesin
            (header::CACHE_CONTROL, "no-cache"),
        ],
        INDEX_HTML,
    )
}

async fn state(State(hub): State<Arc<Hub>>) -> ApiResult {
    Ok(Json(json!({
        "devices": hub.devices(),
        "permit_join": hub.permit_remaining(),
    })))
}

async fn events(
    State(hub): State<Arc<Hub>>,
) -> Sse<impl Stream<Item = Result<SseEvent, Infallible>>> {
    let stream = BroadcastStream::new(hub.subscribe()).filter_map(|event| async move {
        // Geride kalan istemci olayları kaçırır; arayüz yeniden bağlanınca durumu tazeler
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

async fn command(
    State(hub): State<Arc<Hub>>,
    Path(ieee): Path<String>,
    Json(action): Json<Action>,
) -> ApiResult {
    hub.command(&ieee, action).await?;
    ok()
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

#[derive(Deserialize)]
struct SettingBody {
    key: String,
    value: f64,
}

async fn setting(
    State(hub): State<Arc<Hub>>,
    Path(ieee): Path<String>,
    Json(body): Json<SettingBody>,
) -> ApiResult {
    let pending = hub.set_setting(&ieee, &body.key, body.value).await?;
    Ok(Json(json!({ "ok": true, "pending": pending })))
}

async fn interview(State(hub): State<Arc<Hub>>, Path(ieee): Path<String>) -> ApiResult {
    let waiting = hub.reinterview(&ieee)?;
    Ok(Json(json!({ "ok": true, "waiting_for_wake": waiting })))
}

async fn remove(State(hub): State<Arc<Hub>>, Path(ieee): Path<String>) -> ApiResult {
    hub.remove(&ieee).await?;
    ok()
}
