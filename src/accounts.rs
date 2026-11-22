use crate::{camera_id, App};
use axum::{
    extract::{Extension, Request, State},
    http::{header, HeaderMap, Method, StatusCode},
    middleware::Next,
    response::{Html, IntoResponse, Redirect, Response},
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{collections::BTreeMap, os::unix::fs::{MetadataExt, PermissionsExt}, path::Path, process::Stdio, sync::LazyLock, time::{Duration, Instant}};
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
    session_id: Option<String>,
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
        .route("/login", get(login_page))
        .route("/api/login", post(login))
        .route("/api/logout", post(logout))
        .route("/api/accounts", get(list).post(save))
}

pub async fn authorize(mut request: Request, next: Next) -> Response {
    let headers = request.headers();
    let token = fs::read_to_string(TOKEN).await.unwrap_or_default();
    let supplied = headers.get("x-camerahome-proxy").and_then(|h| h.to_str().ok());
    let expected = token.trim();
    if expected.len() < 32 || !supplied.is_some_and(|value| equal(value,expected)) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let path = request.extensions().get::<axum::extract::OriginalUri>()
        .map_or_else(|| request.uri().path(), |original| original.0.path());
    if matches!((request.method().as_str(),path),("GET","/login")|("POST","/api/login")|("POST","/api/logout")) {
        return next.run(request).await;
    }
    let sid = session_cookie(headers);
    let logged = if let Some(id) = &sid { session_user(id).await } else { None };
    let trusted = headers.get("x-camerahome-user").and_then(|h| h.to_str().ok()).filter(|v| valid_user(v));
    let user = logged.as_deref().or(trusted);
    let Some(user) = user else {
        return if path=="/" && request.method()==Method::GET { Redirect::to("/login").into_response() }
        else { (StatusCode::UNAUTHORIZED,"Cần đăng nhập CameraHome.").into_response() };
    };
    let principal = if user == "admin" {
        Principal {user: user.to_owned(),session_id: sid.clone().filter(|_|logged.is_some()),admin: true,access: Access::default()}
    } else {
        let Ok(accounts) = directory().await else { return internal().into_response(); };
        let Some(account) = accounts.get(user).filter(|a| a.enabled) else {
            return (StatusCode::FORBIDDEN, "Tài khoản chưa được cấp quyền.").into_response();
        };
        Principal {user: user.to_owned(),session_id: sid.clone().filter(|_|logged.is_some()),admin: false,access: account.access.clone()}
    };
    if !permits(&principal, request.method(), path) {
        return (StatusCode::FORBIDDEN, "Tài khoản không có quyền thực hiện thao tác này.").into_response();
    }
    request.extensions_mut().insert(principal);
    let mut response=next.run(request).await;
    if logged.is_some() {
        if let Some(id)=sid {
            if let Ok(cookie)=session_header(&id).parse() {response.headers_mut().append(header::SET_COOKIE,cookie);}
        }
    }
    response
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
    if let Some(id)=&principal.session_id {
        if session_user(id).await.as_deref()!=Some(principal.user.as_str()) {return false;}
    }
    principal.admin || directory().await.is_ok_and(|accounts| accounts.get(&principal.user)
        .is_some_and(|account| account.enabled && account.access.live && account.access.cameras.iter().any(|camera| camera == id)))
}

async fn session(Extension(user): Extension<Principal>) -> Json<Value> {
    Json(json!({"user":user.user,"admin":user.admin,"access":user.access}))
}

