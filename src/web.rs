//! Web UI and JSON API. Live updates flow over Server-Sent Events.

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, Query, Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::{Stream, StreamExt};
use serde::Deserialize;
use serde_json::json;
use tokio_stream::wrappers::BroadcastStream;

use crate::{firmware, hub::Hub, store::UiAuth};

const INDEX_HTML: &str = include_str!("../assets/index.html");
const FIRMWARE_HTML: &str = include_str!("../assets/firmware.html");

pub fn router(hub: Arc<Hub>) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/api/state", get(state))
        .route("/api/scenes", get(scene_list).post(scene_command))
        .route("/api/scenes/refresh", post(scene_refresh))
        .route("/api/events", get(events))
        .route("/api/permit_join", post(permit_join))
        .route("/api/hue_reset", post(hue_reset))
        .route("/api/hub", get(hub_info))
        .route("/api/hub/settings", post(settings))
        .route("/api/hub/tx_power", post(tx_power))
        .route("/api/hub/restart", post(restart))
        .route("/api/backup", get(download_backup))
        .route("/api/backup/inspect", post(inspect_backup).layer(DefaultBodyLimit::max(crate::backup::MAX_ARCHIVE_BYTES)))
        .route("/api/backup/restore", post(restore_backup).layer(DefaultBodyLimit::max(crate::backup::MAX_ARCHIVE_BYTES)))
        .route("/api/mqtt/cleanup", post(mqtt_cleanup))
        .route("/api/matter/open", post(matter_open))
        .route("/api/matter/reset", post(matter_reset))
        .route("/api/logs", get(logs))
        .route("/api/networkmap", get(network_map))
        .route("/api/networkmap/scan", post(start_network_scan))
        .route("/api/networkmap/scan/{id}", get(network_scan_result))
        .route("/api/health", get(network_health))
        .route("/api/devices/{ieee}", axum::routing::delete(remove))
        .route("/api/devices/{ieee}/set", post(set))
        .route("/api/devices/{ieee}/rename", post(rename))
        .route("/api/devices/{ieee}/options", post(device_options))
        .route("/api/devices/{ieee}/interview", post(interview))
        .route("/api/devices/{ieee}/bindings", get(bindings))
        .route("/api/devices/{ieee}/reporting", get(reporting_fields).post(configure_reporting))
        .route("/api/devices/{ieee}/reporting/{key}", get(read_reporting))
        .route("/api/groups", post(create_group))
        .route("/api/groups/{id}", axum::routing::delete(delete_group))
        .route("/api/groups/{id}/rename", post(rename_group))
        .route("/api/groups/{id}/matter", post(group_matter))
        .route("/api/groups/{id}/members", post(group_member))
        .route("/api/groups/{id}/set", post(set_group))
        .route("/api/devices/{ieee}/bind", post(bind))
        .route("/api/login", post(login))
        .route("/api/logout", post(logout))
        .route("/api/hub/password", post(set_password))
        .layer(axum::middleware::from_fn_with_state(hub.clone(), require_password))
        .with_state(hub)
}

pub fn firmware_router(setup: Arc<firmware::Setup>) -> Router {
    Router::new()
        .route("/", get(|| async { html(FIRMWARE_HTML) }))
        .route("/api/login", post(firmware_login))
        .route(
            "/api/firmware",
            get(firmware_status).post(upload_firmware).layer(DefaultBodyLimit::max(firmware::MAX_IMAGE_SIZE)),
        )
        .layer(axum::middleware::from_fn_with_state(setup.clone(), require_firmware_password))
        .with_state(setup)
}

async fn require_firmware_password(
    State(setup): State<Arc<firmware::Setup>>,
    request: Request,
    next: axum::middleware::Next,
) -> Response {
    let mut response = require_auth(setup.auth.as_ref(), request, next).await;
    // A polling browser must reconnect when the normal hub replaces the setup server.
    response.headers_mut().insert(header::CONNECTION, "close".parse().unwrap());
    response.headers_mut().insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
    response
}

async fn firmware_status(State(setup): State<Arc<firmware::Setup>>) -> ApiResult {
    let discovery = setup.discovery();
    Ok(Json(json!({
        "device": setup.device,
        "startup_error": setup.startup_error,
        "dongle": discovery.dongle,
        "firmware": discovery.firmware,
        "usb_device": discovery.usb_device,
        "serial_available": discovery.serial_available,
        "detecting": discovery.detecting,
        "status": setup.status(),
    })))
}

