
use axum::{body::{to_bytes, Body, Bytes}, extract::{ConnectInfo, State}, http::{header, HeaderMap, Request, StatusCode}, response::{IntoResponse, Response}, Router};
use base64::{engine::general_purpose::STANDARD, Engine};
use serde::{Deserialize, Serialize};
use std::{net::{IpAddr, Ipv4Addr, SocketAddr}, sync::{Arc, Mutex}, time::{Duration, Instant}};
use tauri::WebviewWindow;
use tokio::{net::TcpListener, sync::{oneshot, Semaphore}, time::timeout};
use uuid::Uuid;

const ITEM: usize = 32 * 1024 * 1024;
const TOTAL: usize = 96 * 1024 * 1024;
const TEXT: usize = 64 * 1024;
const IDLE: Duration = Duration::from_secs(600);
const TTL: Duration = Duration::from_secs(1800);
const PAIR: Duration = Duration::from_secs(90);
const PHONE: &str = include_str!("lan-phone.html");
const PHONE_JS: &str = include_str!("lan-phone.js");
type HttpError = (StatusCode, &'static str);

#[derive(Default)]
pub struct Lan(Mutex<Option<Arc<Session>>>);

struct Session {
    host: String,
    code: String,
    inner: Mutex<Room>,
    stop: Mutex<Option<oneshot::Sender<()>>>,
    uploads: Semaphore,
}
struct Room {
    last: Instant,
    pair: Option<Pair>,
    items: Vec<Entry>,
    closed: bool,
}
struct Pair { id: String, ip: IpAddr, since: Instant, approved: bool }
struct Entry { info: Item, data: Bytes, expires: Instant, clipboard: Option<ClipboardFile> }
struct ClipboardFile { dir: std::path::PathBuf, path: std::path::PathBuf }

impl ClipboardFile {
    fn new(name: &str, data: &[u8]) -> Result<Self, String> {
        use std::io::Write;
        let dir = std::env::temp_dir().join(format!("Desklet-transfer-{}", key()));
        std::fs::create_dir(&dir).map_err(|_| "无法创建剪贴板临时目录")?;
        let copy = Self { path: dir.join(name), dir };
        let mut file = std::fs::OpenOptions::new().write(true).create_new(true).open(&copy.path)
            .map_err(|_| "无法创建剪贴板临时文件，请使用另存为")?;
        file.write_all(data).map_err(|_| "无法写入剪贴板临时文件")?;
        Ok(copy)
    }
}

impl Drop for ClipboardFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
        let _ = std::fs::remove_dir(&self.dir);
    }
}
#[derive(Clone, Serialize)]
pub struct Item {
    id: String, name: String, kind: String, mime: String, size: usize, incoming: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    preview: Option<String>,
}

impl Room {
    fn prune(&mut self) {
        self.items.retain(|item| item.expires > Instant::now());
        if self.pair.as_ref().is_some_and(|p| !p.approved && p.since.elapsed() >= PAIR) { self.pair = None; }
    }
    fn live(&self) -> bool { !self.closed && self.last.elapsed() < IDLE }
    fn add(&mut self, name: String, kind: String, data: Bytes, incoming: bool) -> Result<Item, String> {
        self.prune();
        if !self.live() || !self.pair.as_ref().is_some_and(|p| p.approved) { return Err("手机尚未连接或会话已结束".into()); }
        if data.is_empty() || data.len() > ITEM || self.items.len() >= 32
            || self.items.iter().map(|e| e.data.len()).sum::<usize>() + data.len() > TOTAL {
            return Err("单项上限 32 MB，每次连接最多 32 项、合计 96 MB；可以删除已用的内容".into());
        }
        let name = filename(&name);
        let (kind, mime) = match kind.as_str() {
            "text" | "link" => {
                let value = std::str::from_utf8(&data).map_err(|_| "文字编码无效")?;
                if value.trim().is_empty() || data.len() > TEXT { return Err("文字为空或超过 64 KB".into()); }
                if kind == "link" && !(value.starts_with("https://") || value.starts_with("http://")) { return Err("仅支持 HTTP 或 HTTPS 链接".into()); }
                (kind.as_str(), "text/plain")
            }
            _ => match image_type(&data) {
                Some(mime) => ("image", mime),
                None if text_file(&name, &data) => ("file", "text/plain"),
                None => ("file", "application/octet-stream"),
            },
        };
        let preview = (mime == "text/plain").then(|| {
            let value = std::str::from_utf8(&data).unwrap_or_default();
            let mut chars = value.chars();
            let mut preview: String = chars.by_ref().take(if kind == "file" { 360 } else { 600 }).collect();
            if chars.next().is_some() { preview.push('…'); }
            preview
        });
        let info = Item { id: key(), name, kind: kind.into(), mime: mime.into(), size: data.len(), incoming, preview };
        self.items.push(Entry { info: info.clone(), data, expires: Instant::now() + TTL, clipboard: None });
        self.last = Instant::now();
        Ok(info)
    }
}

