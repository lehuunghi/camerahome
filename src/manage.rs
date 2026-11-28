use crate::{camera_id, App};
use axum::{
    extract::{ws::{Message, WebSocket, WebSocketUpgrade}, Extension, Path, Query, State},
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

pub fn require_origin(headers: &HeaderMap) -> Result<(), Problem> {
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
        .route("/api/ptz/{id}", get(ptz_info).post(ptz_move))
        .route("/api/cameras/{id}/settings", get(camera_settings).post(change_camera_settings).delete(delete_camera))
        .route("/api/drive", get(drive_status).post(start_drive))
        .route("/api/engine",get(engine_status))
        .route("/api/drive/token", post(import_drive_token))
        .route("/api/clips/cleanup", get(cleanup_preview).post(cleanup_recordings))
        .route("/api/clips/purge", get(purge_status).post(purge_all))
        .route("/drive/callback", get(finish_drive))
}

#[derive(Deserialize)]
struct CameraInput {
    id: String,
    ip: Ipv4Addr,
    #[serde(default)]
    mac: Option<String>,
    auto_ip: Option<bool>,
    username: String,
    password: String,
    #[serde(flatten)]
    settings: CameraSettings,
}

async fn add_camera(State(app): State<App>, headers: HeaderMap, Json(input): Json<CameraInput>) -> Result<StatusCode, Problem> {
    require_origin(&headers)?;
    let mac = crate::mac::validate(input.mac.as_deref())?;
    let subnet = env::var("CAMERA_SUBNET").unwrap_or_else(|_| "192.168.88".into());
    let address = input.ip.octets();
    if !camera_id(&input.id) || input.id.len() > 48 || (input.id.ends_with("__compact") || input.id.ends_with("__1080")) || format!("{}.{}.{}", address[0], address[1], address[2]) != subnet
        || !(1..255).contains(&address[3]) || input.username.is_empty() || input.password.is_empty()
        || input.username.len() > 128 || input.password.len() > 128 {
        return Err(problem(StatusCode::BAD_REQUEST, "Kiểm tra mã camera, IP trong mạng camera và Camera Account."));
    }
    let _guard = app.management.lock().await;
    let mut config = read_config().await?;
    let url = format!("rtsp://{}:{}@{}:554/stream1", encode(&input.username), encode(&input.password), input.ip);
    let cameras = object_at(&mut config, "cameras")?;
    if cameras.contains_key(&input.id) {
        return Err(problem(StatusCode::CONFLICT, "Mã camera đã tồn tại."));
    }
    cameras.insert(input.id.clone(), json!({}));
    update_settings(&mut config, &input.id, &url, input.settings)?;
    save_config(&app, &config).await?;
    crate::mac::set(&input.id, mac, input.auto_ip.unwrap_or(true)).await?;
    Ok(StatusCode::CREATED)
}

#[derive(Deserialize)]
struct CameraSettings {
    mode: RecordingMode,
    compact: bool,
}

#[derive(Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
enum RecordingMode { Motion, Crying, Both }

pub async fn read_config() -> Result<Value, Problem> {
    let raw = fs::read("/frigate-config/config.yml").await.map_err(|_| internal())?;
    serde_yaml::from_slice(&raw).map_err(|_| internal())
}

fn object_at<'a>(value: &'a mut Value, name: &str) -> Result<&'a mut serde_json::Map<String, Value>, Problem> {
    value.as_object_mut().ok_or_else(internal)?.entry(name).or_insert_with(|| json!({}))
        .as_object_mut().ok_or_else(internal)
}

pub(crate) fn camera_source<'a>(config: &'a Value, id: &str) -> Option<&'a str> {
    config["cameras"].get(id)?;
    let stream = &config["go2rtc"]["streams"][id];
    let source = stream.as_str().or_else(|| stream[0].as_str())
        .or_else(|| config["cameras"][id]["ffmpeg"]["inputs"][0]["path"].as_str())?;
    (source.starts_with("rtsp://") && source.split(['?', '#']).next()?.ends_with("/stream1")).then_some(source)
}