async fn upload_firmware(
    State(setup): State<Arc<firmware::Setup>>,
    headers: HeaderMap,
    data: Bytes,
) -> ApiResult {
    if !headers.get(header::CONTENT_TYPE).is_some_and(|value| value == "application/octet-stream") {
        return Err(anyhow::anyhow!("Send a binary firmware file").into());
    }
    let confirmed = headers.get("x-zhub-confirm-dongle").is_some_and(|value| value == "1");
    setup.submit(data, confirmed)?;
    ok()
}

async fn firmware_login(State(setup): State<Arc<firmware::Setup>>, Json(body): Json<Login>) -> Response {
    password_login(setup.auth.clone(), body).await
}

// ---- Web UI password (Zigbee2MQTT's frontend.auth_token) ----

const AUTH_COOKIE: &str = "zhub_auth";

fn cookie<'a>(headers: &'a axum::http::HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .find_map(|pair| pair.trim().strip_prefix(name)?.strip_prefix('='))
}

fn auth_cookie(key: Option<&str>) -> String {
    match key {
        Some(key) => format!("{AUTH_COOKIE}={key}; Path=/; HttpOnly; SameSite=Strict; Max-Age=31536000"),
        None => format!("{AUTH_COOKIE}=; Path=/; HttpOnly; SameSite=Strict; Max-Age=0"),
    }
}

/// With a password set, the API answers only a browser that logged in; the page itself
/// loads so it can ask for the password
async fn require_password(
    State(hub): State<Arc<Hub>>,
    request: Request,
    next: axum::middleware::Next,
) -> Response {
    require_auth(hub.ui_auth().as_ref(), request, next).await
}

async fn require_auth(auth: Option<&UiAuth>, request: Request, next: axum::middleware::Next) -> Response {
    let path = request.uri().path();
    let open = !path.starts_with("/api/") || path == "/api/login" || path == "/api/logout";
    let allowed = open
        || match auth {
            None => true,
            Some(auth) => cookie(request.headers(), AUTH_COOKIE).is_some_and(|key| auth.accepts_key(key)),
        };
    if allowed {
        next.run(request).await
    } else {
        (StatusCode::UNAUTHORIZED, Json(json!({ "error": "password required", "auth": true }))).into_response()
    }
}

#[derive(Deserialize)]
struct Login {
    password: String,
}

async fn login(State(hub): State<Arc<Hub>>, Json(body): Json<Login>) -> Response {
    password_login(hub.ui_auth(), body).await
}

async fn password_login(auth: Option<UiAuth>, body: Login) -> Response {
    let Some(auth) = auth else {
        return Json(json!({ "ok": true })).into_response();
    };
    let key = auth.key.clone();
    // Stretching the password takes a moment on the router: off the async threads
    let matches = tokio::task::spawn_blocking(move || auth.matches(&body.password)).await.unwrap_or(false);
    if !matches {
        tracing::warn!("Web UI: wrong password");
        // Slows down guessing
        tokio::time::sleep(Duration::from_secs(1)).await;
        return (StatusCode::UNAUTHORIZED, Json(json!({ "error": "wrong password", "auth": true }))).into_response();
    }
    ([(header::SET_COOKIE, auth_cookie(Some(&key)))], Json(json!({ "ok": true }))).into_response()
}

async fn logout() -> Response {
    ([(header::SET_COOKIE, auth_cookie(None))], Json(json!({ "ok": true }))).into_response()
}

#[derive(Deserialize)]
struct NewPassword {
    password: Option<String>,
}

