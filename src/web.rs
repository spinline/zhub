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
        .route("/api/matter/open", post(matter_open))
        .route("/api/matter/reset", post(matter_reset))
        .route("/api/logs", get(logs))
        .route("/api/devices/{ieee}", axum::routing::delete(remove))
        .route("/api/devices/{ieee}/set", post(set))
        .route("/api/devices/{ieee}/rename", post(rename))
        .route("/api/devices/{ieee}/interview", post(interview))
        .route("/api/devices/{ieee}/bindings", get(bindings))
        .route("/api/groups", post(create_group))
        .route("/api/groups/{id}", axum::routing::delete(delete_group))
        .route("/api/groups/{id}/rename", post(rename_group))
        .route("/api/groups/{id}/matter", post(group_matter))
        .route("/api/groups/{id}/members", post(group_member))
        .route("/api/groups/{id}/set", post(set_group))
        .route("/api/devices/{ieee}/bind", post(bind))
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
        "settings": hub.public_settings(),
        "groups": hub.groups(),
    })))
}

#[derive(Deserialize)]
struct GroupName {
    name: String,
}

async fn create_group(State(hub): State<Arc<Hub>>, Json(body): Json<GroupName>) -> ApiResult {
    let id = hub.create_group(&body.name)?;
    Ok(Json(json!({ "ok": true, "id": id })))
}

async fn rename_group(State(hub): State<Arc<Hub>>, Path(id): Path<u16>, Json(body): Json<GroupName>) -> ApiResult {
    hub.rename_group(id, &body.name)?;
    ok()
}

#[derive(Deserialize)]
struct GroupMatter {
    enabled: bool,
}

async fn group_matter(State(hub): State<Arc<Hub>>, Path(id): Path<u16>, Json(body): Json<GroupMatter>) -> ApiResult {
    hub.set_group_matter(id, body.enabled)?;
    ok()
}

async fn delete_group(State(hub): State<Arc<Hub>>, Path(id): Path<u16>) -> ApiResult {
    hub.delete_group(id).await?;
    ok()
}

#[derive(Deserialize)]
struct GroupMemberBody {
    ieee: String,
    ep: u8,
    #[serde(default)]
    remove: bool,
}

async fn group_member(State(hub): State<Arc<Hub>>, Path(id): Path<u16>, Json(body): Json<GroupMemberBody>) -> ApiResult {
    hub.set_group_member(id, &body.ieee, body.ep, !body.remove).await?;
    ok()
}

async fn set_group(State(hub): State<Arc<Hub>>, Path(id): Path<u16>, Json(body): Json<SetBody>) -> ApiResult {
    hub.set_group(id, &body.key, &body.value)?;
    ok()
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

async fn matter_open(State(hub): State<Arc<Hub>>) -> ApiResult {
    hub.matter_command(crate::hub::MatterCommand::OpenCommissioning)?;
    ok()
}

async fn matter_reset(State(hub): State<Arc<Hub>>) -> ApiResult {
    hub.matter_command(crate::hub::MatterCommand::Reset)?;
    ok()
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

async fn bindings(State(hub): State<Arc<Hub>>, Path(ieee): Path<String>) -> ApiResult {
    Ok(Json(json!({ "bindings": hub.bindings(&ieee).await? })))
}

#[derive(Deserialize)]
struct BindBody {
    src_ep: u8,
    clusters: Vec<u16>,
    /// "hub" or a device's IEEE address
    dst: String,
    #[serde(default)]
    dst_ep: u8,
    #[serde(default)]
    unbind: bool,
}

async fn bind(State(hub): State<Arc<Hub>>, Path(ieee): Path<String>, Json(body): Json<BindBody>) -> ApiResult {
    if body.clusters.is_empty() {
        return Err(ApiError(anyhow::anyhow!("choose at least one cluster")));
    }
    for cluster in body.clusters {
        hub.set_binding(&ieee, body.src_ep, cluster, &body.dst, body.dst_ep, body.unbind)
            .await?;
    }
    ok()
}

#[derive(Deserialize)]
struct RemoveQuery {
    #[serde(default)]
    force: bool,
}

async fn remove(
    State(hub): State<Arc<Hub>>,
    Path(ieee): Path<String>,
    Query(query): Query<RemoveQuery>,
) -> ApiResult {
    hub.remove(&ieee, query.force).await?;
    ok()
}