type Sessions = BTreeMap<String,(String,u64)>;
const SESSION_FILE: &str = "/app-data/sessions.json";
static SESSIONS: LazyLock<tokio::sync::Mutex<Option<Sessions>>> = LazyLock::new(Default::default);
fn epoch() -> u64 {std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs()}
async fn sessions() -> Result<tokio::sync::MappedMutexGuard<'static,Sessions>,Problem> {
    let mut guard=SESSIONS.lock().await;
    if guard.is_none() {
        let loaded=match fs::read(SESSION_FILE).await {
            Ok(raw)=>serde_json::from_slice::<Sessions>(&raw).map_err(|_|internal())?,
            Err(e) if e.kind()==std::io::ErrorKind::NotFound=>BTreeMap::new(),
            Err(_)=>return Err(internal()),
        };
        *guard=Some(loaded);
    }
    Ok(tokio::sync::MutexGuard::map(guard,|value|value.as_mut().unwrap()))
}
async fn persist_sessions(items:&Sessions)->Result<(),Problem> {
    atomic_write(SESSION_FILE,&serde_json::to_vec(items).map_err(|_|internal())?,0o600,None).await
}
fn session_header(id:&str)->String {
    format!("{COOKIE}={id}; Path=/; Secure; HttpOnly; SameSite=Lax; Max-Age={SESSION_TTL}")
}
static ATTEMPTS: LazyLock<tokio::sync::Mutex<BTreeMap<String,(Instant,u8)>>> = LazyLock::new(Default::default);
static VERIFY: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(4);
const COOKIE: &str = "__Host-camerahome";
const SESSION_TTL: u64 = 400*24*60*60;

fn equal(a:&str,b:&str)->bool {
    a.len()==b.len() && a.bytes().zip(b.bytes()).fold(0u8,|diff,(x,y)|diff|(x^y))==0
}
fn session_cookie(headers:&HeaderMap)->Option<String> {
    headers.get(header::COOKIE)?.to_str().ok()?.split(';').find_map(|part| {
        let (key,v)=part.trim().split_once('=')?;
        (key==COOKIE && v.len()==64 && v.bytes().all(|c|c.is_ascii_hexdigit())).then(||v.to_owned())
    })
}
async fn session_user(id:&str)->Option<String> {
    let mut items=sessions().await.ok()?;
    let now=epoch();
    let (name,expiry)=items.get(id).filter(|(_,expiry)|*expiry>now)?.clone();
    if expiry.saturating_sub(now)<SESSION_TTL-86400 {
        items.insert(id.to_owned(),(name.clone(),now+SESSION_TTL));
        if persist_sessions(&items).await.is_err(){return None;}
    }
    Some(name)
}
async fn revoke(user:&str)->Result<(),Problem> {
    let mut items=sessions().await?;
    items.retain(|_,(name,_)|name!=user);
    persist_sessions(&items).await
}
async fn login_page() -> impl IntoResponse {
    ([(header::CACHE_CONTROL,"no-store")],Html(include_str!("../web/login.html")))
}
#[derive(Deserialize)]
struct Login {user:String,password:String}

async fn login(headers:HeaderMap,Json(data):Json<Login>)->Result<Response,Problem> {
    crate::manage::require_origin(&headers)?;
    let bad=(StatusCode::UNAUTHORIZED,"Tên tài khoản hoặc mật khẩu không đúng.");
    if !valid_user(&data.user) || data.password.is_empty() || data.password.len()>128 || data.password.chars().any(char::is_control) {return Err(bad);}
    let now=Instant::now();
    let ip=headers.get("x-camerahome-client").and_then(|v|v.to_str().ok()).unwrap_or("local").to_owned();
    {
        let mut attempts=ATTEMPTS.lock().await;
        attempts.retain(|_,(at,_)|now.duration_since(*at)<Duration::from_secs(60));
        if attempts.len()>=4096 && !attempts.contains_key(&ip) {return Err((StatusCode::TOO_MANY_REQUESTS,"Thử lại sau."));}
        let entry=attempts.entry(ip).or_insert((now,0));
        if entry.1>=12 {return Err((StatusCode::TOO_MANY_REQUESTS,"Bạn đã thử quá nhiều lần. Hãy đợi một phút."));}
        entry.1+=1;
    }
    let _slot=VERIFY.try_acquire().map_err(|_|(StatusCode::TOO_MANY_REQUESTS,"Thử lại sau."))?;
    let users=fs::read_to_string(USERS).await.map_err(|_|internal())?;
    let hash=users.lines().filter_map(|line|line.split_once(':')).find(|(name,_)|*name==data.user).map(|(_,hash)|hash);
    let enabled=data.user=="admin" || directory().await?.get(&data.user).is_some_and(|entry|entry.enabled);
    let salt=hash.and_then(|value|value.strip_prefix("$6$")?.split_once('$')).map(|(salt,_)|salt);
    let computed=hash_with_salt(&data.password,Some(salt.unwrap_or("camerahomeDummy"))).await?;
    if !enabled || !hash.is_some_and(|expected|equal(expected,&computed)) {return Err(bad);}
    let mut bytes=[0u8;32];getrandom::fill(&mut bytes).map_err(|_|internal())?;
    let id=bytes.iter().map(|v|format!("{v:02x}")).collect::<String>();
    let mut sessions=sessions().await?;
    sessions.retain(|_,(_,expiry)|*expiry>epoch());
    if let Some(old)=session_cookie(&headers) {sessions.remove(&old);}
    if sessions.len()>=4096 {return Err((StatusCode::SERVICE_UNAVAILABLE,"Thử lại sau."));}
    sessions.insert(id.clone(),(data.user,epoch()+SESSION_TTL));
    persist_sessions(&sessions).await?;
    let cookie=session_header(&id);
    Ok(([(header::SET_COOKIE,cookie),(header::CACHE_CONTROL,"no-store".to_owned())],StatusCode::NO_CONTENT).into_response())
}

