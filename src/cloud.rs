use crate::{App,accounts::Principal,camera_id};
use axum::{body::Body,extract::{State,Extension,Path,Query},http::{header,HeaderMap,Method,StatusCode},response::{Response,IntoResponse},routing::{get,post},Json,Router};
use serde::{Deserialize,Serialize};
use serde_json::{json,Value};
use std::{collections::BTreeMap,process::Stdio,sync::{Arc,LazyLock},time::Duration};
use tokio::{fs,io::{AsyncReadExt,AsyncSeekExt},process::Command,time};
type Problem=(StatusCode,&'static str);
const R2_FILE:&str="/app-data/r2.json";
const POLICY:&str="/app-data/cloud-policy.json";
static JOBS:LazyLock<Arc<tokio::sync::Semaphore>>=LazyLock::new(||Arc::new(tokio::sync::Semaphore::new(8)));
fn error()->Problem{(StatusCode::BAD_GATEWAY,"Không truy cập được kho lưu trữ. Kiểm tra kết nối và quyền truy cập.")}
fn bad()->Problem{(StatusCode::BAD_REQUEST,"Thông tin lưu trữ không hợp lệ.")}
async fn private(path:&str,bytes:&[u8])->Result<(),Problem>{
 let temp=format!("{path}.new");fs::write(&temp,bytes).await.map_err(|_|error())?;
 use std::os::unix::fs::PermissionsExt;
 fs::set_permissions(&temp,std::fs::Permissions::from_mode(0o600)).await.map_err(|_|error())?;
 fs::rename(temp,path).await.map_err(|_|error())
}
#[derive(Clone,Serialize,Deserialize)]
struct R2 {account_id:String,bucket:String,prefix:String,access_key:String,secret_key:String,days:u32,enabled:bool}
#[derive(Clone,Serialize,Deserialize)]
#[serde(default)]
pub(crate) struct Policy {drive_days:u32,drive_enabled:bool,local_days:u32,backup_enabled:bool,local_enabled:bool,r2_enabled:Option<bool>,local_off_since:Option<i64>,local_retention_seconds:Option<u64>,r2_retention_seconds:Option<u64>,local_retention_unit:Option<String>,r2_retention_unit:Option<String>}
impl Default for Policy{fn default()->Self{Self{drive_days:0,drive_enabled:true,local_days:0,backup_enabled:true,local_enabled:true,r2_enabled:None,local_off_since:None,local_retention_seconds:None,r2_retention_seconds:None,local_retention_unit:None,r2_retention_unit:None}}}
async fn r2()->Result<R2,Problem>{serde_json::from_slice(&fs::read(R2_FILE).await.map_err(|_|(StatusCode::CONFLICT,"Chưa kết nối Cloudflare R2."))?).map_err(|_|error())}
pub(crate) async fn policy()->Policy{fs::read(POLICY).await.ok().and_then(|v|serde_json::from_slice(&v).ok()).unwrap_or_default()}
struct Remote {config:String,target:String,env:Vec<(String,String)>}
fn r2_remote(c:&R2)->Remote{
 Remote{config:"/dev/null".into(),target:format!("r2:{}/{}/recordings",c.bucket,c.prefix),env:vec![
 ("RCLONE_CONFIG_R2_TYPE".into(),"s3".into()),("RCLONE_CONFIG_R2_PROVIDER".into(),"Cloudflare".into()),
 ("RCLONE_CONFIG_R2_ACCESS_KEY_ID".into(),c.access_key.clone()),("RCLONE_CONFIG_R2_SECRET_ACCESS_KEY".into(),c.secret_key.clone()),
 ("RCLONE_CONFIG_R2_ENDPOINT".into(),format!("https://{}.r2.cloudflarestorage.com",c.account_id)),
 ("RCLONE_CONFIG_R2_REGION".into(),"auto".into()),("RCLONE_CONFIG_R2_NO_CHECK_BUCKET".into(),"true".into())]}
}
async fn remote(source:&str)->Result<Remote,Problem>{
 match source {
 "r2"=>Ok(r2_remote(&r2().await?)),
 "drive"=>{
 let folder=fs::read_to_string("/app-data/drive-folder").await.map_err(|_|(StatusCode::CONFLICT,"Chưa kết nối Google Drive."))?;
 if !fs::try_exists("/app-data/drive.conf").await.unwrap_or(false){return Err(error());}
 Ok(Remote{config:"/app-data/drive.conf".into(),target:format!("drive:{folder}/recordings"),env:vec![]})
 },_=>Err(bad())}
}
fn command(remote:&Remote)->Command{
 let mut c=Command::new("rclone");
 c.args(["--config",&remote.config,"--contimeout","10s","--timeout","90s","--retries","1","--low-level-retries","5"]).envs(remote.env.iter().cloned()).kill_on_drop(true);
 if remote.config=="/app-data/drive.conf" {c.args(["--tpslimit","1","--tpslimit-burst","1","--drive-pacer-min-sleep","1s","--drive-pacer-burst","1","--fast-list"]);}
 c
}
fn transfer_error(stderr:&[u8])->Problem {
 let message=String::from_utf8_lossy(stderr).to_ascii_lowercase();
 let text=if message.contains("failed to load filters")||message.contains("unknown flag") {
  "Lệnh sao lưu không tương thích với phiên bản rclone."
 }else if message.contains("invalid_grant")||message.contains("invalid_client")||message.contains("unauthorized_client"){
  "Quyền Google Drive đã hết hiệu lực. Kết nối lại bằng OAuth riêng trong mục Google Drive."
 }else if message.contains("ratelimitexceeded")||message.contains("rate_limit_exceeded")||message.contains("rate limit"){
  "Google Drive đang giới hạn lượt gọi API. Hệ thống sẽ chờ và tự thử lại."
 }else if message.contains("storagequotaexceeded")||message.contains("storage quota"){
  "Google Drive đã hết dung lượng lưu trữ."
 }else if message.contains("insufficient")||message.contains("access denied")||message.contains("forbidden"){
  "Kho lưu trữ từ chối quyền đọc hoặc ghi. Kiểm tra quyền truy cập."
 }else{"Không truy cập được kho lưu trữ. Kiểm tra kết nối và quyền truy cập."};
 (StatusCode::BAD_GATEWAY,text)
}
async fn output(remote:&Remote,args:&[&str],seconds:u64)->Result<Vec<u8>,Problem>{
 let _slot=JOBS.clone().try_acquire_owned().map_err(|_|(StatusCode::TOO_MANY_REQUESTS,"Kho lưu trữ đang bận. Thử lại sau."))?;
 let result=time::timeout(Duration::from_secs(seconds),command(remote).args(args).output()).await.map_err(|_|error())?.map_err(|_|error())?;
 if result.status.success(){Ok(result.stdout)}else{Err(transfer_error(&result.stderr))}
}
pub fn routes()->Router<App>{
 Router::new().route("/api/storage",get(settings).post(save_policy))
 .route("/api/storage/r2",post(save_r2))
 .route("/api/storage/retention",post(save_retention))
 .route("/api/storage/backup",post(save_backup))
 .route("/api/cloud/clips",get(clips))
 .route("/api/cloud/media/{source}/{*path}",get(media))
}
async fn settings()->Json<Value>{
 let policy=policy().await;
 let r=r2().await.ok();
 let last=fs::read_to_string("/app-data/r2-status").await.unwrap_or_default();
 Json(json!({"local_retention_unit":policy.local_retention_unit.as_deref().unwrap_or("days"),"r2_retention_unit":policy.r2_retention_unit.as_deref().unwrap_or("days"),"local_retention_seconds":policy.local_retention_seconds.unwrap_or(u64::from(policy.local_days)*86400),"r2_retention_seconds":policy.r2_retention_seconds.unwrap_or(r.as_ref().map_or(30*86400,|c|u64::from(c.days)*86400)),"backup_enabled":policy.backup_enabled,"local_enabled":policy.local_enabled,"r2_enabled":r.as_ref().is_some_and(|c|policy.r2_enabled.unwrap_or(c.enabled)),"drive_configured":remote("drive").await.is_ok(),"drive_days":policy.drive_days,"drive_enabled":policy.drive_enabled,"local_days":policy.local_days,"r2":r.map(|c|json!({"account_id":c.account_id,"bucket":c.bucket,"prefix":c.prefix,"days":c.days,"enabled":c.enabled,"configured":true})),"r2_status":last,"local_status":fs::read_to_string("/app-data/local-retention-status").await.unwrap_or_default()}))
}
#[derive(Deserialize)]
struct RetentionEdit {drive_days:u32,local_days:u32}
async fn save_policy(State(app):State<App>,headers:HeaderMap,Json(edit):Json<RetentionEdit>)->Result<StatusCode,Problem>{
 crate::manage::require_origin(&headers)?;
 if edit.drive_days>3650||edit.local_days>3650{return Err(bad());}
 let _lock=app.management.lock().await;
 let mut p=policy().await;p.drive_days=edit.drive_days;p.local_days=edit.local_days;
 private(POLICY,&serde_json::to_vec(&p).map_err(|_|bad())?).await?;
 Ok(StatusCode::NO_CONTENT)
}
#[derive(Deserialize)]
struct DurationInput {value:u64,unit:String}
impl DurationInput {
 fn seconds(&self)->Result<u64,Problem>{
  let multiplier=match self.unit.as_str(){"hours"=>3600,"days"=>86400,_=>return Err(bad())};
  self.value.checked_mul(multiplier).filter(|seconds|*seconds<=3650*86400).ok_or_else(bad)
 }
}
#[derive(Deserialize)]
struct StorageRetention {local:DurationInput,r2:DurationInput}
async fn save_retention(State(app):State<App>,headers:HeaderMap,Json(edit):Json<StorageRetention>)->Result<StatusCode,Problem>{
 crate::manage::require_origin(&headers)?;
 let local=edit.local.seconds()?;let r2=edit.r2.seconds()?;
 let _guard=app.management.lock().await;
 let mut p=policy().await;p.local_retention_seconds=Some(local);p.r2_retention_seconds=Some(r2);p.local_retention_unit=Some(edit.local.unit);p.r2_retention_unit=Some(edit.r2.unit);
 private(POLICY,&serde_json::to_vec(&p).map_err(|_|bad())?).await?;
 Ok(StatusCode::NO_CONTENT)
}
#[derive(Deserialize)]
struct BackupEdit {backup_enabled:bool,local_enabled:bool,drive_enabled:bool,r2_enabled:bool}
async fn save_backup(State(app):State<App>,headers:HeaderMap,Json(edit):Json<BackupEdit>)->Result<StatusCode,Problem>{
 crate::manage::require_origin(&headers)?;
 if edit.drive_enabled&&remote("drive").await.is_err(){return Err((StatusCode::CONFLICT,"Kết nối Google Drive trước khi bật sao lưu."));}
 if edit.r2_enabled&&r2().await.is_err(){return Err((StatusCode::CONFLICT,"Kết nối R2 trước khi bật sao lưu."));}
 let _lock=app.management.lock().await;
 let mut p=policy().await;
 if p.local_enabled&&!edit.local_enabled {p.local_off_since=Some(chrono::Utc::now().timestamp_millis());}
 if edit.local_enabled{p.local_off_since=None;}
 p.backup_enabled=edit.backup_enabled;p.local_enabled=edit.local_enabled;
 p.drive_enabled=edit.drive_enabled;p.r2_enabled=Some(edit.r2_enabled);
 private(POLICY,&serde_json::to_vec(&p).map_err(|_|bad())?).await?;
 Ok(StatusCode::NO_CONTENT)
}
async fn source_enabled(source:&str)->bool{
 let p=policy().await;
 p.backup_enabled&&if source=="drive"{p.drive_enabled}else{r2().await.is_ok_and(|c|p.r2_enabled.unwrap_or(c.enabled))}
}
// Dropping output's future terminates rclone; playback uses the ungated output helper.
async fn backup_output(r:&Remote,args:&[&str],seconds:u64,source:&str)->Result<Vec<u8>,Problem>{
 let work=output(r,args,seconds);tokio::pin!(work);
 loop{tokio::select!{
  result=&mut work=>return result,
  _=time::sleep(Duration::from_secs(2))=>if !source_enabled(source).await{return Err((StatusCode::CONFLICT,"Đã tắt sao lưu nguồn này."));}
 }}
}
fn valid_r2(c:&R2)->bool{
 c.account_id.len()==32 && c.account_id.bytes().all(|b|b.is_ascii_hexdigit())
 && (3..=63).contains(&c.bucket.len()) && c.bucket.bytes().all(|b|b.is_ascii_lowercase()||b.is_ascii_digit()||b==b'-')
 && c.bucket.as_bytes()[0]!=b'-' && !c.bucket.ends_with('-')
 && (1..=3650).contains(&c.days)
 && !c.prefix.is_empty() && c.prefix.len()<=100 && c.prefix.split('/').all(|part|!part.is_empty()&&part!="."&&part!=".."&&part.bytes().all(|b|b.is_ascii_alphanumeric()||b"-_".contains(&b)))
 && [c.access_key.as_str(),c.secret_key.as_str()].iter().all(|k|(16..=256).contains(&k.len())&&!k.chars().any(char::is_control))
}
async fn save_r2(State(app):State<App>,headers:HeaderMap,Json(mut c):Json<R2>)->Result<StatusCode,Problem>{
 crate::manage::require_origin(&headers)?;
 let _lock=app.drive_lock.lock().await;
 if let Ok(previous)=r2().await {
  if previous.account_id==c.account_id {
   if c.access_key.is_empty(){c.access_key=previous.access_key;}
   if c.secret_key.is_empty(){c.secret_key=previous.secret_key;}
  }
 }
 if !valid_r2(&c){return Err(bad());}
 let r=r2_remote(&c);
 output(&r,&["lsf",&r.target,"--dirs-only","--max-depth","1"],30).await?;
 private(R2_FILE,&serde_json::to_vec(&c).map_err(|_|bad())?).await?;
 private("/app-data/r2-status","Đã kiểm tra kết nối R2. Đang chờ sao lưu.".as_bytes()).await?;
 Ok(StatusCode::NO_CONTENT)
}
pub(crate) fn clip_camera(path:&str)->Option<&str>{
 let p:Vec<_>=path.split('/').collect();
 if p.len()!=4 || !crate::valid_recording_day(p[0]) || p[1].len()!=2 || !p[1].bytes().all(|b|b.is_ascii_digit()) || !camera_id(p[2]) {return None;}
 let stem=p[3].strip_suffix(".mp4")?;
 if stem.len()!=5 || stem.as_bytes()[2]!=b'.' || !stem.bytes().enumerate().all(|(i,b)|i==2||b.is_ascii_digit()) {return None;}
 clip_time(path)?;
 Some(p[2])
}
fn clip_time(path:&str)->Option<i64>{
 let p:Vec<_>=path.split('/').collect();if p.len()!=4{return None;}
 let stamp=format!("{} {}.{}",p[0],p[1],p[3].strip_suffix(".mp4")?);
 chrono::NaiveDateTime::parse_from_str(&stamp,"%Y-%m-%d %H.%M.%S").ok().map(|v|v.and_utc().timestamp())
}
#[derive(Deserialize)]struct ClipQuery{source:String,day:String,camera:Option<String>}
#[derive(Default,Serialize,Deserialize)]
#[serde(default)]
struct Catalog{target:String,files:BTreeMap<String,u64>,days:std::collections::BTreeSet<String>,complete:bool}
static CATALOG_LOCK:LazyLock<tokio::sync::Mutex<()>>=LazyLock::new(Default::default);
fn source_name(r:&Remote)->&'static str{if r.config=="/app-data/drive.conf"{"drive"}else{"r2"}}
async fn catalog(r:&Remote)->Catalog{
 let file=format!("/app-data/cloud-catalog-{}.json",source_name(r));
 let stored=fs::read(&file).await.ok().and_then(|v|serde_json::from_slice::<Catalog>(&v).ok()).unwrap_or_default();
 let mut stored=if stored.target==r.target{stored}else{Catalog{target:r.target.clone(),..Catalog::default()}};
 if std::env::var("STORAGE_WORKER").as_deref()==Ok("engine") {
  let base=std::env::var("FRIGATE_URL").unwrap_or_else(|_|"http://engine:5000".into());
  if let Ok(response)=reqwest::Client::new().get(format!("{}/api/catalog",base.trim_end_matches('/')))
   .query(&[("source",source_name(r)),("target",r.target.as_str())]).timeout(Duration::from_secs(5)).send().await {
   if let Ok(value)=response.json::<Value>().await {
    if let Some(files)=value["files"].as_object(){for (path,size) in files{if let Some(size)=size.as_u64(){stored.files.insert(path.clone(),size);}}}
    if let Some(expired)=value["expired"].as_array(){for path in expired.iter().filter_map(Value::as_str){stored.files.remove(path);}}
   }
  }
 }
 stored
}
async fn record_catalog(r:&Remote,entries:Vec<(String,u64)>,day:Option<&str>)->Result<(),Problem>{
 let _guard=CATALOG_LOCK.lock().await;
 let mut data=catalog(r).await;
 if let Some(day)=day{
  data.files.retain(|path,_|!path.starts_with(&format!("{day}/")));
  data.days.insert(day.to_owned());
 }
 for (path,size) in entries{if size>0&&clip_camera(&path).is_some(){data.files.insert(path,size);}}
 private(&format!("/app-data/cloud-catalog-{}.json",source_name(r)),&serde_json::to_vec(&data).map_err(|_|error())?).await
}
async fn prune_catalog(r:&Remote,days:u32)->Result<(),Problem>{
 if days==0{return Ok(());}
 let _guard=CATALOG_LOCK.lock().await;
 let mut data=catalog(r).await;
 let cutoff=chrono::Utc::now().timestamp()-i64::from(days)*86400;
 data.files.retain(|path,_|clip_time(path).is_some_and(|at|at>=cutoff));
 private(&format!("/app-data/cloud-catalog-{}.json",source_name(r)),&serde_json::to_vec(&data).map_err(|_|error())?).await
}
static DRIVE_DISCOVERY:LazyLock<tokio::sync::Mutex<()>>=LazyLock::new(Default::default);
static R2_DISCOVERY:LazyLock<tokio::sync::Mutex<()>>=LazyLock::new(Default::default);
static LIST_ERRORS:LazyLock<tokio::sync::Mutex<BTreeMap<String,(std::time::Instant,Problem)>>>=LazyLock::new(Default::default);
async fn catalog_day(remote:&Remote,day:&str)->Option<Vec<String>>{
 let saved=catalog(remote).await;
 let known:Vec<String>=saved.files.keys().filter(|path|path.starts_with(&format!("{day}/"))&&clip_camera(path).is_some()).cloned().collect();
 (saved.complete||saved.days.contains(day)||!known.is_empty()).then_some(known)
}
static LIST_CACHE:LazyLock<tokio::sync::Mutex<BTreeMap<String,(std::time::Instant,Vec<String>)>>>=LazyLock::new(Default::default);
async fn list_day(remote:&Remote,day:&str)->Result<Vec<String>,Problem>{
 if let Some(known)=catalog_day(remote,day).await{return Ok(known);}
 let _guard=if source_name(remote)=="drive"{DRIVE_DISCOVERY.lock().await}else{R2_DISCOVERY.lock().await};
 if let Some(known)=catalog_day(remote,day).await{return Ok(known);}
 let key=format!("{}/{}",remote.target,day);
 {let errors=LIST_ERRORS.lock().await;if let Some((at,problem))=errors.get(&key){if at.elapsed()<Duration::from_secs(30){return Err(*problem);}}}
 {let cache=LIST_CACHE.lock().await;if let Some((at,list))=cache.get(&key){if at.elapsed()<Duration::from_secs(60){return Ok(list.clone());}}}
 let list=match list_day_uncached(remote,day).await{
  Ok(list)=>list,
  Err(problem)=>{
   let mut errors=LIST_ERRORS.lock().await;errors.retain(|_,(at,_)|at.elapsed()<Duration::from_secs(30));
   if errors.len()<256{errors.insert(key.clone(),(std::time::Instant::now(),problem));}
   return Err(problem);
  }
 };
 let mut cache=LIST_CACHE.lock().await;
 cache.retain(|_,(at,_)|at.elapsed()<Duration::from_secs(20));
 if cache.len()<256{cache.insert(key,(std::time::Instant::now(),list.clone()));}
 Ok(list)
}
async fn list_day_uncached(remote:&Remote,day:&str)->Result<Vec<String>,Problem>{
 let target=format!("{}/{day}",remote.target);
 // Missing day directories are normal; distinguish them using the directory listing.
 let bytes=output(remote,&["lsjson",&target,"--recursive","--files-only","--no-modtime","--no-mimetype"],60).await;
 let bytes=match bytes {Ok(v)=>v,Err(e)=>{
   let dirs=output(remote,&["lsf",&remote.target,"--dirs-only","--max-depth","1"],30).await?;
   if !String::from_utf8_lossy(&dirs).lines().any(|line|line.trim_end_matches('/')==day){return Ok(vec![]);}return Err(e);
 }};
 let rows:Vec<Value>=serde_json::from_slice(&bytes).map_err(|_|error())?;
 let entries:Vec<(String,u64)>=rows.iter().filter_map(|v|Some((format!("{day}/{}",v["Path"].as_str()?),v["Size"].as_u64()?))).filter(|(p,_)|clip_camera(p).is_some()).collect();
 record_catalog(remote,entries.clone(),Some(day)).await?;
 Ok(entries.into_iter().map(|(path,_)|path).collect())
}
async fn clips(Extension(user):Extension<Principal>,Query(q):Query<ClipQuery>)->Result<Json<Vec<String>>,Problem>{
 if !crate::valid_recording_day(&q.day)||chrono::NaiveDate::parse_from_str(&q.day,"%Y-%m-%d").is_err()||q.camera.as_deref().is_some_and(|id|!camera_id(id)){return Err(bad());}
 let r=remote(&q.source).await?;
 let mut list=list_day(&r,&q.day).await?;
 list.retain(|path|clip_camera(path).is_some_and(|id|user.has_camera(id)&&q.camera.as_deref().is_none_or(|filter|id==filter)));
 list.sort();list.dedup();Ok(Json(list))
}
fn byte_range(range:Option<&str>,size:u64)->Result<(u64,u64,bool),()>{
 if size==0{return Err(());}
 let Some(range)=range else{return Ok((0,size-1,false));};
 let value=range.strip_prefix("bytes=").ok_or(())?;
 let (start,end)=value.split_once('-').ok_or(())?;
 if start.is_empty(){
 let tail=end.parse::<u64>().map_err(|_|())?;if tail==0{return Err(());}
 return Ok((size.saturating_sub(tail),size-1,true));
 }
 let start=start.parse::<u64>().map_err(|_|())?;
 let end=if end.is_empty(){size-1}else{end.parse::<u64>().map_err(|_|())?.min(size-1)};
 if start>=size||start>end{return Err(());}Ok((start,end,true))
}
fn media_response(body:Body,start:u64,end:u64,size:u64,partial:bool)->Result<Response,Problem>{
 let mut response=Response::builder().status(if partial{StatusCode::PARTIAL_CONTENT}else{StatusCode::OK})
 .header(header::CONTENT_TYPE,"video/mp4").header(header::ACCEPT_RANGES,"bytes").header(header::CONTENT_LENGTH,(end-start+1).to_string())
 .header(header::CACHE_CONTROL,"private, no-store").header("X-Content-Type-Options","nosniff");
 if partial{response=response.header(header::CONTENT_RANGE,format!("bytes {start}-{end}/{size}"));}
 response.body(body).map_err(|_|error())
}
async fn cached_media(file:std::path::PathBuf,size:u64,start:u64,end:u64,partial:bool)->Result<Option<Response>,Problem>{
 let length=end-start+1;
 if fs::symlink_metadata(&file).await.is_ok_and(|m|m.file_type().is_file()&&m.len()==size){
  if let Ok(mut file)=fs::File::open(file).await{
   file.seek(std::io::SeekFrom::Start(start)).await.map_err(|_|error())?;
   let stream=futures_util::stream::try_unfold((file,length),|(mut file,left)|async move{
    if left==0{return Ok::<_,std::io::Error>(None);}
    let mut buffer=vec![0;65536.min(left as usize)];let n=file.read(&mut buffer).await?;
    if n==0{return Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof,"cached video ended"));}
    buffer.truncate(n);Ok(Some((buffer,(file,left-n as u64))))
   });
   let mut response=media_response(Body::from_stream(stream),start,end,size,partial)?;
   response.headers_mut().insert("X-CameraHome-Playback",header::HeaderValue::from_static("verified-cloud-cache"));
   return Ok(Some(response));
  }
 }
 Ok(None)
}
async fn media(State(app):State<App>,Extension(user):Extension<Principal>,Path((source,path)):Path<(String,String)>,method:Method,headers:HeaderMap)->Result<Response,Problem>{
 let camera=clip_camera(&path).ok_or_else(bad)?;
 if !user.has_camera(camera){return Err((StatusCode::FORBIDDEN,"Không được xem camera này."));}
 let r=remote(&source).await?;
 let saved=catalog(&r).await;
 let mut known=saved.files.get(&path).copied().filter(|size|*size>0);
 if known.is_none(){
  let target=format!("{}/{}",r.target,path);
  let raw=output(&r,&["lsjson",&target,"--stat","--no-modtime","--no-mimetype"],45).await?;
  let item:Value=serde_json::from_slice(&raw).map_err(|_|error())?;
  known=item["Size"].as_u64().filter(|s|*s>0);
  if let Some(size)=known{record_catalog(&r,vec![(path.clone(),size)],None).await?;}
 }
 let size=known.ok_or_else(error)?;
 let range=headers.get(header::RANGE).map(|v|v.to_str().unwrap_or("invalid"));
 let Ok((start,end,partial))=byte_range(range,size) else{
  return Ok((StatusCode::RANGE_NOT_SATISFIABLE,[(header::CONTENT_RANGE,format!("bytes */{size}"))]).into_response());
 };
 let length=end-start+1;
 if method==Method::HEAD{return media_response(Body::empty(),start,end,size,partial);}
 // Serve a local cache only after the cloud copy was confirmed above.
 if let Some(response)=cached_media(app.recordings.join(&path),size,start,end,partial).await?{return Ok(response);}
 let permit=JOBS.clone().try_acquire_owned().map_err(|_|(StatusCode::TOO_MANY_REQUESTS,"Đang phát quá nhiều video đám mây."))?;
 let target=format!("{}/{}",r.target,path);
 let mut child=command(&r).args(["cat",&target,"--offset",&start.to_string(),"--count",&length.to_string()]).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().map_err(|_|error())?;
 let mut stdout=child.stdout.take().ok_or_else(error)?;
 let mut first=vec![0;65536.min(length as usize)];
 let count=time::timeout(Duration::from_secs(60),stdout.read(&mut first)).await.map_err(|_|(StatusCode::GATEWAY_TIMEOUT,"Drive phản hồi chậm. Chờ một lúc rồi bấm Phát lại."))?.map_err(|_|error())?;
 if count==0 {
  let result=time::timeout(Duration::from_secs(3),child.wait_with_output()).await;
  return Err(match result{Ok(Ok(output))=>transfer_error(&output.stderr),_=>error()});
 }
 first.truncate(count);
 let stream=futures_util::stream::try_unfold((child,stdout,permit,length,Some(first)),|(mut child,mut out,permit,left,first)|async move{
  if let Some(buffer)=first {let n=buffer.len() as u64;return Ok::<_,std::io::Error>(Some((buffer,(child,out,permit,left-n,None))));}
  if left==0 {let _=child.wait().await;return Ok(None);}
  let mut buffer=vec![0;65536.min(left as usize)];
  let n=time::timeout(Duration::from_secs(60),out.read(&mut buffer)).await.map_err(|_|std::io::Error::new(std::io::ErrorKind::TimedOut,"cloud video stalled"))??;
  if n==0{return Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof,"cloud video ended"));}
  buffer.truncate(n);Ok(Some((buffer,(child,out,permit,left-n as u64,None))))
 });
 let mut response=media_response(Body::from_stream(stream),start,end,size,partial)?;
 response.headers_mut().insert("X-CameraHome-Playback",header::HeaderValue::from_static("cloud-stream"));
 Ok(response)
}
async fn purge_busy()->bool{
 let raw=fs::read("/app-data/purge-status.json").await.ok();
 raw.and_then(|v|serde_json::from_slice::<Value>(&v).ok()).is_some_and(|v|matches!(v["state"].as_str(),Some("pending"|"running")))
}
fn settled_file(modified:std::time::SystemTime,now:std::time::SystemTime)->bool{
 now.duration_since(modified).is_ok_and(|age|age>=Duration::from_secs(60))
}
async fn local_files(app:&App,days:u32)->Result<Vec<String>,Problem>{
 let root=app.recordings.clone();
 tokio::task::spawn_blocking(move||{
  let mut result=Vec::new();let cutoff=chrono::Utc::now().timestamp()-i64::from(days)*86400;
  for item in crate::entries(&root)? {
   let day=item.file_name().to_string_lossy().into_owned();
   if !item.file_type()?.is_dir()||!crate::valid_recording_day(&day){continue;}
   for path in crate::clips_for_day(root.clone(),crate::Selection{day,camera:None})? {
    let settled=root.join(&path).metadata().ok().and_then(|m|m.modified().ok()).is_some_and(|modified|settled_file(modified,std::time::SystemTime::now()));
    if settled&&clip_camera(&path).is_some()&&(days==0||clip_time(&path).is_some_and(|t|t>=cutoff)){result.push(path);}
   }
  }Ok::<_,std::io::Error>(result)
 }).await.map_err(|_|error())?.map_err(|_|error())
}
async fn expire(remote:&Remote,days:u32,source:&str)->Result<(),Problem>{
 if days==0{return Ok(());}
 let cutoff=chrono::Utc::now().timestamp()-i64::from(days)*86400;
 let dirs=output(remote,&["lsf",&remote.target,"--dirs-only","--max-depth","1"],30).await?;
 let mut expired=Vec::new();
 for day in String::from_utf8_lossy(&dirs).lines().map(|v|v.trim_end_matches('/')) {
  let Ok(date)=chrono::NaiveDate::parse_from_str(day,"%Y-%m-%d") else{continue;};
  if !crate::valid_recording_day(day)||date.and_hms_opt(0,0,0).unwrap().and_utc().timestamp()>cutoff{continue;}
  for path in list_day_uncached(remote,day).await? {
   if clip_time(&path).is_some_and(|at|at<cutoff){expired.push(path);}
  }
 }
 if !expired.is_empty(){
  let file=format!("/app-data/cloud-expire-{source}.txt");
  private(&file,expired.join("\n").as_bytes()).await?;
  if !source_enabled(source).await{return Ok(());}
  let result=backup_output(remote,&["delete",&remote.target,"--files-from",&file],180,source).await;
  let _=fs::remove_file(file).await;result?;
 }
 Ok(())
}
async fn sync(app:&App,source:&str,days:u32)->Result<(),Problem>{
 let r=remote(source).await?;
 let files=local_files(app,days).await?;
 let copied=if !files.is_empty(){
  let file=format!("/app-data/cloud-upload-{source}.txt");
  private(&file,files.join("\n").as_bytes()).await?;
  let status_file=format!("/app-data/{source}-status");
  let _=private(&status_file,format!("Đang kiểm tra và sao lưu {} video; tự chờ nếu kho lưu trữ giới hạn API.|{}",files.len(),chrono::Utc::now().timestamp()).as_bytes()).await;
  let result=backup_output(&r,&["copy",app.recordings.to_str().ok_or_else(error)?,&r.target,"--files-from",&file,"--transfers","1","--checkers","1","--retries","3","--retries-sleep","30s"],600,source).await;
  let _=fs::remove_file(file).await;
  if result.is_ok(){
   let entries=files.iter().filter_map(|path|app.recordings.join(path).metadata().ok().map(|m|(path.clone(),m.len()))).collect();
   record_catalog(&r,entries,None).await?;
  }
  result.map(|_|())
 }else{Ok(())};
 if !source_enabled(source).await{return copied;}
 let expired=expire(&r,days,source).await;
 if expired.is_ok(){prune_catalog(&r,days).await?;}
 copied?;expired
}
fn old_enough(at:i64,now:i64,days:u32)->bool {days>0&&at<now-i64::from(days)*86400}
fn copies_confirmed(path:&str,size:u64,copies:&[BTreeMap<String,u64>])->bool{
 size>0&&!copies.is_empty()&&copies.iter().all(|copy|copy.get(path)==Some(&size))
}
pub(crate) fn is_staging(p:&Policy,path:&std::path::Path)->bool{
 !p.local_enabled&&p.local_off_since.is_some_and(|since|path.metadata().ok().and_then(|m|m.modified().ok()).and_then(|at|at.duration_since(std::time::UNIX_EPOCH).ok()).is_some_and(|at|at.as_millis()>=since as u128))
}
async fn cleanup_local(app:&App,p:&Policy)->Result<usize,Problem>{
 if !p.backup_enabled||(p.local_enabled&&p.local_days==0){return Ok(0);}
 let mut sources=Vec::new();
 if p.drive_enabled&&remote("drive").await.is_ok(){sources.push(remote("drive").await?);}
 if let Ok(c)=r2().await {if p.r2_enabled.unwrap_or(c.enabled){sources.push(r2_remote(&c));}}
 if sources.is_empty(){return Ok(0);}
 let now=chrono::Utc::now().timestamp();
 let candidates:Vec<String>=local_files(app,0).await?.into_iter().filter(|path|clip_time(path).is_some_and(|at|old_enough(at,now,p.local_days))||is_staging(p,&app.recordings.join(path))).collect();
 let days:std::collections::BTreeSet<_>=candidates.iter().filter_map(|p|p.split('/').next()).collect();
 let mut verified:Vec<BTreeMap<String,u64>>=Vec::new();
 for source in &sources {
  let mut sizes=BTreeMap::new();
  for day in &days {
   let target=format!("{}/{}",source.target,day);
   let Ok(raw)=output(source,&["lsjson",&target,"--recursive","--files-only","--no-modtime","--no-mimetype"],60).await else {continue;};
   let rows:Vec<Value>=serde_json::from_slice(&raw).map_err(|_|error())?;
   for row in rows {
    if let (Some(path),Some(size))=(row["Path"].as_str(),row["Size"].as_u64()){
     let relative=format!("{day}/{path}");
     if clip_camera(&relative).is_some(){sizes.insert(relative,size);}
    }
   }
  }
  verified.push(sizes);
 }
 let mut removed=0;
 for path in candidates {
  let file=app.recordings.join(&path);
  let Ok(meta)=fs::symlink_metadata(&file).await else{continue;};
  if !meta.file_type().is_file()||meta.len()==0{continue;}
  let current=policy().await;
  if !current.backup_enabled{return Ok(removed);}
  if !is_staging(&current,&file)&&!clip_time(&path).is_some_and(|at|old_enough(at,now,current.local_days)){continue;}
  if copies_confirmed(&path,meta.len(),&verified){
   if fs::remove_file(&file).await.is_ok(){removed+=1;}
  }
 }
 Ok(removed)
}
pub async fn backup_worker(app:App){
 let mut tick=time::interval(Duration::from_secs(300));tick.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
 loop {
 tick.tick().await;
 let Ok(_lock)=app.drive_lock.try_lock() else{continue;};
 if purge_busy().await{continue;}
 let p=policy().await;
 if !p.backup_enabled{continue;}
 for source in ["drive","r2"]{
  let (enabled,days)=if source=="drive"{(p.drive_enabled,p.drive_days)}else{match r2().await {Ok(c)=>(p.r2_enabled.unwrap_or(c.enabled),c.days),Err(_)=>continue}};
  if !enabled||!source_enabled(source).await||remote(source).await.is_err(){continue;}
  let outcome=sync(&app,source,days).await;
  let message=match outcome {Ok(())=>"Đã sao lưu và kiểm tra hạn lưu trữ".to_owned(),Err((_,reason))=>format!("{reason} Sẽ thử lại ở lượt sau.")};
  let file=format!("/app-data/{source}-status");
  let _=private(&file,format!("{message}|{}",chrono::Utc::now().timestamp()).as_bytes()).await;
 }
 if p.local_days>0||!p.local_enabled {
  let message=match cleanup_local(&app,&p).await{
   Ok(removed)=>format!("Đã dọn {removed} video quá hạn có bản sao xác nhận trên các nguồn đám mây đang bật. Video chưa xác nhận sao lưu được giữ lại."),
   Err(_)=>"Chưa xác nhận được bản sao đám mây. Giữ nguyên video trên máy chủ.".to_owned()
  };
  let _=private("/app-data/local-retention-status",format!("{message}|{}",chrono::Utc::now().timestamp()).as_bytes()).await;
 }
 }
}
#[cfg(test)]mod tests {
 use super::*;
 #[test]fn staging_keeps_old_archives_and_is_reversible(){
 let path=std::env::temp_dir().join(format!("camerahome-stage-{}",std::process::id()));
 std::fs::write(&path,b"test").unwrap();
 let modified=path.metadata().unwrap().modified().unwrap().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as i64;
 let mut p=Policy{local_enabled:false,local_off_since:Some(modified+1000),..Policy::default()};
 assert!(!is_staging(&p,&path));
 p.local_off_since=Some(modified-1000);assert!(is_staging(&p,&path));
 p.local_enabled=true;assert!(!is_staging(&p,&path));
 std::fs::remove_file(path).unwrap();
}
 #[test]fn retention_units_and_limits(){
 assert_eq!(DurationInput{value:6,unit:"hours".into()}.seconds().unwrap(),21600);
 assert_eq!(DurationInput{value:2,unit:"days".into()}.seconds().unwrap(),172800);
 assert_eq!(DurationInput{value:0,unit:"hours".into()}.seconds().unwrap(),0);
 assert!(DurationInput{value:u64::MAX,unit:"days".into()}.seconds().is_err());
 assert!(DurationInput{value:1,unit:"minutes".into()}.seconds().is_err());
}
 #[test]fn legacy_policy_preserves_storage(){
 let p:Policy=serde_json::from_str(r#"{"drive_enabled":false,"local_days":1}"#).unwrap();
 assert!(p.backup_enabled&&p.local_enabled);assert!(!p.drive_enabled);assert_eq!(p.r2_enabled,None);assert_eq!(p.local_days,1);
}
 #[test]fn requires_matching_copy_in_every_active_source(){
 let path="2026-11-22/07/bep/01.02.mp4";
 let good=BTreeMap::from([(path.to_owned(),100)]);
 let short=BTreeMap::from([(path.to_owned(),99)]);
 assert!(!copies_confirmed(path,100,&[]));
 assert!(!copies_confirmed(path,100,&[good.clone(),BTreeMap::new()]));
 assert!(!copies_confirmed(path,100,&[good.clone(),short]));
 assert!(copies_confirmed(path,100,&[good.clone(),good]));
}
 #[test]fn local_expiry_never_deletes_when_disabled(){assert!(!old_enough(0,1000000,0));assert!(!old_enough(100000,100001,1));assert!(old_enough(0,100000,1));}
 #[test]fn only_settled_files_are_uploaded(){
 let now=std::time::UNIX_EPOCH+Duration::from_secs(1000);
 assert!(!settled_file(now-Duration::from_secs(59),now));
 assert!(settled_file(now-Duration::from_secs(60),now));
 assert!(!settled_file(now+Duration::from_secs(1),now));
}
 #[test]fn safe_failure_reason(){
 let (_,reason)=transfer_error(b"Failed to load filters: private credential");
 assert!(reason.contains("rclone"));assert!(!reason.contains("private credential"));
 let (_,rate)=transfer_error(b"RATE_LIMIT_EXCEEDED");assert!(rate.contains("giới hạn"));
}
 #[test]fn ranges(){assert_eq!(byte_range(Some("bytes=10-19"),100),Ok((10,19,true)));assert_eq!(byte_range(Some("bytes=-10"),100),Ok((90,99,true)));assert_eq!(byte_range(Some("bytes=90-"),100),Ok((90,99,true)));assert!(byte_range(Some("bytes=100-"),100).is_err());assert!(byte_range(Some("bytes=0-1,4-5"),100).is_err());}
 #[test]fn paths(){assert_eq!(clip_camera("2026-11-22/07/bep/01.02.mp4"),Some("bep"));for bad in ["2026-11-22/07/../01.02.mp4","2026-11-22/99/bep/01.02.mp4","2026-11-22/07/bep/../../key","2026-02-30/07/bep/01.02.mp4"]{assert!(clip_camera(bad).is_none());}}
 #[test]fn retention_uses_recording_time(){let at=clip_time("2026-11-22/07/bep/01.02.mp4").unwrap();assert_eq!(chrono::DateTime::from_timestamp(at,0).unwrap().to_rfc3339(),"2026-11-22T07:01:02+00:00");assert_eq!(Policy::default().drive_days,0);}
}

#[cfg(test)]mod playback_cache_tests{
 use super::*;
 #[tokio::test]async fn verified_cache_supports_seeking_and_rejects_mismatch(){
  let path=std::env::temp_dir().join(format!("camerahome-playback-test-{}",std::process::id()));
  fs::write(&path,b"0123456789").await.unwrap();
  let response=cached_media(path.clone(),10,2,5,true).await.unwrap().unwrap();
  assert_eq!(response.status(),StatusCode::PARTIAL_CONTENT);
  assert_eq!(response.headers()[header::CONTENT_RANGE],"bytes 2-5/10");
  assert_eq!(axum::body::to_bytes(response.into_body(),100).await.unwrap().as_ref(),b"2345");
  assert!(cached_media(path.clone(),9,0,1,true).await.unwrap().is_none());
  let link=path.with_extension("link");std::os::unix::fs::symlink(&path,&link).unwrap();
  assert!(cached_media(link.clone(),10,0,1,true).await.unwrap().is_none());
  fs::remove_file(link).await.unwrap();fs::remove_file(path).await.unwrap();
 }
}