/// Sets, changes or (null) removes the password; this browser stays logged in
async fn set_password(State(hub): State<Arc<Hub>>, Json(body): Json<NewPassword>) -> Result<Response, ApiError> {
    let password = body.password.filter(|p| !p.is_empty());
    let key = tokio::task::spawn_blocking(move || hub.set_ui_password(password.as_deref())).await.map_err(anyhow::Error::from)??;
    Ok(([(header::SET_COOKIE, auth_cookie(key.as_deref()))], Json(json!({ "ok": true }))).into_response())
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

async fn reporting_fields(State(hub): State<Arc<Hub>>, Path(ieee): Path<String>) -> ApiResult {
    let view = hub.device_view(&ieee).ok_or_else(|| anyhow::anyhow!("device not found"))?;
    Ok(Json(json!({
        "fields": view.reporting_fields,
        "measurement_poll_interval": view.measurement_poll_interval,
    })))
}

async fn read_reporting(State(hub): State<Arc<Hub>>, Path((ieee, key)): Path<(String, String)>) -> ApiResult {
    Ok(Json(hub.read_reporting(&ieee, &key).await?))
}

async fn configure_reporting(
    State(hub): State<Arc<Hub>>,
    Path(ieee): Path<String>,
    Json(request): Json<crate::hub::reporting::ReportingRequest>,
) -> ApiResult {
    Ok(Json(json!(hub.configure_reporting(&ieee, request).await?)))
}

#[derive(Deserialize)]
struct SceneTarget {
    target: String,
    ep: Option<u8>,
    group_id: Option<u16>,
}

async fn scene_list(State(hub): State<Arc<Hub>>, Query(q): Query<SceneTarget>) -> ApiResult {
    Ok(Json(json!({"scenes":hub.scene_list(&q.target,q.ep,q.group_id)?})))
}

async fn scene_refresh(State(hub): State<Arc<Hub>>, Json(q): Json<SceneTarget>) -> ApiResult {
    Ok(Json(json!({"scenes":hub.scene_refresh(&q.target,q.ep,q.group_id).await?})))
}

#[derive(Deserialize)]
struct SceneCommand {
    target: String,
    command: String,
    value: serde_json::Value,
}

async fn scene_command(State(hub): State<Arc<Hub>>, Json(q): Json<SceneCommand>) -> ApiResult {
    hub.scene_apply(&q.target,&q.command,&q.value).await?;
    ok()
}

fn ok() -> ApiResult {
    Ok(Json(json!({ "ok": true })))
}

async fn index() -> impl IntoResponse {
    html(INDEX_HTML)
}

fn html(page: &'static str) -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            // The page ships with the binary; the browser must not show a stale copy
            (header::CACHE_CONTROL, "no-cache"),
        ],
        page,
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
    if let Some(command) = as_light_command(&body.key, &body.value) {
        crate::mqtt::apply_to(&hub, crate::mqtt::ControlTarget::Group(id), command).await?;
        return ok();
    }
    hub.set_group(id, &body.key, &body.value)?;
    ok()
}

async fn hub_info(State(hub): State<Arc<Hub>>) -> ApiResult {
    Ok(Json(json!(hub.info())))
}

