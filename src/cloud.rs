use crate::{App,accounts::Principal,camera_id};
use axum::{body::Body,extract::{State,Extension,Path,Query},http::{header,HeaderMap,Method,StatusCode},response::{Response,IntoResponse},routing::{get,post},Json,Router};
use serde::{Deserialize,Serialize};
use serde_json::{json,Value};
use std::{collections::BTreeMap,process::Stdio,sync::{Arc,LazyLock},time::Duration};
use tokio::{fs,io::AsyncReadExt,process::Command,time};
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
struct Policy {drive_days:u32,drive_enabled:bool}
impl Default for Policy{fn default()->Self{Self{drive_days:0,drive_enabled:true}}}
async fn r2()->Result<R2,Problem>{serde_json::from_slice(&fs::read(R2_FILE).await.map_err(|_|(StatusCode::CONFLICT,"Chưa kết nối Cloudflare R2."))?).map_err(|_|error())}
async fn policy()->Policy{fs::read(POLICY).await.ok().and_then(|v|serde_json::from_slice(&v).ok()).unwrap_or_default()}
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
 c.args(["--config",&remote.config,"--contimeout","8s","--timeout","45s","--retries","1","--low-level-retries","2"]).envs(remote.env.iter().cloned()).kill_on_drop(true);
 c
}
async fn output(remote:&Remote,args:&[&str],seconds:u64)->Result<Vec<u8>,Problem>{
 let _slot=JOBS.clone().try_acquire_owned().map_err(|_|(StatusCode::TOO_MANY_REQUESTS,"Kho lưu trữ đang bận. Thử lại sau."))?;
 let result=time::timeout(Duration::from_secs(seconds),command(remote).args(args).output()).await.map_err(|_|error())?.map_err(|_|error())?;
 if result.status.success(){Ok(result.stdout)}else{Err(error())}
}
pub fn routes()->Router<App>{
 Router::new().route("/api/storage",get(settings).post(save_policy))
 .route("/api/storage/r2",post(save_r2))
 .route("/api/cloud/clips",get(clips))
 .route("/api/cloud/media/{source}/{*path}",get(media))
}
async fn settings()->Json<Value>{
 let policy=policy().await;
 let r=r2().await.ok();
 let last=fs::read_to_string("/app-data/r2-status").await.unwrap_or_default();
 Json(json!({"drive_days":policy.drive_days,"drive_enabled":policy.drive_enabled,"r2":r.map(|c|json!({"account_id":c.account_id,"bucket":c.bucket,"prefix":c.prefix,"days":c.days,"enabled":c.enabled,"configured":true})),"r2_status":last}))
}
async fn save_policy(State(app):State<App>,headers:HeaderMap,Json(policy):Json<Policy>)->Result<StatusCode,Problem>{
 crate::manage::require_origin(&headers)?;
 if policy.drive_days>3650 {return Err(bad());}
 let _lock=app.drive_lock.lock().await;
 private(POLICY,&serde_json::to_vec(&policy).map_err(|_|bad())?).await?;
 Ok(StatusCode::NO_CONTENT)
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
static LIST_CACHE:LazyLock<tokio::sync::Mutex<BTreeMap<String,(std::time::Instant,Vec<String>)>>>=LazyLock::new(Default::default);
async fn list_day(remote:&Remote,day:&str)->Result<Vec<String>,Problem>{
 let key=format!("{}/{}",remote.target,day);
 {let cache=LIST_CACHE.lock().await;if let Some((at,list))=cache.get(&key){if at.elapsed()<Duration::from_secs(20){return Ok(list.clone());}}}
 let list=list_day_uncached(remote,day).await?;
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
 Ok(rows.iter().filter_map(|v|v["Path"].as_str()).map(|path|format!("{day}/{path}")).filter(|p|clip_camera(p).is_some()).collect())
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
async fn media(Extension(user):Extension<Principal>,Path((source,path)):Path<(String,String)>,method:Method,headers:HeaderMap)->Result<Response,Problem>{
 let camera=clip_camera(&path).ok_or_else(bad)?;
 if !user.has_camera(camera){return Err((StatusCode::FORBIDDEN,"Không được xem camera này."));}
 let r=remote(&source).await?;
 let target=format!("{}/{}",r.target,path);
 let raw=output(&r,&["lsjson",&target,"--stat","--no-modtime","--no-mimetype"],30).await?;
 let item:Value=serde_json::from_slice(&raw).map_err(|_|error())?;
 let size=item["Size"].as_u64().filter(|s|*s>0).ok_or_else(error)?;
 let range=headers.get(header::RANGE).map(|v|v.to_str().unwrap_or("invalid"));
 let Ok((start,end,partial))=byte_range(range,size) else{
 return Ok((StatusCode::RANGE_NOT_SATISFIABLE,[(header::CONTENT_RANGE,format!("bytes */{size}"))]).into_response());
 };
 let length=end-start+1;
 let body=if method==Method::HEAD {Body::empty()}else{
  let permit=JOBS.clone().try_acquire_owned().map_err(|_|(StatusCode::TOO_MANY_REQUESTS,"Đang phát quá nhiều video đám mây."))?;
  let mut child=command(&r).args(["cat",&target,"--offset",&start.to_string(),"--count",&length.to_string()]).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().map_err(|_|error())?;
  let stdout=child.stdout.take().ok_or_else(error)?;
  let stream=futures_util::stream::try_unfold((child,stdout,permit,length),|(mut child,mut out,permit,left)|async move{
   if left==0 {let _=child.wait().await;return Ok::<_,std::io::Error>(None);}
   let mut buf=vec![0u8;65536.min(left as usize)];
   let n=time::timeout(Duration::from_secs(60),out.read(&mut buf)).await.map_err(|_|std::io::Error::new(std::io::ErrorKind::TimedOut,"cloud video stalled"))??;
   if n==0{return Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof,"cloud video ended"));}
   buf.truncate(n);Ok(Some((buf,(child,out,permit,left-n as u64))))
  });
  Body::from_stream(stream)
 };
 let mut response=Response::builder().status(if partial{StatusCode::PARTIAL_CONTENT}else{StatusCode::OK})
 .header(header::CONTENT_TYPE,"video/mp4").header(header::ACCEPT_RANGES,"bytes").header(header::CONTENT_LENGTH,length.to_string())
 .header(header::CACHE_CONTROL,"private, no-store").header("X-Content-Type-Options","nosniff");
 if partial{response=response.header(header::CONTENT_RANGE,format!("bytes {start}-{end}/{size}"));}
 Ok(response.body(body).map_err(|_|error())?)
}
async fn purge_busy()->bool{
 let raw=fs::read("/app-data/purge-status.json").await.ok();
 raw.and_then(|v|serde_json::from_slice::<Value>(&v).ok()).is_some_and(|v|matches!(v["state"].as_str(),Some("pending"|"running")))
}
async fn local_files(app:&App,days:u32)->Result<Vec<String>,Problem>{
 let root=app.recordings.clone();
 tokio::task::spawn_blocking(move||{
  let mut result=Vec::new();let cutoff=chrono::Utc::now().timestamp()-i64::from(days)*86400;
  for item in crate::entries(&root)? {
   let day=item.file_name().to_string_lossy().into_owned();
   if !item.file_type()?.is_dir()||!crate::valid_recording_day(&day){continue;}
   for path in crate::clips_for_day(root.clone(),crate::Selection{day,camera:None})? {
    if clip_camera(&path).is_some()&&(days==0||clip_time(&path).is_some_and(|t|t>=cutoff)){result.push(path);}
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
  let result=output(remote,&["delete",&remote.target,"--files-from",&file],180).await;
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
  let result=output(&r,&["copy",app.recordings.to_str().ok_or_else(error)?,&r.target,"--files-from",&file,"--min-age","1m","--transfers","2"],240).await;
  let _=fs::remove_file(file).await;result.map(|_|())
 }else{Ok(())};
 let expired=expire(&r,days,source).await;
 copied?;expired
}
pub async fn backup_worker(app:App){
 let mut tick=time::interval(Duration::from_secs(300));tick.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
 loop {
 tick.tick().await;
 let Ok(_lock)=app.drive_lock.try_lock() else{continue;};
 if purge_busy().await{continue;}
 let p=policy().await;
 for source in ["drive","r2"]{
  let (enabled,days)=if source=="drive"{(p.drive_enabled,p.drive_days)}else{match r2().await {Ok(c)=>(c.enabled,c.days),Err(_)=>continue}};
  if !enabled||remote(source).await.is_err(){continue;}
  let ok=sync(&app,source,days).await.is_ok();
  let message=if ok{"Đã sao lưu và kiểm tra hạn lưu trữ"}else{"Sao lưu hoặc dọn hạn lưu trữ chưa hoàn tất. Sẽ thử lại"};
  let file=format!("/app-data/{source}-status");
  let _=private(&file,format!("{message}|{}",chrono::Utc::now().timestamp()).as_bytes()).await;
 }
 }
}
#[cfg(test)]mod tests {
 use super::*;
 #[test]fn ranges(){assert_eq!(byte_range(Some("bytes=10-19"),100),Ok((10,19,true)));assert_eq!(byte_range(Some("bytes=-10"),100),Ok((90,99,true)));assert_eq!(byte_range(Some("bytes=90-"),100),Ok((90,99,true)));assert!(byte_range(Some("bytes=100-"),100).is_err());assert!(byte_range(Some("bytes=0-1,4-5"),100).is_err());}
 #[test]fn paths(){assert_eq!(clip_camera("2026-11-22/07/bep/01.02.mp4"),Some("bep"));for bad in ["2026-11-22/07/../01.02.mp4","2026-11-22/99/bep/01.02.mp4","2026-11-22/07/bep/../../key","2026-02-30/07/bep/01.02.mp4"]{assert!(clip_camera(bad).is_none());}}
 #[test]fn retention_uses_recording_time(){let at=clip_time("2026-11-22/07/bep/01.02.mp4").unwrap();assert_eq!(chrono::DateTime::from_timestamp(at,0).unwrap().to_rfc3339(),"2026-11-22T07:01:02+00:00");assert_eq!(Policy::default().drive_days,0);}
}
