use crate::{App, manage};
use axum::http::StatusCode;
use serde_json::{json, Value};
use std::{collections::HashMap, net::Ipv4Addr, time::Duration};
use tokio::{fs, time};

type Problem = (StatusCode, &'static str);
const IDENTITIES: &str = "/app-data/camera-identities.json";
const NEIGHBORS: &str = "/app-data/lan-neighbors.json";
fn error() -> Problem {(StatusCode::INTERNAL_SERVER_ERROR,"Không lưu được nhận diện MAC.")}
pub(crate) fn validate(input: Option<&str>) -> Result<String, Problem> {
    let raw=input.unwrap_or("").trim().replace('-',":").to_ascii_lowercase();
    if raw.is_empty() {return Ok(raw);}
    let parts:Vec<_>=raw.split(':').collect();
    if parts.len()!=6 || parts.iter().any(|p|p.len()!=2 || !p.bytes().all(|b|b.is_ascii_hexdigit()))
        || u8::from_str_radix(parts[0],16).unwrap_or(1)&1!=0 || raw=="00:00:00:00:00:00" {
        return Err((StatusCode::BAD_REQUEST,"MAC phải có dạng AA:BB:CC:DD:EE:FF và là địa chỉ thiết bị hợp lệ."));
    }
    Ok(raw)
}
async fn load() -> Result<Value,Problem> {
    match fs::read(IDENTITIES).await {
        Ok(raw)=>serde_json::from_slice(&raw).map_err(|_|error()),
        Err(e) if e.kind()==std::io::ErrorKind::NotFound=>Ok(json!({})),
        Err(_)=>Err(error())
    }
}
async fn store(value:&Value)->Result<(),Problem> {
    let tmp=format!("{IDENTITIES}.new");
    fs::write(&tmp,serde_json::to_vec(value).map_err(|_|error())?).await.map_err(|_|error())?;
    fs::rename(tmp,IDENTITIES).await.map_err(|_|error())
}
pub(crate) async fn identity(id:&str)->Result<Value,Problem> {
    Ok(load().await?.get(id).cloned().unwrap_or_else(||json!({"mac":"","auto_ip":true,"status":"Đang chờ dò MAC trong LAN."})))
}
pub(crate) async fn set(id:&str,mac:String,auto_ip:bool)->Result<(),Problem> {
    let mut all=load().await?;
    if !all.is_object() {return Err(error());}
    if !mac.is_empty() && all.as_object().unwrap().iter().any(|(key,v)|key!=id && v["mac"].as_str()==Some(mac.as_str())) {
        return Err((StatusCode::CONFLICT,"MAC này đã được gán cho camera khác."));
    }
    all[id]=json!({"mac":mac,"auto_ip":auto_ip,"status":if auto_ip {"Đang dò IP theo MAC."} else {"Đã tắt tự dò IP."}});
    store(&all).await
}
pub(crate) async fn remove(id:&str)->Result<(),Problem> {
    let mut all=load().await?;
    if let Some(items)=all.as_object_mut(){items.remove(id);}
    store(&all).await
}
fn allowed_ip(ip:&str)->bool {
    let Ok(ip)=ip.parse::<Ipv4Addr>() else{return false;};
    let [a,b,c,d]=ip.octets();
    format!("{a}.{b}.{c}")==std::env::var("CAMERA_SUBNET").unwrap_or_else(|_|"192.168.88".into()) && (1..255).contains(&d)
}
fn replace_host(source:&str,ip:&str)->Option<String> {
    // Replace only the host, preserving credentials and URL options byte for byte.
    let start=source.find("://")?+3;
    let end=start+source[start..].find('/')?;
    let auth=source[start..end].rfind('@').map(|i|start+i+1).unwrap_or(start);
    let port=source[auth..end].find(':').map(|i|auth+i).unwrap_or(end);
    Some(format!("{}{}{}",&source[..auth],ip,&source[port..]))
}
async fn cycle(app:&App)->Result<(),Problem> {
    let raw=fs::read(NEIGHBORS).await.map_err(|_|error())?;
    let snapshot:Value=serde_json::from_slice(&raw).map_err(|_|error())?;
    let age=chrono::Utc::now().timestamp()-snapshot["at"].as_i64().unwrap_or(0);
    if !(0..120).contains(&age) {return Ok(());}
    let neighbors=snapshot["neighbors"].as_object().ok_or_else(error)?;
    let mut by_mac:HashMap<String,Vec<String>>=HashMap::new();
    for (ip,mac) in neighbors {
        if !allowed_ip(ip){continue;}
        if let Ok(mac)=validate(mac.as_str()) {if !mac.is_empty(){by_mac.entry(mac).or_default().push(ip.clone());}}
    }
    let _guard=app.management.lock().await;
    let mut config=manage::read_config().await?;
    let mut identities=load().await?;
    let original=identities.clone();
    let stats:Value=app.client.get(format!("{}/api/stats",app.frigate)).send().await.map_err(|_|error())?.json().await.map_err(|_|error())?;
    let ids:Vec<String>=config["cameras"].as_object().ok_or_else(error)?.keys().cloned().collect();
    let mut changed=Vec::new();
    for id in ids {
        let Some(source)=manage::camera_source(&config,&id).map(str::to_owned) else{continue;};
        let Some(old_ip)=reqwest::Url::parse(&source).ok().and_then(|u|u.host_str().map(str::to_owned)) else{continue;};
        if !allowed_ip(&old_ip){continue;}
        if identities.get(&id).is_none(){identities[&id]=json!({"mac":"","auto_ip":true});}
        if identities[&id]["auto_ip"]==false {continue;}
        let mut mac=identities[&id]["mac"].as_str().unwrap_or("").to_owned();
        if mac.is_empty() && stats["cameras"][&id]["camera_fps"].as_f64().unwrap_or(0.0)>0.0 {
            if let Some(found)=neighbors.get(&old_ip).and_then(Value::as_str) {
                if let Ok(found)=validate(Some(found)) {
                    if !found.is_empty() && !identities.as_object().ok_or_else(error)?.iter().any(|(key,v)|key!=&id && v["mac"]==found) {
                        mac=found;identities[&id]["mac"]=json!(mac);
                    }
                }
            }
        }
        let Some(ips)=by_mac.get(&mac).filter(|ips|ips.len()==1) else{
            identities[&id]["status"]=json!(if mac.is_empty() {"Chưa học được MAC. Nhập MAC trên tem hoặc trong app Tapo."} else {"Chưa tìm thấy MAC duy nhất trong LAN."});continue;
        };
        let ip=&ips[0];
        if ip==&old_ip {identities[&id]["status"]=json!("Đã nhận diện MAC · IP hiện tại đúng.");continue;}
        let target=format!("{ip}:554");
        if !matches!(time::timeout(Duration::from_secs(2),tokio::net::TcpStream::connect(&target)).await,Ok(Ok(_))) {
            identities[&id]["status"]=json!("Tìm thấy IP mới nhưng cổng RTSP chưa sẵn sàng.");continue;
        }
        let streams=config["go2rtc"]["streams"].as_object_mut().ok_or_else(error)?;
        for name in [id.clone(),format!("{id}__compact")] {
            if let Some(stream)=streams.get_mut(&name) {
                if let Some(url)=stream.as_str().and_then(|s|replace_host(s,ip)){*stream=json!(url);}
                else if let Some(items)=stream.as_array_mut() {
                    for item in items {
                        if let Some(url)=item.as_str().filter(|s|s.starts_with("rtsp://")).and_then(|s|replace_host(s,ip)){*item=json!(url);}
                    }
                }
            }
        }
        if config["cameras"][&id]["onvif"].get("host").is_some(){config["cameras"][&id]["onvif"]["host"]=json!(ip);}
        identities[&id]["status"]=json!(format!("Đã tự đổi IP: {old_ip} → {ip}"));
        identities[&id]["changed_at"]=json!(chrono::Utc::now().timestamp());
        changed.push(id);
    }
    if !changed.is_empty() {manage::save_config(app,&config).await?;eprintln!("MAC recovery updated {} camera(s)",changed.len());}
    if identities!=original {store(&identities).await?;}
    Ok(())
}
pub(crate) async fn worker(app:App) {
    let mut tick=time::interval(Duration::from_secs(30));
    tick.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
    loop {tick.tick().await;if cycle(&app).await.is_err(){eprintln!("MAC discovery: waiting for LAN snapshot or camera service");}}
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test] fn validates_mac() {
        assert_eq!(validate(Some("AA-BB-CC-DD-EE-02")).unwrap(),"aa:bb:cc:dd:ee:02");
        assert!(validate(Some("ff:ff:ff:ff:ff:ff")).is_err());
        assert!(validate(Some("00:00:00:00:00:00")).is_err());
        assert!(validate(Some("aa:bb:cc")).is_err());
    }
    #[test] fn changes_only_host() {
        assert_eq!(replace_host("rtsp://a:p%40x@192.168.88.9:554/stream1#x","192.168.88.19").unwrap(),"rtsp://a:p%40x@192.168.88.19:554/stream1#x");
        assert!(!allowed_ip("8.8.8.8"));assert!(!allowed_ip("192.168.88.255"));
    }
}