fn update_settings(config: &mut Value, id: &str, url: &str, settings: CameraSettings) -> Result<(), Problem> {
    let cry = settings.mode != RecordingMode::Motion;
    let motion = settings.mode != RecordingMode::Crying;
    let compact_id = format!("{id}__compact");
    let go2rtc = config.as_object_mut().ok_or_else(internal)?.entry("go2rtc").or_insert_with(|| json!({}));
    let streams = object_at(go2rtc, "streams")?;
    streams.insert(id.to_owned(), json!([url, format!("ffmpeg:{id}#audio=aac")]));
    {
        let location = url.split(['?', '#']).next().ok_or_else(internal)?;
        let mut substream = location.strip_suffix("/stream1").ok_or_else(internal)?.to_owned();
        substream.push_str("/stream2");
        substream.push_str(&url[location.len()..]);
        streams.insert(compact_id.clone(), json!([substream, format!("ffmpeg:{compact_id}#audio=aac")]));
    }
    streams.insert(format!("{id}__1080"),json!([format!("ffmpeg:{id}#video=h264#height=1080#audio=aac")]));
    let feed = if settings.compact { &compact_id } else { id };
    let main_path = format!("rtsp://127.0.0.1:8554/{id}");
    let record_path = format!("rtsp://127.0.0.1:8554/{feed}");
    let mut roles = vec!["detect", "record"];
    if cry && !settings.compact { roles.push("audio"); }
    let mut inputs = vec![json!({
        "path": record_path, "input_args":"preset-rtsp-restream", "roles": roles
    })];
    if cry && settings.compact {
        inputs.push(json!({"path":main_path,"input_args":"preset-rtsp-restream","roles":["audio"]}));
    }
    let camera = object_at(config, "cameras")?.get_mut(id).ok_or_else(internal)?;
    object_at(camera, "ffmpeg")?.insert("inputs".into(), json!(inputs));
    object_at(camera, "audio")?.insert("enabled".into(), json!(cry));
    if cry {
        object_at(camera, "audio")?.insert("listen".into(), json!(["crying"]));
        let ffmpeg = camera.get_mut("ffmpeg").ok_or_else(internal)?;
        object_at(ffmpeg, "output_args")?.insert("record".into(), json!("preset-record-generic-audio-aac"));
        object_at(camera, "review")?.entry("alerts").or_insert_with(|| json!({}))["labels"] = json!(["crying"]);
    }
    let record = object_at(camera, "record")?;
    record.insert("enabled".into(), json!(true));
    record.insert("continuous".into(), json!({"days":0}));
    record.insert("motion".into(), json!({"days":if motion {3} else {0}}));
    record.insert("alerts".into(), json!({"pre_capture":5,"post_capture":10,"retain":{"days":if cry {3} else {0},"mode":"all"}}));
    record.insert("detections".into(), json!({"retain":{"days":0,"mode":"all"}}));
    Ok(())
}

async fn camera_settings(State(app): State<App>, Extension(user): Extension<crate::accounts::Principal>, Path(id): Path<String>) -> Result<Json<Value>, Problem> {
    let _guard = app.management.lock().await;
    let config = read_config().await?;
    if !camera_id(&id) || camera_source(&config, &id).is_none() {
        return Err(problem(StatusCode::NOT_FOUND, "Camera này chưa có cấu hình Tapo tương thích."));
    }
    let camera = &config["cameras"][&id];
    let cry = camera["audio"]["enabled"].as_bool().unwrap_or(false);
    let mode = if !cry {"motion"} else if camera["record"]["motion"]["days"].as_f64() == Some(0.0) {"crying"} else {"both"};
    let compact = camera["ffmpeg"]["inputs"][0]["path"].as_str().is_some_and(|path| path.ends_with("__compact"));
    let ip = if user.admin { camera_source(&config, &id).and_then(|url| reqwest::Url::parse(url).ok()).and_then(|url| url.host_str().map(str::to_owned)) } else { None };
    let identity = if user.admin { crate::mac::identity(&id).await? } else { json!(null) };
    Ok(Json(json!({"mode":mode,"compact":compact,"ip":ip,"identity":identity})))
}

