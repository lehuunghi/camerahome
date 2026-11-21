mod manage;
use axum::{extract::{Query, State}, http::{header, StatusCode}, response::{Html, IntoResponse}, routing::get, Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{env, fs, path::PathBuf, time::Duration};
use tower_http::services::ServeDir;

#[derive(Clone)]
struct App {
    client: reqwest::Client,
    frigate: String,
    recordings: PathBuf,
    status: PathBuf,
    management: std::sync::Arc<tokio::sync::Mutex<()>>,
    oauth: std::sync::Arc<tokio::sync::Mutex<Option<manage::OAuthPending>>>,
    drive_lock: std::sync::Arc<tokio::sync::Mutex<()>>,
}

#[derive(Deserialize)]
struct Selection {
    day: String,
    camera: Option<String>,
}

#[derive(Serialize)]
struct Camera {
    id: String,
    fps: Option<f64>,
}

fn camera_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 100 && id.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
}

async fn cameras(State(app): State<App>) -> Result<Json<Vec<Camera>>, StatusCode> {
    let stats: Value = app.client.get(format!("{}/api/stats", app.frigate))
        .send().await.map_err(|_| StatusCode::BAD_GATEWAY)?
        .error_for_status().map_err(|_| StatusCode::BAD_GATEWAY)?
        .json().await.map_err(|_| StatusCode::BAD_GATEWAY)?;
    let items = stats["cameras"].as_object().ok_or(StatusCode::BAD_GATEWAY)?;
    Ok(Json(items.iter().filter(|(id, _)| camera_id(id)).map(|(id, info)| Camera {
        id: id.clone(), fps: info["camera_fps"].as_f64(),
    }).collect()))
}

async fn snapshot(State(app): State<App>, axum::extract::Path(id): axum::extract::Path<String>) -> impl IntoResponse {
    if !camera_id(&id) {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let Ok(response) = app.client.get(format!("{}/api/{}/latest.jpg?h=360", app.frigate, id)).send().await else {
        return StatusCode::BAD_GATEWAY.into_response();
    };
    if !response.status().is_success() {
        return StatusCode::BAD_GATEWAY.into_response();
    }
    match response.bytes().await {
        Ok(bytes) => ([(header::CONTENT_TYPE, "image/jpeg"), (header::CACHE_CONTROL, "no-store")], bytes).into_response(),
        Err(_) => StatusCode::BAD_GATEWAY.into_response(),
    }
}

fn entries(path: &std::path::Path) -> std::io::Result<Vec<fs::DirEntry>> {
    match fs::read_dir(path) {
        Ok(list) => list.collect(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(error),
    }
}

fn clips_for_day(root: PathBuf, filter: Selection) -> std::io::Result<Vec<String>> {
    let mut clips = Vec::new();
    for hour in entries(&root.join(&filter.day))? {
        let hour_name = hour.file_name().to_string_lossy().into_owned();
        if !hour.file_type()?.is_dir() || hour_name.len() != 2 || !hour_name.bytes().all(|c| c.is_ascii_digit()) { continue; }
        for camera in entries(&hour.path())? {
            let id = camera.file_name().to_string_lossy().into_owned();
            if !camera.file_type()?.is_dir() || !camera_id(&id) || filter.camera.as_ref().is_some_and(|name| name != &id) { continue; }
            for clip in entries(&camera.path())? {
                let name = clip.file_name().to_string_lossy().into_owned();
                if clip.file_type()?.is_file() && name.ends_with(".mp4") && name.bytes().all(|c| c.is_ascii_digit() || c == b'.' || matches!(c, b'm' | b'p' | b'4')) {
                    clips.push(format!("{}/{}/{}/{}", filter.day, hour_name, id, name));
                }
            }
        }
    }
    clips.sort_unstable_by(|a,b| {
        let parts = |s: &str| s.split('/').nth(3).unwrap_or_default().to_owned();
        b.get(..13).cmp(&a.get(..13)).then_with(|| parts(b).cmp(&parts(a))).then_with(|| a.cmp(b))
    });
    clips.truncate(250);
    Ok(clips)
}

async fn clips(State(app): State<App>, Query(filter): Query<Selection>) -> Result<Json<Vec<String>>, StatusCode> {
    let valid_day = valid_recording_day(&filter.day);
    if !valid_day || filter.camera.as_deref().is_some_and(|id| !camera_id(id)) {
        return Err(StatusCode::BAD_REQUEST);
    }
    tokio::task::spawn_blocking(move || clips_for_day(app.recordings, filter))
        .await.map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .map(Json).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

async fn latest_clips(State(app): State<App>, axum::extract::Path(id): axum::extract::Path<String>) -> Result<Json<Value>, StatusCode> {
    if !camera_id(&id) { return Err(StatusCode::BAD_REQUEST); }
    let result = tokio::task::spawn_blocking(move || -> std::io::Result<Value> {
        let mut days = entries(&app.recordings)?;
        days.sort_unstable_by_key(|entry| std::cmp::Reverse(entry.file_name()));
        for entry in days.into_iter().take(31) {
            let day = entry.file_name().to_string_lossy().into_owned();
            if !entry.file_type()?.is_dir() || !valid_recording_day(&day) { continue; }
            let clips = clips_for_day(app.recordings.clone(), Selection {day: day.clone(), camera: Some(id.clone())})?;
            if !clips.is_empty() { return Ok(serde_json::json!({"day":day,"clips":clips})); }
        }
        Ok(serde_json::json!({"day":null,"clips":[]}))
    }).await.map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    result.map(Json).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

fn valid_recording_day(day: &str) -> bool {
    day.len() == 10 && day.bytes().enumerate()
        .all(|(i,c)| if i == 4 || i == 7 { c == b'-' } else { c.is_ascii_digit() })
}

async fn backup(State(app): State<App>) -> impl IntoResponse {
    let state = tokio::fs::read_to_string(app.status.join("latest.txt")).await
        .unwrap_or_else(|_| "Chưa có lượt sao lưu.".into());
    ([(header::CACHE_CONTROL, "no-store")], state)
}

#[tokio::main]
async fn main() {
    let app = App {
        client: reqwest::Client::builder().timeout(Duration::from_secs(5)).build().expect("HTTP client"),
        frigate: env::var("FRIGATE_URL").unwrap_or_else(|_| "http://frigate:5000".into()).trim_end_matches('/').to_owned(),
        recordings: env::var("RECORDINGS_DIR").unwrap_or_else(|_| "/recordings".into()).into(),
        management: Default::default(),
        drive_lock: Default::default(),
        oauth: Default::default(),
        status: env::var("STATUS_DIR").unwrap_or_else(|_| "/backup-status".into()).into(),
    };
    tokio::spawn(manage::backup_worker(app.clone()));
    let router = Router::new()
        .merge(manage::routes())
        .route("/", get(|| async { Html(include_str!("../web/index.html")) }))
        .route("/api/cameras", get(cameras))
        .route("/api/snapshot/{id}", get(snapshot))
        .route("/api/clips", get(clips))
        .route("/api/clips/latest/{id}", get(latest_clips))
        .route("/api/backup", get(backup))
        .nest_service("/media", ServeDir::new(&app.recordings))
        .with_state(app);
    let listener = tokio::net::TcpListener::bind("0.0.0.0:8080").await.expect("listen");
    axum::serve(listener, router).await.expect("serve");
}