async fn logout(headers:HeaderMap)->Result<Response,Problem> {
    crate::manage::require_origin(&headers)?;
    if let Some(id)=session_cookie(&headers) {let mut items=sessions().await?;items.remove(&id);persist_sessions(&items).await?;}
    let expired=format!("{COOKIE}=; Path=/; Secure; HttpOnly; SameSite=Lax; Max-Age=0");
    Ok(([(header::SET_COOKIE,expired),(header::CACHE_CONTROL,"no-store".to_owned())],StatusCode::NO_CONTENT).into_response())
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

async fn hash_password(password: &str) -> Result<String, Problem> {hash_with_salt(password,None).await}

async fn hash_with_salt(password:&str,salt:Option<&str>)->Result<String,Problem> {
    let mut command=Command::new("openssl");
    command.args(["passwd","-6","-stdin"]);
    if let Some(salt)=salt {command.args(["-salt",salt]);}
    let mut child=command.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null()).kill_on_drop(true).spawn().map_err(|_| internal())?;
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
    if !valid_user(&data.user) || data.access.cameras.len() > 100
        || data.access.cameras.iter().any(|id| !camera_id(id))
        || (!data.password.is_empty() && (!(6..=128).contains(&data.password.chars().count()) || data.password.chars().any(char::is_control))) {
        return Err(bad());
    }
    let _lock = app.management.lock().await;
    if data.user=="admin" {
        if data.password.is_empty() {return Err(bad());}
        let original=fs::read_to_string(USERS).await.map_err(|_|internal())?;
        let group=fs::metadata(USERS).await.map_err(|_|internal())?.gid();
        let hash=hash_password(&data.password).await?;
        let mut updated=format!("admin:{hash}\n");
        for line in original.lines().filter(|line|!line.starts_with("admin:")) {updated.push_str(line);updated.push('\n');}
        atomic_write(USERS,updated.as_bytes(),0o640,Some(group)).await?;
        revoke("admin").await?;
        return Ok(StatusCode::NO_CONTENT);
    }
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
    let invalidate=!data.password.is_empty() || !data.enabled;
    let username=data.user.clone();
    accounts.insert(data.user,Account {enabled:data.enabled,access:data.access,hash});
    let original = fs::read_to_string(USERS).await.map_err(|_| internal())?;
    let group = fs::metadata(USERS).await.map_err(|_| internal())?.gid();
    let admin = original.lines().find(|line| line.starts_with("admin:")).ok_or_else(internal)?;
    let mut credentials = format!("{admin}\n");
    for line in original.lines() {
        if let Some((name,_))=line.split_once(':') {
            if name!="admin" && valid_user(name) && !accounts.contains_key(name) {credentials.push_str(line);credentials.push('\n');}
        }
    }
    for (id,account) in &accounts {
        if account.enabled { credentials.push_str(&format!("{id}:{}\n",account.hash)); }
    }
    atomic_write(POLICY,&serde_json::to_vec(&accounts).map_err(|_| internal())?,0o600,None).await?;
    if let Err(error) = atomic_write(USERS,credentials.as_bytes(),0o640,Some(group)).await {
        let _ = atomic_write(POLICY,&previous,0o600,None).await;
        return Err(error);
    }
    if invalidate {revoke(&username).await?;}
    Ok(StatusCode::NO_CONTENT)
}