fn key() -> String { Uuid::new_v4().simple().to_string() }
fn filename(s: &str) -> String {
    let s: String = s.chars().map(|c| if c.is_control() || "\\/:*?\"<>|".contains(c) { '_' } else { c }).take(110).collect();
    let s = s.trim().trim_matches(['.', ' ']);
    if s.is_empty() { "Desklet-transfer".into() } else { s.into() }
}
fn image_type(data: &[u8]) -> Option<&'static str> {
    if data.starts_with(b"\x89PNG\r\n\x1a\n") { Some("image/png") }
    else if data.starts_with(&[255,216,255]) { Some("image/jpeg") }
    else if data.starts_with(b"GIF87a") || data.starts_with(b"GIF89a") { Some("image/gif") }
    else if data.starts_with(b"RIFF") && data.get(8..12)==Some(b"WEBP") { Some("image/webp") }
    else { None }
}
fn text_file(name: &str, data: &[u8]) -> bool {
    let ext = name.rsplit_once('.').map(|(_, ext)| ext.to_ascii_lowercase());
    data.len() <= TEXT && !data.contains(&0) && std::str::from_utf8(data).is_ok()
        && matches!(ext.as_deref(), Some("txt"|"md"|"csv"|"tsv"|"json"|"log"|"xml"|"yaml"|"yml"|"ini"))
}
fn interfaces() -> Vec<Ipv4Addr> {
    let mut list: Vec<_> = if_addrs::get_if_addrs().unwrap_or_default().into_iter()
        .filter_map(|a| match a.ip() {
            IpAddr::V4(ip) if ip.is_private() || ip.is_link_local() => Some(ip),
            _ => None,
        }).collect();
    list.sort(); list.dedup(); list
}
#[tauri::command]
pub fn lan_addresses() -> Vec<String> { interfaces().iter().map(ToString::to_string).collect() }

impl Session {
    fn close(&self) {
        let mut room = self.inner.lock().unwrap();
        room.closed = true; room.items.clear(); room.pair = None;
        drop(room);
        if let Some(tx) = self.stop.lock().unwrap().take() { let _ = tx.send(()); }
    }
}
fn active(lan: &Lan) -> Result<Arc<Session>, String> {
    lan.0.lock().unwrap().as_ref().cloned().ok_or("互传未开启".into())
}
#[derive(Serialize)]
pub struct Status {
    running: bool, url: String, pending: Option<(String,String)>, connected: bool, remaining: u64, items: Vec<Item>,
}
fn status(session: &Session) -> Status {
    let mut room = session.inner.lock().unwrap();
    room.prune();
    let running = room.live();
    Status {
        running,
        url: if running { format!("http://{}/#{}",session.host,session.code) } else { String::new() },
        pending: room.pair.as_ref().filter(|p| running && !p.approved).map(|p| (p.id.clone(),p.ip.to_string())),
        connected: running && room.pair.as_ref().is_some_and(|p|p.approved),
        remaining: IDLE.saturating_sub(room.last.elapsed()).as_secs(),
        items: if running {room.items.iter().map(|e|e.info.clone()).collect()} else {Vec::new()},
    }
}

