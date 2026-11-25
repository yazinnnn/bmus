//! 弹幕引擎：视频弹幕（XML 分段拉取）+ 直播弹幕（WebSocket + zlib/brotli）。
//!
//! 两者都跑在独立线程里，通过 mpsc 把解析好的行丢给 TUI，避免阻塞界面。

use anyhow::{anyhow, Result};
use serde_json::Value;
use std::io::Read;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::api::Api;
use crate::config::Config;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DmKind {
    Danmu,
    Gift,
    SuperChat,
    Rank,
}

#[derive(Debug, Clone)]
pub struct DmLine {
    /// 视频内时间点（秒）；直播为 0
    pub at: f64,
    pub user: String,
    pub text: String,
    pub color: u32,
    pub mode: u8,
    pub kind: DmKind,
}

impl DmLine {
    fn new(at: f64, user: &str, text: &str, color: u32, mode: u8, kind: DmKind) -> Self {
        Self {
            at,
            user: user.to_string(),
            text: text.to_string(),
            color,
            mode,
            kind,
        }
    }
}

pub enum DmEvent {
    /// 新弹幕（视频按 360s 一段陆续到达）
    Lines(Vec<DmLine>),
    /// 状态/错误提示
    Note(String),
    /// 连接结束
    End,
}

// ---------------- 视频弹幕 ----------------

/// 视频弹幕线程：按 360 秒一段顺序拉取并回传
pub fn spawn_video(
    cfg: Config,
    cid: i64,
    aid: i64,
    duration: u64,
    tx: Sender<DmEvent>,
    stop: Arc<AtomicBool>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let api = match Api::new(&cfg) {
            Ok(a) => a,
            Err(e) => {
                let _ = tx.send(DmEvent::Note(format!("弹幕初始化失败: {}", e)));
                return;
            }
        };
        let segs = ((duration.max(1) + 359) / 360).clamp(1, 30) as u32;
        let mut seen: std::collections::HashSet<(i64, String)> = std::collections::HashSet::new();
        let mut prev_digest = String::new();
        for seg in 1..=segs {
            if stop.load(Ordering::Relaxed) {
                break;
            }
            match api.dm_xml(cid, aid, seg) {
                Ok(xml) => {
                    // 服务端常常忽略 segment_index，几段返回同一份内容：内容没变就不再翻
                    let digest = format!("{:x}", md5::compute(&xml));
                    if digest == prev_digest {
                        break;
                    }
                    prev_digest = digest;
                    let mut fresh = Vec::new();
                    for l in parse_video_xml(&xml) {
                        let key = (l.at as i64, l.text.clone());
                        if seen.insert(key) {
                            fresh.push(l);
                        }
                    }
                    if !fresh.is_empty() {
                        fresh.sort_by(|a, b| a.at.partial_cmp(&b.at).unwrap_or(std::cmp::Ordering::Equal));
                        let _ = tx.send(DmEvent::Lines(fresh));
                    }
                }
                Err(e) => {
                    if seg == 1 {
                        let _ = tx.send(DmEvent::Note(format!("弹幕拉取失败: {}", e)));
                    }
                }
            }
        }
        let _ = tx.send(DmEvent::End);
    })
}

/// `<d p="时间,模式,字号,颜色,...">文本</d>`
pub fn parse_video_xml(xml: &str) -> Vec<DmLine> {
    let mut out = Vec::new();
    let mut rest = xml;
    while let Some(pos) = rest.find("<d p=\"") {
        let after = &rest[pos + 7..];
        let (attr, tail) = match after.find("\">") {
            Some(e) => (&after[..e], &after[e + 2..]),
            None => break,
        };
        let (body, next) = match tail.find("</d>") {
            Some(e) => (&tail[..e], &tail[e + 4..]),
            None => break,
        };
        rest = next;

        let fields: Vec<&str> = attr.split(',').collect();
        let at: f64 = fields.first().and_then(|s| s.parse().ok()).unwrap_or(0.0);
        let mode: u8 = fields.get(1).and_then(|s| s.parse().ok()).unwrap_or(1);
        let color: u32 = fields.get(3).and_then(|s| s.parse().ok()).unwrap_or(0xFFFFFF);
        let text = unescape(body);
        if text.trim().is_empty() {
            continue;
        }
        // 视频弹幕没有用户名，B 站 XML 里也没有；用空串占位
        out.push(DmLine::new(at, "", &text, color, mode, DmKind::Danmu));
    }
    out
}

fn unescape(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&nbsp;", " ")
}

// ---------------- 直播弹幕 ----------------