/// Takes the settings to change; what the body leaves out stays as it is
async fn settings(State(hub): State<Arc<Hub>>, Json(body): Json<serde_json::Value>) -> ApiResult {
    hub.patch_settings(body)?;
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

async fn mqtt_cleanup(State(hub): State<Arc<Hub>>) -> ApiResult {
    let removed = crate::mqtt::clean_stale(&hub).await?;
    Ok(Json(json!({ "ok": true, "removed": removed })))
}

#[derive(Deserialize)]
struct MapQuery {
    #[serde(rename = "type", default = "raw_format")]
    kind: String,
    #[serde(default)]
    routes: bool,
}

fn raw_format() -> String {
    "raw".into()
}

/// The network map, as Zigbee2MQTT's bridge/response/networkmap data
async fn network_map(State(hub): State<Arc<Hub>>, Query(q): Query<MapQuery>) -> ApiResult {
    if !crate::networkmap::FORMATS.contains(&q.kind.as_str()) {
        return Err(anyhow::anyhow!("Type '{}' not supported", q.kind).into());
    }
    let map = hub.network_map(q.routes).await;
    Ok(Json(crate::networkmap::render(&map, &q.kind, q.routes, crate::networkmap::now_ms())?))
}

async fn start_network_scan(State(hub): State<Arc<Hub>>, Query(q): Query<MapQuery>) -> ApiResult {
    Ok(Json(json!({ "id": hub.start_web_scan(q.routes)? })))
}

async fn network_scan_result(State(hub): State<Arc<Hub>>, Path(id): Path<u64>) -> Result<impl IntoResponse, ApiError> {
    let map = hub.web_scan_result(id)?;
    Ok((
        [(header::CACHE_CONTROL, "no-store")],
        Json(json!({ "pending": map.is_none(), "value": map.as_deref() })),
    ))
}

async fn network_health(State(hub): State<Arc<Hub>>) -> ApiResult {
    Ok(Json(hub.network_health()))
}

async fn download_backup(State(hub): State<Arc<Hub>>) -> Result<impl IntoResponse, ApiError> {
    let zip = tokio::task::spawn_blocking(move || crate::backup::download(hub.backup_state(), &hub.data_dir()))
        .await??;
    Ok(([
        (header::CONTENT_TYPE, "application/zip"),
        (header::CONTENT_DISPOSITION, "attachment; filename=zhub-backup.zip"),
        (header::CACHE_CONTROL, "no-store"),
    ], zip))
}

fn require_zip(headers: &HeaderMap) -> anyhow::Result<()> {
    anyhow::ensure!(
        headers.get(header::CONTENT_TYPE).and_then(|value| value.to_str().ok()) == Some("application/zip"),
        "expected a zhub backup ZIP"
    );
    Ok(())
}

async fn inspect_backup(headers: HeaderMap, bytes: Bytes) -> ApiResult {
    require_zip(&headers)?;
    let summary = tokio::task::spawn_blocking(move || {
        crate::backup::Backup::from_bytes(&bytes).map(|backup| backup.summary())
    }).await??;
    Ok(Json(json!(summary)))
}

async fn restore_backup(State(hub): State<Arc<Hub>>, headers: HeaderMap, bytes: Bytes) -> ApiResult {
    require_zip(&headers)?;
    let path = hub.data_dir();
    tokio::task::spawn_blocking(move || crate::backup::stage_restore(&path, &bytes)).await??;
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(1)).await;
        hub.request_restart("backup restore");
    });
    ok()
}

async fn restart(State(hub): State<Arc<Hub>>) -> ApiResult {
    hub.request_restart("web UI");
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
    let stream = BroadcastStream::new(hub.subscribe())
        // End this stream on lag so EventSource reconnects and reloads the snapshot.
        .take_while(|event| std::future::ready(event.is_ok()))
        .filter_map(|event| async move {
            SseEvent::default().json_data(event.ok()?).ok().map(Ok)
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

/// The UI's light controls as Zigbee2MQTT's commands, so they follow the same rules
/// (transition option, brightness 0 turns off) as MQTT tools
fn as_light_command(key: &str, value: &serde_json::Value) -> Option<serde_json::Map<String, serde_json::Value>> {
    let (k, v) = match key {
        "on" => ("state", json!(if value.as_bool()? { "ON" } else { "OFF" })),
        "level" => ("brightness", value.clone()),
        "color_temp" => ("color_temp", value.clone()),
        _ => return None,
    };
    Some(serde_json::Map::from_iter([(k.to_owned(), v)]))
}

async fn set(State(hub): State<Arc<Hub>>, Path(ieee): Path<String>, Json(body): Json<SetBody>) -> ApiResult {
    let light = hub.device_view(&ieee).is_some_and(|d| d.features.iter().any(|f| f.key == "on"));
    if light && let Some(command) = as_light_command(&body.key, &body.value) {
        crate::mqtt::apply_to(&hub, crate::mqtt::ControlTarget::Device(&ieee), command).await?;
        return Ok(Json(json!({ "ok": true, "pending": false })));
    }
    let pending = hub.set(&ieee, &body.key, body.value).await?;
    Ok(Json(json!({ "ok": true, "pending": pending })))
}

#[derive(Deserialize)]
struct Rename {
    name: String,
}

async fn device_options(State(hub): State<Arc<Hub>>, Path(ieee): Path<String>, Json(body): Json<serde_json::Map<String, serde_json::Value>>) -> ApiResult {
    hub.set_device_options(&ieee, body)?;
    ok()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_the_auth_cookie_among_others() {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(header::COOKIE, "a=1; zhub_auth=abc123; b=2".parse().unwrap());
        assert_eq!(cookie(&headers, AUTH_COOKIE), Some("abc123"));
        headers.insert(header::COOKIE, "zhub_authx=1".parse().unwrap());
        assert_eq!(cookie(&headers, AUTH_COOKIE), None);
    }
}