#[derive(Deserialize)]
struct CameraEdit {
    #[serde(flatten)]
    settings: CameraSettings,
    connection: Option<CameraConnection>,
    mac: Option<String>,
    auto_ip: Option<bool>,
}

#[derive(Deserialize)]
struct CameraConnection { ip: Ipv4Addr, username: String, password: String }

async fn change_camera_settings(State(app): State<App>, Extension(user): Extension<crate::accounts::Principal>, Path(id): Path<String>, headers: HeaderMap, Json(edit): Json<CameraEdit>) -> Result<StatusCode, Problem> {
    require_origin(&headers)?;
    let _guard = app.management.lock().await;
    let mut config = read_config().await?;
    let url = camera_source(&config, &id).filter(|_| camera_id(&id)).map(str::to_owned)
        .ok_or_else(|| problem(StatusCode::NOT_FOUND, "Camera này chưa có cấu hình Tapo tương thích."))?;
    let identity_change = edit.mac.is_some() || edit.auto_ip.is_some();
    if identity_change && !user.admin { return Err(problem(StatusCode::FORBIDDEN, "Chỉ admin được đổi MAC.")); }
    let mac = crate::mac::validate(edit.mac.as_deref())?;
    let url = if let Some(connection) = edit.connection {
        if !user.admin { return Err(problem(StatusCode::FORBIDDEN, "Chỉ admin được đổi kết nối camera.")); }
        let [a,b,c,d] = connection.ip.octets();
        let subnet = env::var("CAMERA_SUBNET").unwrap_or_else(|_| "192.168.88".into());
        if format!("{a}.{b}.{c}") != subnet || !(1..255).contains(&d) || connection.username.is_empty()
            || connection.password.is_empty() || connection.username.len()>128 || connection.password.len()>128 {
            return Err(problem(StatusCode::BAD_REQUEST, "Nhập IP trong mạng camera cùng tên và mật khẩu Camera Account."));
        }
        if let Some(onvif) = config["cameras"][&id]["onvif"].as_object().filter(|v|v.contains_key("host")) {
            let mut settings = onvif.clone();
            settings.insert("host".into(),json!(connection.ip.to_string()));
            settings.insert("user".into(),json!(connection.username));
            settings.insert("password".into(),json!(connection.password));
            config["cameras"][&id]["onvif"] = Value::Object(settings);
        }
        format!("rtsp://{}:{}@{}:554/stream1", encode(&connection.username), encode(&connection.password), connection.ip)
    } else { url };
    update_settings(&mut config, &id, &url, edit.settings)?;
    save_config(&app, &config).await?;
    if identity_change { crate::mac::set(&id, mac, edit.auto_ip.unwrap_or(true)).await?; }
    Ok(StatusCode::NO_CONTENT)
}

async fn delete_camera(State(app): State<App>, Extension(user): Extension<crate::accounts::Principal>, Path(id): Path<String>, headers: HeaderMap) -> Result<StatusCode, Problem> {
    require_origin(&headers)?;
    if !user.admin { return Err(problem(StatusCode::FORBIDDEN,"Chỉ admin được xóa camera.")); }
    let _guard = app.management.lock().await;
    let mut config = read_config().await?;
    if !camera_id(&id) || object_at(&mut config,"cameras")?.remove(&id).is_none() {
        return Err(problem(StatusCode::NOT_FOUND,"Không tìm thấy camera."));
    }
    if let Some(streams) = config["go2rtc"]["streams"].as_object_mut() {
        for name in [id.clone(),format!("{id}__compact"),format!("{id}__1080")] { streams.remove(&name); }
    }
    if let Some(groups) = config["camera_groups"].as_object_mut() {
        for group in groups.values_mut() {
            if let Some(cameras) = group["cameras"].as_array_mut() { cameras.retain(|name|name.as_str()!=Some(id.as_str())); }
        }
    }
    save_config(&app,&config).await?;
    crate::mac::remove(&id).await?;
    Ok(StatusCode::NO_CONTENT)
}