/// 直播弹幕线程：拿到 token 后连 comet，断线自动重连（复用 token，认证失败才重取）
pub fn spawn_live(
    cfg: Config,
    room: i64,
    tx: Sender<DmEvent>,
    stop: Arc<AtomicBool>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let api = match Api::new(&cfg) {
            Ok(a) => a,
            Err(e) => {
                let _ = tx.send(DmEvent::Note(format!("弹幕初始化失败: {}", e)));
                return;
            }
        };

        // token + 节点缓存：只在首次 / 认证失败 / 全部节点失败时重新请求
        let mut info: Option<(String, Vec<String>)> = None;
        let mut host_idx = 0usize;
        let mut failures = 0u32;

        while !stop.load(Ordering::Relaxed) {
            if info.is_none() {
                match api.live_danmu_info(room) {
                    Ok(v) => {
                        info = Some(v);
                        host_idx = 0;
                    }
                    Err(e) => {
                        let _ = tx.send(DmEvent::Note(format!("{}", e)));
                        std::thread::sleep(Duration::from_secs(5));
                        continue;
                    }
                }
            }
            let (token, hosts) = info.clone().unwrap();
            let host = hosts[host_idx % hosts.len()].clone();
            let url = format!("wss://{}/sub", host);
            match connect_and_read(&url, &token, room, &cfg, &tx, &stop) {
                Ok(()) => {
                    failures = 0;
                }
                Err(e) => {
                    failures += 1;
                    let _ = tx.send(DmEvent::Note(format!("弹幕连接: {}（{}s 后重连）", e, 3)));
                    // 连续失败视为 token 失效，下次重新取
                    if failures >= 2 {
                        info = None;
                    } else {
                        host_idx += 1;
                    }
                    std::thread::sleep(Duration::from_secs(3));
                }
            }
            if stop.load(Ordering::Relaxed) {
                break;
            }
            host_idx += 1;
            std::thread::sleep(Duration::from_millis(500));
        }
        let _ = tx.send(DmEvent::End);
    })
}

fn connect_and_read(
    url: &str,
    token: &str,
    room: i64,
    cfg: &Config,
    tx: &Sender<DmEvent>,
    stop: &Arc<AtomicBool>,
) -> Result<()> {
    use tungstenite::stream::MaybeTlsStream;
    use tungstenite::Message;

    let (mut ws, _resp) = tungstenite::connect(url)?;
    // 读超时：让心跳/停止标志能定期检查，避免死连接卡住线程
    match ws.get_mut() {
        MaybeTlsStream::Plain(s) => {
            let _ = s.set_read_timeout(Some(Duration::from_secs(25)));
        }
        MaybeTlsStream::Rustls(s) => {
            let _ = s.sock.set_read_timeout(Some(Duration::from_secs(25)));
        }
        _ => {}
    }

    let buvid = cfg
        .buvid3
        .clone();
    let auth = serde_json::json!({
        "uid": cfg.dede_user_id.parse::<i64>().unwrap_or(0),
        "roomid": room,
        "protover": 3,
        "platform": "web",
        "type": 2,
        "key": token,
        "buvid": buvid,
    });
    ws.send(Message::Binary(pack(7, auth.to_string().as_bytes())))?;

    let mut last_hb = Instant::now();
    loop {
        if stop.load(Ordering::Relaxed) {
            let _ = ws.close(None);
            return Ok(());
        }
        match ws.read() {
            Ok(Message::Binary(data)) => {
                let mut lines = Vec::new();
                let mut sys = String::new();
                parse_packet(&data, &mut lines, &mut sys);
                if !lines.is_empty() {
                    let _ = tx.send(DmEvent::Lines(lines));
                }
                if !sys.is_empty() {
                    let _ = tx.send(DmEvent::Note(sys));
                }
            }
            Ok(Message::Text(t)) => {
                let mut lines = Vec::new();
                if let Ok(v) = serde_json::from_str::<Value>(&t) {
                    parse_json(&v, &mut lines);
                }
                if !lines.is_empty() {
                    let _ = tx.send(DmEvent::Lines(lines));
                }
            }
            Ok(Message::Close(_)) => return Ok(()),
            Ok(_) => {}
            Err(tungstenite::Error::Io(e))
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                if last_hb.elapsed() >= Duration::from_secs(25) {
                    let _ = ws.send(Message::Binary(pack(2, b"[object Object]")));
                    last_hb = Instant::now();
                }
                continue;
            }
            Err(tungstenite::Error::ConnectionClosed) | Err(tungstenite::Error::AlreadyClosed) => {
                return Ok(())
            }
            Err(e) => return Err(anyhow!("{}", e)),
        }
        if last_hb.elapsed() >= Duration::from_secs(25) {
            let _ = ws.send(Message::Binary(pack(2, b"[object Object]")));
            last_hb = Instant::now();
        }
    }
}

fn pack(op: u32, body: &[u8]) -> Vec<u8> {
    let total = 16 + body.len();
    let mut out = Vec::with_capacity(total);
    out.extend_from_slice(&(total as u32).to_be_bytes()); // 包长
    out.extend_from_slice(&16u16.to_be_bytes()); // 头长
    out.extend_from_slice(&1u16.to_be_bytes()); // 版本
    out.extend_from_slice(&op.to_be_bytes()); // 操作
    out.extend_from_slice(&1u32.to_be_bytes()); // 序号
    out.extend_from_slice(body);
    out
}

