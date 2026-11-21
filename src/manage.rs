use crate::{camera_id, App};
use axum::{
    extract::{ws::{Message, WebSocket, WebSocketUpgrade}, Path, Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{Redirect, Response},
    routing::{get, post},
    Json, Router,
};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{env, net::Ipv4Addr, os::unix::fs::PermissionsExt, path::PathBuf, time::Duration};
use tokio::{fs, process::Command, time};
use tokio_tungstenite::tungstenite::Message as Upstream;

type Problem = (StatusCode, &'static str);

fn problem(status: StatusCode, message: &'static str) -> Problem {
    (status, message)
}

fn require_origin(headers: &HeaderMap) -> Result<(), Problem> {
    let origin = env::var("PUBLIC_ORIGIN").unwrap_or_else(|_| "https://camera.nghi.org".into());
    if headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()) != Some(origin.as_str()) {
        return Err(problem(StatusCode::FORBIDDEN, "Yêu cầu phải đến từ trang CameraHome."));
    }
    Ok(())
}

fn internal() -> Problem {
    problem(StatusCode::INTERNAL_SERVER_ERROR, "Không lưu được cấu hình trên máy chủ.")
}

pub fn routes() -> Router<App> {
    Router::new()
        .route("/api/cameras", post(add_camera))
        .route("/api/live/{id}", get(live))
        .route("/api/drive", get(drive_status).post(start_drive))
        .route("/drive/callback", get(finish_drive))
}

#[derive(Deserialize)]
struct CameraInput {
    id: String,
    ip: Ipv4Addr,
    username: String,
    password: String,
}

async fn add_camera(State(app): State<App>, headers: HeaderMap, Json(input): Json<CameraInput>) -> Result<StatusCode, Problem> {
    require_origin(&headers)?;
    let subnet = env::var("CAMERA_SUBNET").unwrap_or_else(|_| "192.168.88".into());
    let address = input.ip.octets();
    if !camera_id(&input.id) || input.id.len() > 48 || format!("{}.{}.{}", address[0], address[1], address[2]) != subnet
        || !(1..255).contains(&address[3]) || input.username.is_empty() || input.password.is_empty()
        || input.username.len() > 128 || input.password.len() > 128 {
        return Err(problem(StatusCode::BAD_REQUEST, "Kiểm tra mã camera, IP trong mạng camera và Camera Account."));
    }
    let _guard = app.management.lock().await;
    let raw = fs::read("/frigate-config/config.yml").await.map_err(|_| internal())?;
    let mut config: Value = serde_yaml::from_slice(&raw).map_err(|_| internal())?;
    let root = config.as_object_mut().ok_or_else(internal)?;
    let url = format!("rtsp://{}:{}@{}:554/stream1", encode(&input.username), encode(&input.password), input.ip);
    let camera = root.entry("cameras").or_insert_with(|| json!({})).as_object_mut().ok_or_else(internal)?;
    if camera.contains_key(&input.id) {
        return Err(problem(StatusCode::CONFLICT, "Mã camera đã tồn tại."));
    }
    let relay = format!("rtsp://127.0.0.1:8554/{}", input.id);
    camera.insert(input.id.clone(), json!({
        "ffmpeg": {"inputs": [{"path": relay, "input_args": "preset-rtsp-restream", "roles": ["detect", "record"]}]}
    }));
    let streams = root.entry("go2rtc").or_insert_with(|| json!({})).as_object_mut().ok_or_else(internal)?
        .entry("streams").or_insert_with(|| json!({})).as_object_mut().ok_or_else(internal)?;
    if streams.contains_key(&input.id) {
        return Err(problem(StatusCode::CONFLICT, "Mã luồng trực tiếp đã tồn tại."));
    }
    // Backfill live streams for Tapo cameras previously added through Frigate's config.
    streams.insert(input.id, json!([url]));
    add_existing_streams(&mut config);
    let payload = serde_yaml::to_string(&config).map_err(|_| internal())?;
    let response = app.client.post(format!("{}/api/config/save?save_option=restart", app.frigate))
        .timeout(Duration::from_secs(20)).header(header::CONTENT_TYPE, "text/plain").body(payload).send().await
        .map_err(|_| problem(StatusCode::BAD_GATEWAY, "Frigate chưa phản hồi. Làm mới để kiểm tra trước khi thử lại."))?;
    if !response.status().is_success() || !response.json::<Value>().await.is_ok_and(|body| body["success"].as_bool() == Some(true)) {
        return Err(problem(StatusCode::BAD_GATEWAY, "Frigate từ chối cấu hình. Camera chưa được xác nhận."));
    }
    Ok(StatusCode::CREATED)
}

fn add_existing_streams(config: &mut Value) {
    let direct: Vec<_> = config["cameras"].as_object().into_iter().flatten().filter_map(|(id, cam)| {
        let path = cam["ffmpeg"]["inputs"].as_array()?.first()?["path"].as_str()?;
        (camera_id(id) && path.starts_with("rtsp://") && !path.contains("127.0.0.1:8554")).then(|| (id.clone(), json!([path])))
    }).collect();
    if let Some(streams) = config["go2rtc"]["streams"].as_object_mut() {
        for (id, url) in direct {
            streams.entry(id).or_insert(url);
        }
    }
}

fn encode(value: &str) -> String {
    let mut output = String::new();
    for c in value.bytes() {
        if c.is_ascii_alphanumeric() || b"-._~".contains(&c) {
            output.push(c as char);
        } else {
            output.push_str(&format!("%{c:02X}"));
        }
    }
    output
}

async fn live(State(app): State<App>, Path(id): Path<String>, headers: HeaderMap, ws: WebSocketUpgrade) -> Result<Response, Problem> {
    require_origin(&headers)?;
    if !camera_id(&id) || !app.frigate.starts_with("http://") {
        return Err(problem(StatusCode::BAD_REQUEST, "Mã camera không hợp lệ."));
    }
    let url = format!("{}{}/live/mse/api/ws?src={id}", "ws://", &app.frigate[7..]);
    Ok(ws.on_upgrade(move |socket| live_session(socket, url)))
}

async fn live_session(mut socket: WebSocket, url: String) {
    let Ok(Ok((mut upstream, _))) = time::timeout(Duration::from_secs(8), tokio_tungstenite::connect_async(url)).await else {
        let _ = socket.send(Message::Text(r#"{"type":"error","value":"Chưa kết nối được luồng trực tiếp."}"#.into())).await;
        return;
    };
    let request = json!({"type":"mse","value":"avc1.640029,avc1.64002A,avc1.42E01E,mp4a.40.2"}).to_string();
    if upstream.send(Upstream::Text(request.into())).await.is_err() { return; }
    loop {
        tokio::select! {
            browser = socket.recv() => {
                if !matches!(browser, Some(Ok(Message::Ping(_) | Message::Pong(_)))) { break; }
            }
            server = upstream.next() => {
                let output = match server {
                    Some(Ok(Upstream::Text(text))) => Message::Text(text.to_string().into()),
                    Some(Ok(Upstream::Binary(bytes))) => Message::Binary(bytes),
                    Some(Ok(Upstream::Ping(_) | Upstream::Pong(_))) => continue,
                    _ => break,
                };
                if socket.send(output).await.is_err() { break; }
            }
        }
    }
}

const DRIVE_CONFIG: &str = "/app-data/drive.conf";
const DRIVE_FOLDER: &str = "/app-data/drive-folder";
const DRIVE_STATUS: &str = "/app-data/drive-status";

#[derive(Deserialize)]
struct DriveInput {
    folder: String,
    client_id: String,
    client_secret: String,
}

async fn drive_status() -> Json<Value> {
    let folder = fs::read_to_string(DRIVE_FOLDER).await.ok();
    let last = fs::read_to_string(DRIVE_STATUS).await.ok();
    Json(json!({"folder":folder,"last":last}))
}

async fn write_private(path: &str, value: &str) -> std::io::Result<()> {
    let temp = PathBuf::from(format!("{path}.new"));
    fs::write(&temp, value).await?;
    fs::set_permissions(&temp, std::fs::Permissions::from_mode(0o600)).await?;
    fs::rename(temp, path).await
}

pub struct OAuthPending {
    state: String,
    expires: std::time::Instant,
    settings: DriveInput,
}

fn callback_url() -> String {
    let origin = env::var("PUBLIC_ORIGIN").unwrap_or_else(|_| "https://camera.nghi.org".into());
    format!("{origin}/drive/callback")
}

async fn start_drive(State(app): State<App>, headers: HeaderMap, Json(mut input): Json<DriveInput>) -> Result<Json<Value>, Problem> {
    require_origin(&headers)?;
    let folder = input.folder.trim().trim_matches('/');
    let safe_key = |value: &str| value.len() < 300 && !value.is_empty()
        && value.bytes().all(|c| c.is_ascii_alphanumeric() || b"-_.".contains(&c));
    if folder.is_empty() || folder.len() > 200 || folder.split('/').any(|part| part.is_empty() || matches!(part, "." | ".."))
        || folder.chars().any(|c| c.is_control() || c == ':' || c == char::from(92))
        || !safe_key(&input.client_id) || !input.client_id.ends_with(".apps.googleusercontent.com")
        || !safe_key(&input.client_secret) {
        return Err(problem(StatusCode::BAD_REQUEST, "Kiểm tra thư mục và thông tin OAuth của Google."));
    }
    input.folder = folder.to_owned();
    let mut entropy = [0u8; 32];
    getrandom::fill(&mut entropy).map_err(|_| internal())?;
    let state = entropy.iter().map(|byte| format!("{byte:02x}")).collect::<String>();
    let mut url = reqwest::Url::parse("https://accounts.google.com/o/oauth2/v2/auth").map_err(|_| internal())?;
    url.query_pairs_mut().extend_pairs([
        ("client_id", input.client_id.as_str()), ("redirect_uri", callback_url().as_str()),
        ("response_type", "code"), ("scope", "https://www.googleapis.com/auth/drive.file"),
        ("access_type", "offline"), ("prompt", "consent"), ("state", state.as_str()),
    ]);
    *app.oauth.lock().await = Some(OAuthPending {
        state, expires: std::time::Instant::now() + Duration::from_secs(600), settings: input,
    });
    Ok(Json(json!({"url": url.to_string()})))
}

#[derive(Deserialize)]
struct Callback {
    state: Option<String>,
    code: Option<String>,
}

async fn finish_drive(State(app): State<App>, Query(query): Query<Callback>) -> Redirect {
    let pending = {
        let mut oauth = app.oauth.lock().await;
        if !oauth.as_ref().is_some_and(|p| query.state.as_deref() == Some(p.state.as_str())
            && p.expires > std::time::Instant::now()) {
            return Redirect::to("/?drive=expired#drive");
        }
        oauth.take()
    };
    let (Some(pending), Some(code)) = (pending, query.code) else {
        return Redirect::to("/?drive=cancelled#drive");
    };
    let result = connect_drive(&app, pending.settings, code).await;
    Redirect::to(if result.is_ok() { "/?drive=ok#drive" } else { "/?drive=failed#drive" })
}

async fn connect_drive(app: &App, settings: DriveInput, code: String) -> Result<(), Problem> {
    let mut token: Value = app.client.post("https://oauth2.googleapis.com/token").timeout(Duration::from_secs(20))
        .form(&[("code", code.as_str()), ("client_id", settings.client_id.as_str()),
            ("client_secret", settings.client_secret.as_str()), ("redirect_uri", callback_url().as_str()),
            ("grant_type", "authorization_code")])
        .send().await.map_err(|_| internal())?.error_for_status().map_err(|_| internal())?
        .json().await.map_err(|_| internal())?;
    if token["refresh_token"].as_str().is_none_or(str::is_empty) || token["access_token"].as_str().is_none_or(str::is_empty) {
        return Err(internal());
    }
    let expires = token["expires_in"].as_i64().unwrap_or(3600).clamp(1, 86400);
    token["expiry"] = json!((chrono::Utc::now() + chrono::Duration::seconds(expires)).to_rfc3339());
    let folder = &settings.folder;
    let _drive_guard = app.drive_lock.lock().await;
    let pending = "/app-data/drive-test.conf";
    write_private(pending, &format!("[drive]\ntype = drive\nscope = drive.file\nclient_id = {}\nclient_secret = {}\ntoken = {token}\n", settings.client_id, settings.client_secret))
        .await.map_err(|_| internal())?;
    let destination = format!("drive:{folder}");
    let result = Command::new("rclone").args(["--config", pending, "mkdir", &destination, "--contimeout", "8s", "--timeout", "15s", "--retries", "1", "--low-level-retries", "1"])
        .kill_on_drop(true).output();
    if !matches!(time::timeout(Duration::from_secs(30), result).await, Ok(Ok(output)) if output.status.success()) {
        let _ = fs::remove_file(pending).await;
        return Err(problem(StatusCode::BAD_GATEWAY, "Google Drive chưa chấp nhận token hoặc không tạo được thư mục."));
    }
    fs::rename(pending, DRIVE_CONFIG).await.map_err(|_| internal())?;
    write_private(DRIVE_FOLDER, folder).await.map_err(|_| internal())?;
    write_private(DRIVE_STATUS, "Đã kết nối. Đang chờ lượt sao lưu.").await.map_err(|_| internal())?;
    Ok(())
}

pub async fn backup_worker(app: App) {
    let mut tick = time::interval(Duration::from_secs(300));
    tick.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
    loop {
        tick.tick().await;
        let Ok(_guard) = app.drive_lock.try_lock() else { continue; };
        let Ok(folder) = fs::read_to_string(DRIVE_FOLDER).await else { continue; };
        let dest = format!("drive:{folder}/recordings");
        let outcome = Command::new("rclone").args(["--config", DRIVE_CONFIG, "copy"])
            .arg(&app.recordings).args([&dest, "--min-age", "1m", "--contimeout", "10s", "--timeout", "1m", "--retries", "2"])
            .kill_on_drop(true).output();
        let success = matches!(time::timeout(Duration::from_secs(280), outcome).await, Ok(Ok(output)) if output.status.success());
        let when = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs();
        let message = if success { "Lượt sao lưu hoàn tất" } else { "Lượt sao lưu lỗi hoặc vượt thời gian. Sẽ thử lại." };
        let _ = write_private(DRIVE_STATUS, &format!("{message}|{when}")).await;
    }
}
