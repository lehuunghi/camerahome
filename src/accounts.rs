use crate::{camera_id, App};
use axum::{
    extract::{Extension, Request, State},
    http::{Method, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{collections::BTreeMap, os::unix::fs::{MetadataExt, PermissionsExt}, path::Path, process::Stdio, time::Duration};
use tokio::{fs, io::AsyncWriteExt, process::Command};
const USERS: &str = "/nginx-auth/users";
const POLICY: &str = "/nginx-auth/accounts.json";
const TOKEN: &str = "/nginx-auth/proxy-token";

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Access {
    pub cameras: Vec<String>,
    pub live: bool,
    pub recordings: bool,
    pub ptz: bool,
    pub settings: bool,
}

#[derive(Clone)]
pub struct Principal {
    user: String,
    pub admin: bool,
    pub access: Access,
}

impl Principal {
    pub fn has_camera(&self, id: &str) -> bool {
        self.admin || self.access.cameras.iter().any(|name| name == id)
    }
}

#[derive(Clone, Deserialize, Serialize)]
struct Account {
    enabled: bool,
    access: Access,
    hash: String,
}

type Directory = BTreeMap<String, Account>;
type Problem = (StatusCode, &'static str);

fn internal() -> Problem {
    (StatusCode::INTERNAL_SERVER_ERROR, "Không cập nhật được tài khoản.")
}

async fn directory() -> Result<Directory, Problem> {
    match fs::read(POLICY).await {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(|_| internal()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(BTreeMap::new()),
        Err(_) => Err(internal()),
    }
}

pub fn routes() -> Router<App> {
    Router::new()
        .route("/api/session", get(session))
        .route("/api/accounts", get(list).post(save))
}

pub async fn authorize(mut request: Request, next: Next) -> Response {
    let headers = request.headers();
    let token = fs::read_to_string(TOKEN).await.unwrap_or_default();
    let supplied = headers.get("x-camerahome-proxy").and_then(|h| h.to_str().ok());
    let user = headers.get("x-camerahome-user").and_then(|h| h.to_str().ok()).unwrap_or("");
    let expected = token.trim().as_bytes();
    let constant_match = supplied.is_some_and(|value| value.len()==expected.len() && value.bytes().zip(expected)
        .fold(0u8,|difference,(a,b)| difference | (a ^ *b))==0);
    if expected.len() < 32 || !constant_match || !valid_user(user) {
        return (StatusCode::UNAUTHORIZED, "Cần đăng nhập CameraHome.").into_response();
    }
    let principal = if user == "admin" {
        Principal {user: user.to_owned(),admin: true,access: Access::default()}
    } else {
        let Ok(accounts) = directory().await else { return internal().into_response(); };
        let Some(account) = accounts.get(user).filter(|a| a.enabled) else {
            return (StatusCode::FORBIDDEN, "Tài khoản chưa được cấp quyền.").into_response();
        };
        Principal {user: user.to_owned(),admin: false,access: account.access.clone()}
    };
    let path = request.extensions().get::<axum::extract::OriginalUri>()
        .map_or_else(|| request.uri().path(), |original| original.0.path());
    if !permits(&principal, request.method(), path) {
        return (StatusCode::FORBIDDEN, "Tài khoản không có quyền thực hiện thao tác này.").into_response();
    }
    request.extensions_mut().insert(principal);
    next.run(request).await
}

fn permits(p: &Principal, method: &Method, path: &str) -> bool {
    if p.admin { return true; }
    let read = method == Method::GET || method == Method::HEAD;
    if read && matches!(path, "/" | "/api/session" | "/api/cameras") { return true; }
    if path == "/api/clips" { return read && p.access.recordings; }
    for (prefix, allowed) in [
        ("/api/live/", p.access.live && read),
        ("/api/snapshot/", p.access.live && read),
        ("/api/clips/latest/", p.access.recordings && read),
        ("/api/ptz/", p.access.ptz && (read || method == Method::POST)),
    ] {
        if let Some(id) = path.strip_prefix(prefix) {
            return allowed && camera_id(id) && p.has_camera(id);
        }
    }
    if let Some(id) = path.strip_prefix("/api/cameras/").and_then(|v| v.strip_suffix("/settings")) {
        return p.access.settings && (read || method == Method::POST) && camera_id(id) && p.has_camera(id);
    }
    if let Some(relative) = path.strip_prefix("/media/") {
        let parts: Vec<_> = relative.split('/').collect();
        return read && p.access.recordings && parts.len()==4 && crate::valid_recording_day(parts[0])
            && parts[1].len()==2 && parts[1].bytes().all(|c|c.is_ascii_digit())
            && camera_id(parts[2]) && p.has_camera(parts[2])
            && parts[3].strip_suffix(".mp4").is_some_and(|name| name.len()==5 && name.as_bytes()[2]==b'.'
                && name.bytes().enumerate().all(|(i,c)|i==2||c.is_ascii_digit()));
    }
    false
}

pub async fn still_allowed_live(principal: &Principal, id: &str) -> bool {
    principal.admin || directory().await.is_ok_and(|accounts| accounts.get(&principal.user)
        .is_some_and(|account| account.enabled && account.access.live && account.access.cameras.iter().any(|camera| camera == id)))
}

async fn session(Extension(user): Extension<Principal>) -> Json<Value> {
    Json(json!({"user":user.user,"admin":user.admin,"access":user.access}))
}

async fn list() -> Result<Json<Value>, Problem> {
    let policy=directory().await?;
    let auth=fs::read_to_string(USERS).await.map_err(|_|internal())?;
    let active:BTreeMap<_,_>=auth.lines().filter_map(|line|line.split_once(':')).filter(|(name,_)|valid_user(name)).collect();
    let mut names:std::collections::BTreeSet<_>=policy.keys().map(String::as_str).chain(active.keys().copied()).collect();
    let mut accounts=Vec::new();
    if names.remove("admin") {accounts.push(json!({"user":"admin","enabled":active.contains_key("admin"),"admin":true,"access":Access::default()}));}
    for user in names {
        let entry=policy.get(user);
        accounts.push(json!({"user":user,"enabled":active.contains_key(user)&&entry.is_some_and(|e|e.enabled),
            "access":entry.map(|e|e.access.clone()).unwrap_or_default()}));
    }
    Ok(Json(json!(accounts)))
}

#[derive(Deserialize)]
struct Update {
    user: String,
    password: String,
    enabled: bool,
    access: Access,
}

fn valid_user(user: &str) -> bool {
    (1..=32).contains(&user.len()) && user.bytes().all(|c| c.is_ascii_alphanumeric() || b"_-".contains(&c))
}

async fn hash_password(password: &str) -> Result<String, Problem> {
    let mut child = Command::new("openssl").args(["passwd","-6","-stdin"])
        .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null()).kill_on_drop(true).spawn().map_err(|_| internal())?;
    let mut input = child.stdin.take().ok_or_else(internal)?;
    input.write_all(format!("{password}\n").as_bytes()).await.map_err(|_| internal())?;
    drop(input);
    let result = tokio::time::timeout(Duration::from_secs(5), child.wait_with_output()).await
        .map_err(|_| internal())?.map_err(|_| internal())?;
    let hash = String::from_utf8(result.stdout).map_err(|_| internal())?.trim().to_owned();
    if !result.status.success() || !hash.starts_with("$6$") { return Err(internal()); }
    Ok(hash)
}

async fn atomic_write(path: &str, value: &[u8], mode: u32, group: Option<u32>) -> Result<(), Problem> {
    let temp = format!("{path}.staged");
    fs::write(&temp,value).await.map_err(|_| internal())?;
    fs::set_permissions(&temp,std::fs::Permissions::from_mode(mode)).await.map_err(|_| internal())?;
    if let Some(group) = group { std::os::unix::fs::chown(Path::new(&temp), Some(0), Some(group)).map_err(|_| internal())?; }
    fs::rename(&temp,path).await.map_err(|_| internal())
}

async fn save(State(app): State<App>, headers: axum::http::HeaderMap, Json(mut data): Json<Update>) -> Result<StatusCode, Problem> {
    crate::manage::require_origin(&headers)?;
    let bad = || (StatusCode::BAD_REQUEST, "Kiểm tra tên tài khoản, mật khẩu và quyền camera.");
    if !valid_user(&data.user) || data.user == "admin" || data.access.cameras.len() > 100
        || data.access.cameras.iter().any(|id| !camera_id(id))
        || (!data.password.is_empty() && (!(12..=128).contains(&data.password.len()) || data.password.chars().any(char::is_control))) {
        return Err(bad());
    }
    let _lock = app.management.lock().await;
    let cfg = crate::manage::read_config().await?;
    if data.access.cameras.iter().any(|id| cfg["cameras"].get(id).is_none()) { return Err(bad()); }
    data.access.cameras.sort_unstable();
    data.access.cameras.dedup();
    let mut accounts = directory().await?;
    if accounts.len() >= 100 && !accounts.contains_key(&data.user) { return Err(bad()); }
    let previous = serde_json::to_vec(&accounts).map_err(|_| internal())?;
    let hash = if data.password.is_empty() {
        accounts.get(&data.user).ok_or_else(bad)?.hash.clone()
    } else { hash_password(&data.password).await? };
    accounts.insert(data.user,Account {enabled:data.enabled,access:data.access,hash});
    let original = fs::read_to_string(USERS).await.map_err(|_| internal())?;
    let group = fs::metadata(USERS).await.map_err(|_| internal())?.gid();
    let admin = original.lines().find(|line| line.starts_with("admin:")).ok_or_else(internal)?;
    let mut credentials = format!("{admin}\n");
    for (id,account) in &accounts {
        if account.enabled { credentials.push_str(&format!("{id}:{}\n",account.hash)); }
    }
    atomic_write(POLICY,&serde_json::to_vec(&accounts).map_err(|_| internal())?,0o600,None).await?;
    if let Err(error) = atomic_write(USERS,credentials.as_bytes(),0o640,Some(group)).await {
        let _ = atomic_write(POLICY,&previous,0o600,None).await;
        return Err(error);
    }
    Ok(StatusCode::NO_CONTENT)
}