pub(crate) async fn save_config(app: &App, config: &Value) -> Result<(), Problem> {
    if matches!(purge_status().await.0["state"].as_str(),Some("pending"|"running")) {
        return Err(problem(StatusCode::CONFLICT,"Đang xóa bản ghi. Chờ hoàn tất trước khi đổi cấu hình camera."));
    }
    let payload = serde_yaml::to_string(config).map_err(|_| internal())?;
    let response = app.client.post(format!("{}/api/config/save?save_option=restart", app.frigate))
        .timeout(Duration::from_secs(20)).header(header::CONTENT_TYPE, "text/plain").body(payload).send().await
        .map_err(|_| problem(StatusCode::BAD_GATEWAY, "Dịch vụ camera chưa phản hồi. Làm mới để kiểm tra trước khi thử lại."))?;
    if !response.status().is_success() || !response.json::<Value>().await.is_ok_and(|body| body["success"].as_bool() == Some(true)) {
        return Err(problem(StatusCode::BAD_GATEWAY, "Dịch vụ camera từ chối cấu hình. Camera chưa được xác nhận."));
    }
    Ok(())
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

#[derive(Deserialize)]
struct LiveQuality { quality: Option<String> }

async fn live(State(app): State<App>, Extension(user): Extension<crate::accounts::Principal>, Path(id): Path<String>, Query(query): Query<LiveQuality>, headers: HeaderMap, ws: WebSocketUpgrade) -> Result<Response, Problem> {
    require_origin(&headers)?;
    if !camera_id(&id) || !app.frigate.starts_with("http://") {
        return Err(problem(StatusCode::BAD_REQUEST, "Mã camera không hợp lệ."));
    }
    let stream = match query.quality.as_deref() {
        None | Some("original") => id.clone(),
        Some("low") => format!("{id}__compact"),
        Some("hd") => format!("{id}__1080"),
        _ => return Err(problem(StatusCode::BAD_REQUEST, "Chất lượng không hợp lệ.")),
    };
    let url = if let Ok(base)=env::var("GO2RTC_URL") {format!("{}/api/ws?src={stream}",base.replacen("http://","ws://",1).trim_end_matches('/'))} else {format!("ws://{}/live/mse/api/ws?src={stream}", &app.frigate[7..])};
    Ok(ws.on_upgrade(move |socket| live_session(socket, url, user, id)))
}

async fn live_session(mut socket: WebSocket, url: String, user: crate::accounts::Principal, id: String) {
    let Ok(Ok((mut upstream, _))) = time::timeout(Duration::from_secs(8), tokio_tungstenite::connect_async(url)).await else {
        let _ = socket.send(Message::Text(r#"{"type":"error","value":"Chưa kết nối được luồng trực tiếp."}"#.into())).await;
        return;
    };
    let request = json!({"type":"mse","value":"avc1.640029,avc1.64002A,avc1.42E01E,mp4a.40.2"}).to_string();
    if upstream.send(Upstream::Text(request.into())).await.is_err() { return; }
    let mut check = time::interval(Duration::from_secs(15));
    loop {
        tokio::select! {
            _ = check.tick() => {
                if !crate::accounts::still_allowed_live(&user, &id).await { break; }
            }
            browser = socket.recv() => {
                if !matches!(browser, Some(Ok(Message::Ping(_) | Message::Pong(_)))) { break; }
            }
            server = upstream.next() => {
                let output = match server {
                    Some(Ok(Upstream::Text(text))) => {
                        let Ok(payload) = serde_json::from_str::<Value>(&text) else { continue; };
                        match payload["type"].as_str() {
                            Some("mse") => {
                                let Some(codec) = payload["value"].as_str().filter(|v| v.starts_with("video/mp4") && v.len() < 256) else { continue; };
                                Message::Text(json!({"type":"mse","value":codec}).to_string().into())
                            }
                            Some("error") => Message::Text(json!({"type":"error"}).to_string().into()),
                            _ => continue,
                        }
                    },
                    Some(Ok(Upstream::Binary(bytes))) => Message::Binary(bytes),
                    Some(Ok(Upstream::Ping(_) | Upstream::Pong(_))) => continue,
                    _ => break,
                };
                if socket.send(output).await.is_err() { break; }
            }
        }
    }
}

async fn supports_ptz(app: &App, id: &str) -> Result<bool, Problem> {
    if !camera_id(id) {
        return Err(problem(StatusCode::BAD_REQUEST, "Mã camera không hợp lệ."));
    }
    let config = read_config().await?;
    if config["cameras"][id]["onvif"]["host"].as_str().is_none() { return Ok(false); }
    let error = || problem(StatusCode::BAD_GATEWAY, "Không đọc được khả năng xoay của camera.");
    let info: Value = app.client.get(format!("{}/api/{id}/ptz/info", app.frigate))
        .send().await.map_err(|_| error())?.error_for_status().map_err(|_| error())?
        .json().await.map_err(|_| error())?;
    Ok(info["features"].as_array().is_some_and(|features| features.iter().any(|value| value.as_str() == Some("pt"))))
}

async fn ptz_info(State(app): State<App>, Path(id): Path<String>) -> Result<Json<Value>, Problem> {
    Ok(Json(json!({"pan_tilt": supports_ptz(&app, &id).await?})))
}

#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum Direction { Up, Down, Left, Right }

#[derive(Deserialize)]
struct MoveInput { direction: Direction }

async fn ptz_move(State(app): State<App>, Path(id): Path<String>, headers: HeaderMap, Json(input): Json<MoveInput>) -> Result<StatusCode, Problem> {
    require_origin(&headers)?;
    if !supports_ptz(&app, &id).await? {
        return Err(problem(StatusCode::CONFLICT, "Camera này không hỗ trợ xoay."));
    }
    let guard = app.ptz_lock.clone().try_lock_owned().map_err(|_| problem(StatusCode::CONFLICT, "Đang xoay camera."));
    let guard = guard?;
    let host = app.frigate.strip_prefix("http://").ok_or_else(internal)?;
    let ws_url = format!("ws://{host}/ws");
    let topic = format!("{id}/ptz");
    let payload = match input.direction {
        Direction::Up => "MOVE_UP",
        Direction::Down => "MOVE_DOWN",
        Direction::Left => "MOVE_LEFT",
        Direction::Right => "MOVE_RIGHT",
    };
    let moved = tokio::spawn(async move {
        let _guard = guard;
        let Ok(Ok((mut ws, _))) = time::timeout(Duration::from_secs(2), tokio_tungstenite::connect_async(ws_url.as_str())).await else { return false; };
        let start = Upstream::Text(json!({"topic":topic,"payload":payload}).to_string().into());
        let stop = Upstream::Text(json!({"topic":topic,"payload":"STOP"}).to_string().into());
        let started = matches!(time::timeout(Duration::from_secs(1), ws.send(start)).await, Ok(Ok(())));
        if started { time::sleep(Duration::from_millis(280)).await; }
        if matches!(time::timeout(Duration::from_secs(1), ws.send(stop.clone())).await, Ok(Ok(()))) {
            let _ = time::timeout(Duration::from_millis(300), ws.close(None)).await;
            return started;
        }
        if let Ok(Ok((mut retry, _))) = time::timeout(Duration::from_secs(2), tokio_tungstenite::connect_async(ws_url)).await {
            let _ = time::timeout(Duration::from_secs(1), retry.send(stop)).await;
        }
        false
    }).await.unwrap_or(false);
    if !moved { return Err(problem(StatusCode::BAD_GATEWAY, "Không xác nhận được lệnh xoay.")); }
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct Cleanup {
    before: Option<String>,
    camera: Option<String>,
    #[serde(default)]
    confirm: String,
    #[serde(default)]
    count: usize,
}

fn cutoff(input: &Cleanup) -> Result<Option<chrono::NaiveDateTime>, Problem> {
    if let Some(id)=input.camera.as_deref() {
        if !camera_id(id) || input.before.is_some() {
            return Err(problem(StatusCode::BAD_REQUEST,"Phạm vi camera không hợp lệ."));
        }
        return Ok(None);
    }
    let before=input.before.as_deref().ok_or_else(||problem(StatusCode::BAD_REQUEST,"Chọn ngày hoặc một camera."))?;
    let date = chrono::NaiveDate::parse_from_str(before, "%Y-%m-%d")
        .map_err(|_| problem(StatusCode::BAD_REQUEST, "Ngày dọn bản ghi không hợp lệ."))?;
    let today = (chrono::Utc::now() + chrono::Duration::hours(7)).date_naive();
    if date > today {
        return Err(problem(StatusCode::BAD_REQUEST, "Chọn ngày hiện tại hoặc một ngày trước đó."));
    }
    date.and_hms_opt(0, 0, 0).and_then(|d| d.checked_sub_signed(chrono::Duration::hours(7)))
        .map(Some).ok_or_else(internal)
}

fn old_recordings(root: PathBuf, before: Option<chrono::NaiveDateTime>, camera: Option<String>) -> std::io::Result<Vec<PathBuf>> {
    let mut result = Vec::new();
    for entry in crate::entries(&root)? {
        let day = entry.file_name().to_string_lossy().into_owned();
        if !entry.file_type()?.is_dir() || !crate::valid_recording_day(&day) { continue; }
        let Ok(date) = chrono::NaiveDate::parse_from_str(&day, "%Y-%m-%d") else { continue; };
        if before.is_some_and(|cutoff|date > cutoff.date()) { continue; }
        let clips = crate::clips_for_day(root.clone(), crate::Selection { day, camera: camera.clone() })?;
        for clip in clips {
            let parts: Vec<_> = clip.split('/').collect();
            let Some(stem) = parts[3].strip_suffix(".mp4") else { continue; };
            let time = format!("{} {}.{}", parts[0], parts[1], stem);
            if chrono::NaiveDateTime::parse_from_str(&time, "%Y-%m-%d %H.%M.%S").is_ok_and(|at| before.is_none_or(|cutoff|at < cutoff)) {
                result.push(root.join(clip));
            }
        }
    }
    Ok(result)
}

async fn cleanup_preview(State(app): State<App>, Query(input): Query<Cleanup>) -> Result<Json<Value>, Problem> {
    let before = cutoff(&input)?;
    let count = tokio::task::spawn_blocking(move || old_recordings(app.recordings, before, input.camera).map(|list| list.len()))
        .await.map_err(|_| internal())?.map_err(|_| internal())?;
    Ok(Json(json!({"count":count})))
}

async fn cleanup_recordings(State(app): State<App>, headers: HeaderMap, Json(input): Json<Cleanup>) -> Result<Json<Value>, Problem> {
    require_origin(&headers)?;
    if input.confirm != "XOA" || input.count == 0 {
        return Err(problem(StatusCode::BAD_REQUEST, "Hãy kiểm tra số bản ghi và nhập XOA để xác nhận."));
    }
    let before = cutoff(&input)?;
    let _backup = app.drive_lock.try_lock().map_err(|_| problem(StatusCode::CONFLICT, "Đang sao lưu Drive. Thử lại khi lượt sao lưu xong."))?;
    let _cleanup = app.management.lock().await;
    let result = tokio::task::spawn_blocking(move || -> Result<Value, Problem> {
        let clips = old_recordings(app.recordings, before, input.camera).map_err(|_| internal())?;
        if clips.len() != input.count {
            return Err(problem(StatusCode::CONFLICT, "Danh sách bản ghi đã đổi. Hãy kiểm tra lại trước khi xóa."));
        }
        let mut removed = 0;
        let mut failed = 0;
        for path in clips {
            match std::fs::remove_file(&path) {
                Ok(()) => removed += 1,
                Err(_) => failed += 1,
            }
        }
        Ok(json!({"removed":removed,"failed":failed}))
    }).await.map_err(|_| internal())??;
    Ok(Json(result))
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
        if matches!(purge_status().await.0["state"].as_str(),Some("pending"|"running")) {continue;}
        let Ok(_guard) = app.drive_lock.try_lock() else { continue; };
        if matches!(purge_status().await.0["state"].as_str(),Some("pending"|"running")) {continue;}
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

const PURGE_REQUEST:&str="/app-data/purge-request.json";
const PURGE_STATUS:&str="/app-data/purge-status.json";
async fn purge_status()->Json<Value> {
    let status=fs::read(PURGE_STATUS).await.ok().and_then(|b|serde_json::from_slice::<Value>(&b).ok()).unwrap_or_else(||json!({"state":"idle"}));
    Json(status)
}
#[derive(Deserialize)]
struct PurgeInput {confirm:String}
async fn purge_all(State(app):State<App>,Extension(user):Extension<crate::accounts::Principal>,headers:HeaderMap,Json(input):Json<PurgeInput>)->Result<(StatusCode,Json<Value>),Problem> {
    require_origin(&headers)?;
    if !user.admin {return Err(problem(StatusCode::FORBIDDEN,"Chỉ admin được xóa toàn bộ bản ghi."));}
    if input.confirm!="XOA TAT CA" {return Err(problem(StatusCode::BAD_REQUEST,"Nhập XOA TAT CA để xác nhận xóa vĩnh viễn."));}
    let _lock=app.management.lock().await;
    let _backup=app.drive_lock.try_lock().map_err(|_|problem(StatusCode::CONFLICT,"Đang sao lưu Drive. Hãy thử lại sau khi sao lưu xong."))?;
    let current=purge_status().await.0;
    if fs::try_exists(PURGE_REQUEST).await.unwrap_or(true) || matches!(current["state"].as_str(),Some("pending"|"running")) {
        return Err(problem(StatusCode::CONFLICT,"Đang xóa toàn bộ bản ghi. Vui lòng chờ."));
    }
    let job=chrono::Utc::now().timestamp_millis().to_string();
    let status=json!({"state":"pending","job":job,"message":"Đang chờ dừng ghi hình và dọn toàn bộ video trên máy chủ.","at":chrono::Utc::now().timestamp()});
    write_private(PURGE_STATUS,&status.to_string()).await.map_err(|_|internal())?;
    if write_private(PURGE_REQUEST,&json!({"job":job,"confirm":"XOA TAT CA"}).to_string()).await.is_err() {
        let _=write_private(PURGE_STATUS,&json!({"state":"error","message":"Không gửi được yêu cầu xóa."}).to_string()).await;
        return Err(internal());
    }
    Ok((StatusCode::ACCEPTED,Json(status)))
}

#[derive(Deserialize)]
struct DriveTokenInput {folder:String,token:String}
fn drive_token(raw:&str)->Result<Value,Problem>{
    if raw.len()>32768 {return Err(problem(StatusCode::BAD_REQUEST,"Mã kết nối quá dài."));}
    let input:Value=serde_json::from_str(raw.trim()).map_err(|_|problem(StatusCode::BAD_REQUEST,"Dán nguyên phần JSON của mã rclone, bắt đầu bằng { và kết thúc bằng }."))?;
    let credential=|key:&str|input[key].as_str().filter(|v|!v.is_empty()&&v.len()<=16384&&!v.chars().any(char::is_control)).map(str::to_owned);
    let access=credential("access_token").ok_or_else(||problem(StatusCode::BAD_REQUEST,"Mã kết nối thiếu access_token."))?;
    let refresh=credential("refresh_token").ok_or_else(||problem(StatusCode::BAD_REQUEST,"Mã kết nối thiếu refresh_token. Hãy chạy lại rclone authorize."))?;
    let expiry=input["expiry"].as_str().filter(|v|chrono::DateTime::parse_from_rfc3339(v).is_ok()).ok_or_else(||problem(StatusCode::BAD_REQUEST,"Mã kết nối thiếu thời hạn hợp lệ."))?;
    if input["token_type"].as_str().is_some_and(|v|!v.eq_ignore_ascii_case("Bearer")) {return Err(problem(StatusCode::BAD_REQUEST,"Loại mã kết nối không hợp lệ."));}
    Ok(json!({"access_token":access,"refresh_token":refresh,"token_type":"Bearer","expiry":expiry}))
}
fn drive_folder(input:&str)->Result<String,Problem>{
    let folder=input.trim().trim_matches('/');
    if folder.is_empty()||folder.len()>200||folder.split('/').any(|p|p.is_empty()||matches!(p,"."|".."))
        ||folder.chars().any(|c|c.is_control()||c==':'||c==char::from(92)){
        return Err(problem(StatusCode::BAD_REQUEST,"Tên thư mục trên Drive không hợp lệ."));
    }
    Ok(folder.to_owned())
}
async fn import_drive_token(State(app):State<App>,Extension(user):Extension<crate::accounts::Principal>,headers:HeaderMap,Json(input):Json<DriveTokenInput>)->Result<StatusCode,Problem>{
    require_origin(&headers)?;
    if !user.admin {return Err(problem(StatusCode::FORBIDDEN,"Chỉ admin được kết nối Google Drive."));}
    let folder=drive_folder(&input.folder)?;
    let token=drive_token(&input.token)?;
    let _guard=app.drive_lock.lock().await;
    let pending="/app-data/drive-import.conf";
    // No custom client: rclone refreshes using its built-in OAuth client.
    write_private(pending,&format!("[drive]\ntype = drive\nscope = drive\ntoken = {token}\n")).await.map_err(|_|internal())?;
    let destination=format!("drive:{folder}");
    for operation in ["mkdir","lsf"] {
        let mut command=Command::new("rclone");
        command.args(["--config",pending,operation,&destination,"--contimeout","8s","--timeout","15s","--retries","1","--low-level-retries","1"]);
        if operation=="lsf"{command.args(["--max-depth","1"]);}
        let result=command.kill_on_drop(true).output();
        if !matches!(time::timeout(Duration::from_secs(30),result).await,Ok(Ok(output)) if output.status.success()){
            let _=fs::remove_file(pending).await;
            return Err(problem(StatusCode::BAD_GATEWAY,"Không xác thực được mã hoặc quyền thư mục Drive. Chạy lại rclone authorize và dán mã mới; nếu vẫn lỗi, dùng cùng phiên bản rclone với máy chủ."));
        }
    }
    let previous=fs::read(DRIVE_CONFIG).await.ok();
    fs::rename(pending,DRIVE_CONFIG).await.map_err(|_|internal())?;
    if write_private(DRIVE_FOLDER,&folder).await.is_err(){
        if let Some(previous)=previous{let _=write_private(DRIVE_CONFIG,&String::from_utf8_lossy(&previous)).await;}
        else{let _=fs::remove_file(DRIVE_CONFIG).await;}
        return Err(internal());
    }
    let _=write_private(DRIVE_STATUS,"Đã kết nối bằng mã rclone. Đang chờ sao lưu.").await;
    Ok(StatusCode::NO_CONTENT)
}
#[cfg(test)]mod drive_import_tests{
    use super::*;
    #[test]fn token_requires_refresh_and_keeps_only_oauth_fields(){
        let raw=r#"{"access_token":"test-access","refresh_token":"test-refresh","token_type":"Bearer","expiry":"2026-11-22T07:00:00Z","client_secret":"ignored"}"#;
        let token=drive_token(raw).unwrap();
        assert!(token.get("client_secret").is_none());
        assert_eq!(token["token_type"],"Bearer");
        assert!(drive_token(r#"{"access_token":"test","expiry":"2026-11-22T07:00:00Z"}"#).is_err());
        assert!(drive_token("not JSON").is_err());
    }
    #[test]fn restricts_drive_folder(){
        assert_eq!(drive_folder(" Family/Camera ").unwrap(),"Family/Camera");
        for folder in ["../other","drive:folder","safe/../other","safe\n[other]","safe//other"]{assert!(drive_folder(folder).is_err());}
    }
}


async fn engine_status(State(app):State<App>)->Result<Json<Value>,Problem>{
 let status=app.client.get(format!("{}/api/stats",app.frigate)).send().await.map_err(|_|internal())?
 .json::<Value>().await.map_err(|_|internal())?;
 Ok(Json(status))
}