#[tauri::command]
pub async fn lan_start(lan: tauri::State<'_,Lan>, ip: String) -> Result<Status,String> {
    let ip: Ipv4Addr = ip.parse().map_err(|_|"局域网地址无效")?;
    if !interfaces().contains(&ip) { return Err("这个局域网地址已不可用".into()); }
    let listener = TcpListener::bind((ip,0)).await.map_err(|e|format!("无法启动互传：{e}"))?;
    let host = listener.local_addr().map_err(|e|e.to_string())?.to_string();
    let (tx,rx)=oneshot::channel();
    let session=Arc::new(Session {host,code:key(), inner:Mutex::new(Room{last:Instant::now(),pair:None,items:Vec::new(),closed:false}),stop:Mutex::new(Some(tx)),uploads:Semaphore::new(2)});
    if let Some(old)=lan.0.lock().unwrap().replace(session.clone()){old.close();}
    let expiry=session.clone();
    let router=Router::new().fallback(http).with_state(session.clone());
    tauri::async_runtime::spawn(async move {
        let shutdown = async move {
            let mut rx = rx;
            loop {
                tokio::select! {
                    _ = &mut rx => break,
                    _ = tokio::time::sleep(Duration::from_secs(1)) => {
                        let mut room = expiry.inner.lock().unwrap();
                        room.prune();
                        if !room.live() { break; }
                    }
                }
            }
            expiry.close();
        };
        let _ = axum::serve(listener, router.into_make_service_with_connect_info::<SocketAddr>())
            .with_graceful_shutdown(shutdown).await;
    });
    Ok(status(&session))
}
#[tauri::command]
pub fn lan_status(lan: tauri::State<'_,Lan>) -> Option<Status> { active(&lan).ok().map(|s|status(&s)) }
#[tauri::command]
pub fn lan_qr(lan: tauri::State<'_,Lan>) -> Result<String,String> {
    let s=active(&lan)?;
    let url=status(&s).url;
    if url.is_empty(){return Err("互传未开启".into());}
    let qr=qrcode::QrCode::new(url).map_err(|e|e.to_string())?;
    let svg=qr.render::<qrcode::render::svg::Color>().min_dimensions(200,200).build();
    Ok(format!("data:image/svg+xml;base64,{}",STANDARD.encode(svg)))
}
#[tauri::command]
pub fn lan_stop(lan: tauri::State<'_,Lan>) {
    if let Some(s)=lan.0.lock().unwrap().take(){s.close();}
}
#[tauri::command]
pub fn lan_approve(lan: tauri::State<'_,Lan>, id:String, accept:bool) -> Result<(),String>{
    let s=active(&lan)?;
    let mut room=s.inner.lock().unwrap(); room.prune();
    if !room.live(){return Err("会话已结束".into());}
    let pair=room.pair.as_mut().filter(|p|p.id==id && !p.approved).ok_or("连接请求已失效")?;
    if accept {pair.approved=true;room.last=Instant::now();} else {room.pair=None;}
    Ok(())
}
#[tauri::command]
pub fn lan_send(lan: tauri::State<'_,Lan>, name:String, kind:String, contents:String) -> Result<Item,String> {
    let s=active(&lan)?;
    let limit=if kind=="text"||kind=="link"{TEXT}else{ITEM.div_ceil(3)*4};
    if contents.len()>limit{return Err("内容超过单项上限".into());}
    let bytes=if kind=="text"||kind=="link"{contents.into_bytes()}else{STANDARD.decode(contents).map_err(|_|"文件编码无效")?};
    let result=s.inner.lock().unwrap().add(name,kind,Bytes::from(bytes),false);
    result
}
fn entry(s:&Session,id:&str,received_only:bool)->Result<(Item,Bytes),String>{
    let mut room=s.inner.lock().unwrap();room.prune();
    if !room.live(){return Err("会话已结束".into());}
    let e=room.items.iter().find(|e|e.info.id==id && (!received_only || e.info.incoming)).ok_or("内容已过期或被移除")?;
    Ok((e.info.clone(),e.data.clone()))
}
#[tauri::command]
pub fn lan_read(lan:tauri::State<'_,Lan>,id:String)->Result<String,String>{
    let s=active(&lan)?;
    let (info,data)=entry(&s,&id,false)?;
    match info.kind.as_str(){
        _ if info.mime=="text/plain"=>Ok(String::from_utf8_lossy(&data).into_owned()),
        "image"=>Ok(format!("data:{};base64,{}",info.mime,STANDARD.encode(data))),
        _=>Err("请使用另存为".into())
    }
}
#[tauri::command]
pub async fn lan_save(window:WebviewWindow,lan:tauri::State<'_,Lan>,id:String)->Result<Option<String>,String>{
    let s=active(&lan)?;
    let (info,data)=entry(&s,&id,true)?;
    super::backup::save_bytes(window,info.name,data.to_vec(),false).await
}
#[tauri::command]
pub async fn lan_copy_file(window:WebviewWindow,lan:tauri::State<'_,Lan>,id:String)->Result<(),String>{
    let s=active(&lan)?;
    tauri::async_runtime::spawn_blocking(move || {
        let mut room=s.inner.lock().unwrap();
        room.prune();
        if !room.live(){return Err("会话已结束".into());}
        let e=room.items.iter_mut().find(|e|e.info.id==id && matches!(e.info.kind.as_str(),"file"|"image"))
            .ok_or("文件已过期或被移除")?;
        if e.clipboard.is_none() {
            e.clipboard=Some(ClipboardFile::new(&e.info.name,&e.data)?);
        }
        super::shortcuts::copy_file(&window,&e.clipboard.as_ref().unwrap().path)
    }).await.map_err(|_|"复制任务中断")?
}
#[tauri::command]
pub fn lan_remove(lan:tauri::State<'_,Lan>,id:String)->Result<(),String>{
    active(&lan)?.inner.lock().unwrap().items.retain(|e|e.info.id!=id);
    Ok(())
}

#[derive(Deserialize)]
struct TextInput { name:String, kind:String, text:String }
fn auth(room:&Room,headers:&HeaderMap,peer:IpAddr)->Option<bool>{
    let value=headers.get(header::AUTHORIZATION)?.to_str().ok()?.strip_prefix("Bearer ")?;
    room.pair.as_ref().filter(|p|p.id==value && p.ip==peer).map(|p|p.approved)
}
fn error(code:StatusCode,msg:&'static str)->HttpError{(code,msg)}
fn secure(mut response:Response)->Response {
    let h=response.headers_mut();
    h.insert(header::CACHE_CONTROL,"no-store".parse().unwrap());
    h.insert(header::CONTENT_SECURITY_POLICY,"default-src 'none'; script-src 'self'; style-src 'unsafe-inline'; connect-src 'self'; img-src blob: data:; base-uri 'none'; frame-ancestors 'none'".parse().unwrap());
    h.insert(header::X_CONTENT_TYPE_OPTIONS,"nosniff".parse().unwrap());
    h.insert(header::REFERRER_POLICY,"no-referrer".parse().unwrap());
    response
}
async fn http(State(s):State<Arc<Session>>,ConnectInfo(peer):ConnectInfo<SocketAddr>,req:Request<Body>)->Response {
    secure(route(s,peer.ip(),req).await.unwrap_or_else(IntoResponse::into_response))
}
async fn route(s:Arc<Session>,peer:IpAddr,req:Request<Body>)->Result<Response,HttpError>{
    let headers=req.headers();
    let host=headers.get(header::HOST).and_then(|h|h.to_str().ok());
    let origin=headers.get(header::ORIGIN).and_then(|h|h.to_str().ok());
    if host!=Some(s.host.as_str()) || origin.is_some_and(|o|o!=format!("http://{}",s.host)) {
        return Err(error(StatusCode::FORBIDDEN,"地址不匹配"));
    }
    let path=req.uri().path().to_owned();
    let method=req.method().clone();
    {
        let mut room=s.inner.lock().unwrap();room.prune();
        if !room.live(){return Err(error(StatusCode::GONE,"互传已结束"));}
        if method=="GET" && path=="/" {return Ok(axum::response::Html(PHONE).into_response());}
        if method=="GET" && path=="/phone.js" {return Ok(([(header::CONTENT_TYPE,"text/javascript; charset=utf-8")],PHONE_JS).into_response());}
        if method=="POST" && path=="/api/pair" {
            let code=headers.get("x-desklet-code").and_then(|v|v.to_str().ok());
            if code!=Some(s.code.as_str()){return Err(error(StatusCode::FORBIDDEN,"配对码无效"));}
            if room.pair.is_some(){return Err(error(StatusCode::CONFLICT,"已有手机连接或等待确认"));}
            let id=key();
            room.pair=Some(Pair{id:id.clone(),ip:peer,since:Instant::now(),approved:false});
            return Ok(axum::Json(serde_json::json!({"id":id})).into_response());
        }
        let approved=auth(&room,headers,peer).ok_or(error(StatusCode::UNAUTHORIZED,"连接已断开"))?;
        if method=="GET" && path=="/api/status" {
            let items:Vec<_>=if approved {room.items.iter().map(|e|e.info.clone()).collect()} else {Vec::new()};
            return Ok(axum::Json(serde_json::json!({"approved":approved,"items":items})).into_response());
        }
        if !approved{return Err(error(StatusCode::FORBIDDEN,"等待电脑确认"));}
        if method=="GET" {
            if let Some(id)=path.strip_prefix("/api/view/") {
                let e=room.items.iter().find(|e|e.info.id==id && e.info.mime!="application/octet-stream")
                    .ok_or(error(StatusCode::NOT_FOUND,"内容已过期"))?;
                return Ok(([(header::CONTENT_TYPE,e.info.mime.as_str())],e.data.clone()).into_response());
            }
        }
        if method=="GET" && path.starts_with("/api/download/") {
            let e=room.items.iter().find(|e|!e.info.incoming && e.info.id==&path[14..]).ok_or(error(StatusCode::NOT_FOUND,"内容已过期"))?;
            let data=e.data.clone();room.last=Instant::now();
            return Ok(([(header::CONTENT_TYPE,"application/octet-stream"),(header::CONTENT_DISPOSITION,"attachment")],data).into_response());
        }
    }
    if method!="POST" || (path!="/api/file" && path!="/api/text"){return Err(error(StatusCode::NOT_FOUND,"找不到此功能"));}
    let _slot=s.uploads.try_acquire().map_err(|_|error(StatusCode::TOO_MANY_REQUESTS,"有传输正在进行，请稍后再试"))?;
    s.inner.lock().unwrap().last = Instant::now();
    let name=headers.get("x-desklet-name").and_then(|h|h.to_str().ok()).and_then(|s|STANDARD.decode(s).ok()).and_then(|b|String::from_utf8(b).ok()).unwrap_or_else(||"文件".into());
    let limit=if path=="/api/text"{TEXT+4096}else{ITEM};
    let headers=headers.clone();
    let data=timeout(Duration::from_secs(120),to_bytes(req.into_body(),limit)).await
        .map_err(|_|error(StatusCode::REQUEST_TIMEOUT,"传输超时"))?
        .map_err(|_|error(StatusCode::PAYLOAD_TOO_LARGE,"文件超过单项限制或传输已中断"))?;
    let (name,kind,data)=if path=="/api/text"{
        let item:TextInput=serde_json::from_slice(&data).map_err(|_|error(StatusCode::BAD_REQUEST,"文字数据无效"))?;
        if item.kind!="text"&&item.kind!="link"{return Err(error(StatusCode::BAD_REQUEST,"内容类型无效"));}
        (item.name,item.kind,Bytes::from(item.text))
    }else{(name,"file".into(),data)};
    let mut room=s.inner.lock().unwrap();
    if auth(&room,&headers,peer)!=Some(true){return Err(error(StatusCode::UNAUTHORIZED,"连接已断开"));}
    match room.add(name,kind,data,true){
        Ok(item)=>Ok(axum::Json(item).into_response()),
        Err(msg)=>Ok((StatusCode::BAD_REQUEST,msg).into_response()),
    }
}


#[tauri::command]
pub async fn lan_send_path(lan:tauri::State<'_,Lan>,path:String)->Result<Item,String>{
    use std::{io::Read,path::PathBuf};
    let s=active(&lan)?;
    let path=PathBuf::from(path);
    if !path.is_absolute(){return Err("文件路径无效".into());}
    tauri::async_runtime::spawn_blocking(move || {
        let file=std::fs::File::open(&path).map_err(|_|"读不到文件")?;
        let meta=file.metadata().map_err(|_|"读不到文件信息")?;
        if !meta.is_file() || meta.len() > ITEM as u64 || meta.len()==0{return Err("请拖入非空文件，单项最多 32 MB".into());}
        let name=path.file_name().ok_or("文件名无效")?.to_string_lossy().into_owned();
        let mut data=Vec::new();
        file.take(ITEM as u64 + 1).read_to_end(&mut data).map_err(|_|"读取文件失败")?;
        let result=s.inner.lock().unwrap().add(name,"file".into(),Bytes::from(data),false);
        result
    }).await.map_err(|_|"发送任务中断")?
}