/// 逐层拆包：ver=2 走 zlib，ver=3 走 brotli，拆完再解析 JSON
fn parse_packet(data: &[u8], out: &mut Vec<DmLine>, sys: &mut String) {
    if data.len() < 16 {
        return;
    }
    let total = u32::from_be_bytes([data[0], data[1], data[2], data[3]]) as usize;
    let ver = u16::from_be_bytes([data[6], data[7]]);
    let op = u32::from_be_bytes([data[8], data[9], data[10], data[11]]);
    let body = &data[16..total.min(data.len())];

    match op {
        8 => {
            let t = String::from_utf8_lossy(body).to_string();
            if !t.contains("\"code\":0") {
                *sys = format!("弹幕认证失败: {}", t);
            }
        }
        5 => match ver {
            2 => {
                if let Ok(d) = zlib_decompress(body) {
                    split_nested(&d, out, sys);
                }
            }
            3 => {
                if let Ok(d) = brotli_decompress(body) {
                    split_nested(&d, out, sys);
                }
            }
            _ => {
                // 未压缩：body 里还可能串着多个 16 字节头的小包
                split_nested(body, out, sys);
            }
        },
        _ => {}
    }
}

/// 一层解压后可能仍然包含多个带包头的小包
fn split_nested(mut buf: &[u8], out: &mut Vec<DmLine>, sys: &mut String) {
    let mut guard = 0;
    while buf.len() >= 16 && guard < 2000 {
        guard += 1;
        let tot = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
        if tot < 16 || tot > buf.len() {
            break;
        }
        let ver = u16::from_be_bytes([buf[6], buf[7]]);
        let op = u32::from_be_bytes([buf[8], buf[9], buf[10], buf[11]]);
        let body = &buf[16..tot];
        if ver == 2 || ver == 3 {
            if let Ok(d) = if ver == 2 {
                zlib_decompress(body)
            } else {
                brotli_decompress(body)
            } {
                split_nested(&d, out, sys);
            }
        } else if op == 5 {
            if let Ok(v) = serde_json::from_slice::<Value>(body) {
                parse_json(&v, out);
            }
        } else if op == 8 {
            let t = String::from_utf8_lossy(body).to_string();
            if !t.contains("\"code\":0") {
                *sys = format!("弹幕认证失败: {}", t);
            }
        }
        buf = &buf[tot..];
    }
}

fn zlib_decompress(b: &[u8]) -> Result<Vec<u8>> {
    let mut d = flate2::read::ZlibDecoder::new(b);
    let mut out = Vec::new();
    d.read_to_end(&mut out)?;
    Ok(out)
}

fn brotli_decompress(b: &[u8]) -> Result<Vec<u8>> {
    let mut d = brotli::Decompressor::new(b, 4096);
    let mut out = Vec::new();
    d.read_to_end(&mut out)?;
    Ok(out)
}

fn parse_json(v: &Value, out: &mut Vec<DmLine>) {
    let cmd = v["cmd"].as_str().unwrap_or("");
    if cmd.starts_with("DANMU_MSG") {
        let text = v["info"][1].as_str().unwrap_or("");
        let user = v["info"][2][1].as_str().unwrap_or("");
        let color = v["info"][0][3].as_u64().unwrap_or(0xFF_FF_FF) as u32;
        let mode = v["info"][0][1].as_u64().unwrap_or(1) as u8;
        if !text.trim().is_empty() {
            out.push(DmLine::new(0.0, user, text, color, mode, DmKind::Danmu));
        }
    } else if cmd == "SEND_GIFT" {
        let user = v["data"]["uname"].as_str().unwrap_or("");
        let gift = v["data"]["giftName"].as_str().unwrap_or("礼物");
        let num = v["data"]["num"].as_u64().unwrap_or(1);
        out.push(DmLine::new(
            0.0,
            user,
            &format!("投喂 {} ×{}", gift, num),
            0xFF_C1_07,
            2,
            DmKind::Gift,
        ));
    } else if cmd == "SUPER_CHAT_MESSAGE" {
        let user = v["data"]["user_info"]["uname"].as_str().unwrap_or("");
        let text = v["data"]["message"].as_str().unwrap_or("");
        let price = v["data"]["price"].as_u64().unwrap_or(0);
        out.push(DmLine::new(
            0.0,
            user,
            &format!("[SC ¥{}] {}", price, text),
            0xFF_66_99,
            2,
            DmKind::SuperChat,
        ));
    } else if cmd == "GUARD_BUY" {
        let user = v["data"]["username"].as_str().unwrap_or("");
        let level = match v["data"]["guard_level"].as_u64().unwrap_or(0) {
            1 => "总督",
            2 => "提督",
            _ => "舰长",
        };
        out.push(DmLine::new(
            0.0,
            user,
            &format!("开通{}", level),
            0xFF_8C_00,
            2,
            DmKind::Gift,
        ));
    }
}
