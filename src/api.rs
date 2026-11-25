//! bilibili Web API 封装：WBI 签名、搜索、热门、收藏、音轨解析、直播拉流。

use anyhow::{anyhow, bail, Context, Result};
use serde_json::Value;
use std::io::Read;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub const UA: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36";
const REF_VIDEO: &str = "https://www.bilibili.com/";
const REF_LIVE: &str = "https://live.bilibili.com/";

/// wbi mixin 重排表（B 站 web 端固定顺序）
const MIXIN_TAB: [usize; 64] = [
    46, 47, 18, 2, 53, 8, 23, 32, 15, 50, 10, 31, 58, 3, 45, 35, 27, 43, 5, 49, 33, 9, 42, 19,
    29, 28, 14, 39, 12, 38, 41, 13, 37, 48, 7, 16, 24, 55, 40, 61, 26, 17, 0, 1, 60, 51, 30,
    4, 22, 25, 54, 21, 56, 59, 6, 63, 57, 62, 11, 36, 20, 34, 44, 52,
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Video,
    Live,
}

#[derive(Debug, Clone)]
pub struct Page {
    pub page: i64,
    pub cid: i64,
    pub part: String,
    pub duration: u64,
}

#[derive(Debug, Clone)]
pub struct Track {
    pub bvid: Option<String>,
    pub aid: i64,
    pub cid: Option<i64>,
    pub title: String,
    pub author: String,
    pub duration: u64,
    pub source: Source,
    pub room_id: Option<i64>,
    pub pages: Vec<Page>,
    pub pubdate: u64,
    /// UP 主 uid，用于下载目录归并
    pub up_mid: i64,
}

impl Track {
    #[allow(dead_code)]
    pub fn key(&self) -> String {
        match self.source {
            Source::Video => self.bvid.clone().unwrap_or_else(|| format!("av{}", self.aid)),
            Source::Live => format!("live{}", self.room_id.unwrap_or(0)),
        }
    }
}

impl std::fmt::Display for Track {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.title)
    }
}

/// 解析出来的可播放地址（第一个为主，其余为备用 CDN）
#[derive(Debug, Clone)]
pub struct StreamUrls {
    pub urls: Vec<String>,
    pub audio_only: bool,
    pub label: String,
    pub codec: String,
    pub bitrate: u64,
}

pub struct Api {
    http: reqwest::blocking::Client,
    cookie: Mutex<String>,
    wbi: Mutex<Option<(String, String)>>,
    /// 当前登录用户 mid（未登录为 0）
    mid: i64,
    /// best / lowest / 30216 ...
    pub audio_quality: String,
    /// 上一次请求时间，用于全局限速（避免 -799 / 412）
    pace: Mutex<Option<std::time::Instant>>,
    /// video_detail 缓存，避免同一视频重复请求
    detail_cache: Mutex<std::collections::HashMap<String, Track>>,
}

impl Api {
    pub fn new(cfg: &crate::config::Config) -> Result<Self> {
        let http = reqwest::blocking::Client::builder()
            .user_agent(UA)
            .timeout(Duration::from_secs(15))
            .connect_timeout(Duration::from_secs(8))
            .gzip(true)
            .deflate(false)
            .build()?;

        let mut cookie = cfg.cookie();
        if cfg.buvid3.is_empty() {
            // 没有 buvid 会被风控拦成 HTML 错误页
            if let Ok(d) = http
                .get("https://api.bilibili.com/x/frontend/finger/spi")
                .header("User-Agent", UA)
                .send()
                .and_then(|r| r.json::<Value>())
            {
                let b3 = d["data"]["b_3"].as_str().unwrap_or("").to_string();
                let b4 = d["data"]["b_4"].as_str().unwrap_or("").to_string();
                let mut c = cfg.clone();
                c.buvid3 = b3.clone();
                c.buvid4 = b4.clone();
                let _ = c.save();
                cookie = cfg.cookie();
                let extra = format!("buvid3={}; buvid4={}", b3, b4);
                cookie = if cookie.is_empty() { extra } else { format!("{}; {}", cookie, extra) };
            }
        }

        Ok(Self {
            http,
            cookie: Mutex::new(cookie),
            wbi: Mutex::new(None),
            mid: cfg.dede_user_id.trim().parse::<i64>().unwrap_or(0),
            audio_quality: cfg.audio_quality.clone(),
            pace: Mutex::new(None),
            detail_cache: Mutex::new(std::collections::HashMap::new()),
        })
    }

    fn cookie(&self) -> String {
        self.cookie.lock().unwrap().clone()
    }

    /// buvid 失效会触发风控（search 只返回 v_voucher），重新取一份指纹
    fn refresh_buvid(&self) {
        let spi = self
            .http
            .get("https://api.bilibili.com/x/frontend/finger/spi")
            .header("User-Agent", UA)
            .send()
            .and_then(|r| r.json::<Value>());
        let b3 = spi
            .as_ref()
            .ok()
            .and_then(|d| d["data"]["b_3"].as_str())
            .unwrap_or("")
            .to_string();
        let b4 = spi
            .as_ref()
            .ok()
            .and_then(|d| d["data"]["b_4"].as_str())
            .unwrap_or("")
            .to_string();
        if b3.is_empty() {
            return;
        }
        let mut cfg = crate::config::Config::load();
        cfg.buvid3 = b3.clone();
        cfg.buvid4 = b4.clone();
        let _ = cfg.save();
        let ck = cfg.cookie();
        *self.cookie.lock().unwrap() = ck;
        *self.wbi.lock().unwrap() = None;
    }

    pub fn raw(&self, url: &str, referer: &str) -> Result<Value> {
        self.raw_ck(url, referer, &self.cookie())
    }

    /// 去掉 buvid 的 cookie（未登录时 search 带 buvid 反而会触发 v_voucher）
    fn cookie_base(&self) -> String {
        let full = self.cookie();
        full.split(';')
            .map(|s| s.trim())
            .filter(|s| !s.starts_with("buvid3=") && !s.starts_with("buvid4=") && !s.is_empty())
            .collect::<Vec<_>>()
            .join("; ")
    }

    /// 全局最小请求间隔，降低风控概率
    fn pace(&self) {
        let mut g = self.pace.lock().unwrap();
        if let Some(t) = *g {
            let gap = std::time::Duration::from_millis(700);
            if let Some(rest) = gap.checked_sub(t.elapsed()) {
                std::thread::sleep(rest);
            }
        }
        *g = Some(std::time::Instant::now());
    }

    fn raw_ck(&self, url: &str, referer: &str, ck: &str) -> Result<Value> {
        let mut last_msg = String::new();
        for attempt in 0..3u32 {
            self.pace();
            let mut r = self
                .http
                .get(url)
                .header("Referer", referer)
                .header("Origin", referer.trim_end_matches('/'));
            if !ck.is_empty() {
                r = r.header("Cookie", ck);
            }
            match r.send() {
                Ok(resp) => {
                    let status = resp.status();
                    let text = resp.text().unwrap_or_default();
                    // 限流/风控：-799 / -352 / -509 / HTTP 412，退避重试
                    let parsed = serde_json::from_str::<Value>(&text).ok();
                    let code = parsed
                        .as_ref()
                        .and_then(|v| v["code"].as_i64())
                        .unwrap_or(0);
                    let risky = status.as_u16() == 412
                        || matches!(code, -799 | -352 | -509 | -412);
                    match parsed {
                        Some(v) if !risky => return Ok(v),
                        Some(v) => {
                            last_msg = format!(
                                "接口限流 {}: {}",
                                v["code"],
                                v["message"].as_str().unwrap_or("")
                            );
                        }
                        None => {
                            last_msg = format!("HTTP {} 非 JSON（风控/需要登录）", status);
                        }
                    }
                    if attempt == 0 {
                        self.refresh_buvid();
                    }
                    // 逐步加长退避：2s → 5s → 10s → 20s
                    let wait = match attempt {
                        0 => 1500,
                        1 => 3500,
                        _ => 7000,
                    };
                    std::thread::sleep(std::time::Duration::from_millis(wait));
                }
                Err(e) => {
                    last_msg = format!("请求失败: {}", e);
                    std::thread::sleep(std::time::Duration::from_millis(700));
                }
            }
        }
        bail!("{}（B 站限流，请等 1 分钟再按 > 重试）", last_msg)
    }

    fn data_ck(&self, url: &str, referer: &str, ck: &str) -> Result<Value> {
        let v = self.raw_ck(url, referer, ck)?;
        let code = v["code"].as_i64().unwrap_or(-1);
        if code != 0 {
            bail!("bilibili 接口错误 {}: {}", code, v["message"].as_str().unwrap_or("unknown"));
        }
        if !v["data"]["v_voucher"].is_null() {
            bail!("VOUCHER");
        }
        Ok(v["data"].clone())
    }

    pub fn data(&self, url: &str, referer: &str) -> Result<Value> {
        let v = self.raw(url, referer)?;
        let code = v["code"].as_i64().unwrap_or(-1);
        if code != 0 {
            bail!(
                "bilibili 接口错误 {}: {}",
                code,
                v["message"].as_str().unwrap_or("unknown")
            );
        }
        // v_voucher = 风控拦截，换指纹重试一次
        if !v["data"]["v_voucher"].is_null() {
            self.refresh_buvid();
            let v2 = self.raw(url, referer)?;
            if v2["code"].as_i64() != Some(0) {
                bail!("bilibili 风控（v_voucher），重试仍失败");
            }
            if !v2["data"]["v_voucher"].is_null() {
                bail!("bilibili 风控（v_voucher）：换个网络或稍后再试");
            }
            return Ok(v2["data"].clone());
        }
        Ok(v["data"].clone())
    }

    // ---------- WBI 签名 ----------

    fn wbi_keys(&self) -> Result<(String, String)> {
        if let Some(k) = self.wbi.lock().unwrap().clone() {
            return Ok(k);
        }
        let v = self.raw("https://api.bilibili.com/x/web-interface/nav", REF_VIDEO)?;
        let img = v["data"]["wbi_img"]["img_url"].as_str().unwrap_or("");
        let sub = v["data"]["wbi_img"]["sub_url"].as_str().unwrap_or("");
        if img.is_empty() || sub.is_empty() {
            bail!("获取 wbi_img 失败");
        }
        let stem = |u: &str| -> String {
            u.rsplit('/')
                .next()
                .unwrap_or("")
                .split('.')
                .next()
                .unwrap_or("")
                .to_string()
        };
        let k = (stem(img), stem(sub));
        *self.wbi.lock().unwrap() = Some(k.clone());
        Ok(k)
    }

    /// 补 wts、排序、算 w_rid；返回（带 wts 的参数表, 可直接拼到 url 的 query）
    fn wbi_sign(&self, mut params: Vec<(String, String)>) -> Result<(Vec<(String, String)>, String)> {
        let (img, sub) = self.wbi_keys()?;
        let raw = format!("{}{}", img, sub);
        let mixin: String = MIXIN_TAB
            .iter()
            .take(32)
            .filter_map(|&i| raw.chars().nth(i))
            .collect();

        if !params.iter().any(|(k, _)| k == "wts") {
            let wts = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs()
                .to_string();
            params.push(("wts".into(), wts));
        }
        params.sort_by(|a, b| a.0.cmp(&b.0));
        let query: String = params
            .iter()
            .map(|(k, v)| {
                let clean: String = v.chars().filter(|c| !"!'()*".contains(*c)).collect();
                format!("{}={}", k, urlencoding::encode(&clean))
            })
            .collect::<Vec<_>>()
            .join("&");
        let w_rid = format!("{:x}", md5::compute(format!("{}{}", query, mixin)));
        Ok((params.clone(), format!("{}&w_rid={}", query, w_rid)))
    }

    /// 返回带 w_rid/wts 的 query string（值已编码）
    fn signed_query(&self, params: Vec<(String, String)>) -> Result<String> {
        Ok(self.wbi_sign(params)?.1)
    }

    // ---------- 列表类接口 ----------

    pub fn popular(&self, ps: u32) -> Result<Vec<Track>> {
        let url = format!(
            "https://api.bilibili.com/x/web-interface/popular?ps={}&pn=1",
            ps.min(100)
        );
        let d = self.data(&url, REF_VIDEO)?;
        Ok(d["list"].as_array().cloned().unwrap_or_default().iter().map(video_from_popular).collect())
    }

    pub fn search(&self, keyword: &str, kind: &str, page: u32) -> Result<Vec<Track>> {
        let params = vec![
            ("search_type".to_string(), kind.to_string()),
            ("keyword".to_string(), keyword.to_string()),
            ("page".to_string(), page.to_string()),
        ];
        let q = self.signed_query(params)?;
        let url = format!("https://api.bilibili.com/x/web-interface/wbi/search/type?{}", q);
        let full = self.cookie();
        let base = self.cookie_base();
        let mut attempts: Vec<String> = Vec::new();
        if full.contains("SESSDATA=") {
            attempts.push(full.clone());
            attempts.push(base.clone());
        } else {
            attempts.push(base.clone());
            attempts.push(full.clone());
        }
        let mut last_err = None;
        let mut got: Option<Value> = None;
        for ck in &attempts {
            match self.data_ck(&url, REF_VIDEO, ck) {
                Ok(d) => {
                    got = Some(d);
                    break;
                }
                Err(e) => last_err = Some(e),
            }
        }
        if got.is_none() {
            // 指纹可能被标记，换一份再试
            self.refresh_buvid();
            if let Ok(d) = self.data_ck(&url, REF_VIDEO, &self.cookie_base()) {
                got = Some(d);
            }
        }
        let d = match got {
            Some(d) => d,
            None => {
                return Err(last_err.unwrap_or_else(|| anyhow!("搜索失败（风控）")))
            }
        };
        let result = d["result"].as_array().cloned().unwrap_or_default();
        Ok(match kind {
            "live_room" => result
                .iter()
                .map(live_from_search)
                .filter(|t| t.room_id.is_some())
                .collect(),
            _ => result
                .iter()
                .map(video_from_search)
                .filter(|t| {
                    t.bvid.as_deref().map(|b| !b.is_empty()).unwrap_or(false) || t.aid > 0
                })
                .collect(),
        })
    }

    pub fn fav_folders(&self) -> Result<Vec<(i64, String, u64)>> {
        let mid = self
            .cookie()
            .split("DedeUserID=")
            .nth(1)
            .map(|s| s.split(';').next().unwrap_or("").trim().to_string())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow!("未登录，先执行 `bmus login`"))?;
        let url = format!(
            "https://api.bilibili.com/x/v3/fav/folder/created/list-all?up_mid={}",
            mid
        );
        let d = self.data(&url, REF_VIDEO)?;
        Ok(d["list"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .iter()
            .map(|f| {
                (
                    f["id"].as_i64().unwrap_or(0),
                    f["title"].as_str().unwrap_or("").to_string(),
                    f["media_count"].as_u64().unwrap_or(0),
                )
            })
            .collect())
    }

    pub fn fav_items(&self, media_id: i64, page: u32) -> Result<Vec<Track>> {
        let url = format!(
            "https://api.bilibili.com/x/v3/fav/resource/list?media_id={}&pn={}&ps=20&platform=web",
            media_id, page
        );
        let d = self.data(&url, REF_VIDEO)?;
        Ok(d["medias"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .iter()
            .map(|m| Track {
                bvid: m["bvid"].as_str().map(|s| s.to_string()),
                aid: m["id"].as_i64().unwrap_or(0),
                cid: None,
                title: m["title"].as_str().unwrap_or("").to_string(),
                author: m["upper"]["name"].as_str().unwrap_or("").to_string(),
                duration: m["duration"].as_u64().unwrap_or(0),
                source: Source::Video,
                room_id: None,
                pages: vec![],
                pubdate: m["pubtime"].as_u64().unwrap_or(0),
                up_mid: m["upper"]["mid"].as_i64().unwrap_or(0),
            })
            .collect())
    }

    // ---------- 播放地址 ----------

    /// 视频信息（顺带拿到 cid）
    pub fn video_info(&self, bvid: &str) -> Result<(String, String, i64, u64)> {
        let url = format!("https://api.bilibili.com/x/web-interface/view?bvid={}", bvid);
        let d = self.data(&url, REF_VIDEO)?;
        let cid = d["cid"].as_i64().unwrap_or(0);
        Ok((
            d["title"].as_str().unwrap_or("").to_string(),
            d["owner"]["name"].as_str().unwrap_or("").to_string(),
            cid,
            d["duration"].as_u64().unwrap_or(0),
        ))
    }

    pub fn video_aid(&self, bvid: &str) -> Result<i64> {
        let d = self.data(
            &format!("https://api.bilibili.com/x/web-interface/view?bvid={}", bvid),
            REF_VIDEO,
        )?;
        Ok(d["aid"].as_i64().unwrap_or(0))
    }

    /// 只取 DASH 音轨（不请求视频流，省带宽）
    pub fn video_audio(&self, track: &Track) -> Result<StreamUrls> {
        let q = self.audio_quality.clone();
        self.video_audio_q(track, &q)
    }

    /// 临时指定音质：low=true 取最低码率
    pub fn video_audio_quality(&self, track: &Track, low: bool) -> Result<StreamUrls> {
        self.video_audio_q(track, if low { "lowest" } else { "best" })
    }

    fn video_audio_q(&self, track: &Track, quality: &str) -> Result<StreamUrls> {
        let bvid = track
            .bvid
            .clone()
            .ok_or_else(|| anyhow!("缺少 bvid"))?;
        let mut cid = track.cid.unwrap_or(0);
        if cid == 0 {
            cid = self.video_info(&bvid)?.2;
        }
        let url = format!(
            "https://api.bilibili.com/x/player/playurl?bvid={}&cid={}&fnval=16&fourk=1",
            bvid, cid
        );
        let d = self.data(&url, REF_VIDEO)?;
        let dash = &d["dash"];
        let audio = dash["audio"].as_array().cloned().unwrap_or_default();
        if audio.is_empty() {
            bail!("该视频没有 DASH 音轨（可能是互动视频/付费内容/需要登录）");
        }

        let pick = match quality {
            "lowest" => audio
                .iter()
                .min_by_key(|a| a["bandwidth"].as_u64().unwrap_or(0))
                .cloned(),
            "best" => audio
                .iter()
                .max_by_key(|a| a["bandwidth"].as_u64().unwrap_or(0))
                .cloned(),
            id => audio
                .iter()
                .find(|a| a["id"].as_i64().map(|x| x.to_string()) == Some(id.to_string()))
                .cloned(),
        }
        .unwrap_or_else(|| audio.last().cloned().unwrap());

        let mut urls = Vec::new();
        if let Some(u) = pick["baseUrl"].as_str() {
            urls.push(u.to_string());
        }
        for u in pick["backupUrl"].as_array().cloned().unwrap_or_default() {
            if let Some(s) = u.as_str() {
                urls.push(s.to_string());
            }
        }
        let bw = pick["bandwidth"].as_u64().unwrap_or(0);
        let codec = pick["codecs"].as_str().unwrap_or("").to_string();
        Ok(StreamUrls {
            urls,
            audio_only: true,
            codec: codec.clone(),
            bitrate: bw / 1000,
            label: format!(
                "DASH 音轨 id={} {}kbps codec={}",
                pick["id"].as_i64().unwrap_or(0),
                bw / 1000,
                pick["codecs"].as_str().unwrap_or("?")
            ),
        })
    }

    /// 按 UP mid 查他的直播间（不管开播与否）
    pub fn up_live_room(&self, mid: i64) -> Result<Option<(i64, bool, String)>> {
        let url = format!(
            "https://api.live.bilibili.com/room/v1/Room/getRoomInfoOld?mid={}",
            mid
        );
        let d = self.data(&url, REF_LIVE)?;
        let room = d["roomid"].as_i64().unwrap_or(0);
        if room <= 0 {
            return Ok(None);
        }
        let live = d["liveStatus"].as_i64().unwrap_or(0) == 1;
        let title = d["title"].as_str().unwrap_or("").to_string();
        Ok(Some((room, live, title)))
    }

    pub fn live_info(&self, room_id: i64) -> Result<(String, String, bool)> {
        let url = format!("https://api.live.bilibili.com/room/v1/Room/get_info?room_id={}", room_id);
        let d = self.data(&url, REF_LIVE)?;
        Ok((
            d["title"].as_str().unwrap_or("").to_string(),
            d["anchor_info"]["base_info"]["uname"].as_str().unwrap_or("直播间").to_string(),
            d["live_status"].as_i64() == Some(1),
        ))
    }

    /// 直播是音视频复用流：取最低画质 + mpv --no-video，带宽仍高于纯音频
    pub fn live_stream(&self, room_id: i64, low: bool) -> Result<StreamUrls> {
        self.live_stream_qn(room_id, if low { 250 } else { 10000 }, low)
    }

    /// qn 必须写进请求里，否则服务端只会按默认档位（通常是 250）下发，本地改 qn 变量无效
    pub fn live_stream_qn(&self, room_id: i64, req_qn: i64, low: bool) -> Result<StreamUrls> {
        let url = format!(
            "https://api.live.bilibili.com/xlive/web-room/v2/index/getRoomPlayInfo?room_id={}&protocol=0,1&format=0,1,2&codec=0,1&qn={}&platform=web",
            room_id, req_qn
        );
        let d = self.data(&url, REF_LIVE)?;
        if d["live_status"].as_i64() != Some(1) {
            bail!("直播间未开播");
        }
        let streams = d["playurl_info"]["playurl"]["stream"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        if streams.is_empty() {
            bail!("没有拿到直播流（可能需要登录 / 付费直播间）");
        }

        let mut best: Option<(i64, String, String)> = None; // (score, url, label)
        for st in &streams {
            let proto = st["protocol_name"].as_str().unwrap_or("");
            for fmt in st["format"].as_array().cloned().unwrap_or_default() {
                let fname = fmt["format_name"].as_str().unwrap_or("");
                for c in fmt["codec"].as_array().cloned().unwrap_or_default() {
                    let cname = c["codec_name"].as_str().unwrap_or("");
                    if cname != "avc" {
                        continue; // 兼容性优先
                    }
                    let info = match c["url_info"].as_array().and_then(|a| a.first()) {
                        Some(i) => i,
                        None => continue,
                    };
                    let host = info["host"].as_str().unwrap_or("");
                    let base = c["base_url"].as_str().unwrap_or("");
                    let extra = info["extra"].as_str().unwrap_or("");
                    let full = format!("{}{}{}", host, base, extra);

                    let qns: Vec<i64> = c["accept_qn"]
                        .as_array()
                        .cloned()
                        .unwrap_or_default()
                        .iter()
                        .filter_map(|q| q.as_i64())
                        .collect();
                    let picked = c["current_qn"].as_i64().unwrap_or(req_qn);
                    let qn = if low {
                        qns.iter().min().copied().unwrap_or(picked)
                    } else {
                        picked
                    };
                    // 优先 fmp4 hls（省流、mpv 友好），其次 flv
                    let score = match (proto, fname) {
                        ("http_hls", "fmp4") => 3,
                        ("http_hls", "ts") => 2,
                        ("http_stream", "flv") => 1,
                        _ => 0,
                    };
                    if score > best.as_ref().map(|b| b.0).unwrap_or(0) {
                        best = Some((
                            score,
                            full,
                            format!("{} {} qn={}", proto, fname, qn),
                        ));
                    }
                }
            }
        }
        let (_, url, label) = best.ok_or_else(|| anyhow!("找不到可用的直播流地址"))?;
        Ok(StreamUrls {
            urls: vec![url],
            audio_only: false,
            label,
            codec: String::new(),
            bitrate: 0,
        })
    }
}

impl Api {
    /// 把 bvid / av号 / 视频链接 / 直播链接 / 关键词 解析成 Track
    pub fn resolve(&self, s: &str) -> Result<Track> {
        let s = s.trim();
        if s.is_empty() {
            bail!("目标为空");
        }

        if let Some(rest) = s.split("live.bilibili.com/").nth(1) {
            let id: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
            let room: i64 = id.parse().map_err(|_| anyhow!("直播间号无效: {}", rest))?;
            let (title, author, _live) = self.live_info(room)?;
            return Ok(Track {
                bvid: None,
                aid: 0,
                cid: None,
                title,
                author,
                duration: 0,
                source: Source::Live,
                room_id: Some(room),
                pages: vec![],
                pubdate: 0,
        up_mid: 0,
            });
        }

        if let Some(pos) = s.find("BV1") {
            let bvid: String = s[pos..].chars().take(12).collect();
            return self.video_detail(&bvid);
        }

        if let Some(rest) = s.strip_prefix("av") {
            if let Ok(aid) = rest.trim().parse::<i64>() {
                let v = self.raw(
                    &format!("https://api.bilibili.com/x/web-interface/view?aid={}", aid),
                    REF_VIDEO,
                )?;
                let code = v["code"].as_i64().unwrap_or(-1);
                if code != 0 {
                    bail!("bilibili 接口错误 {}: {}", code, v["message"]);
                }
                let bvid = v["data"]["bvid"].as_str().unwrap_or("").to_string();
                let (title, author, cid, duration) = self.video_info(&bvid)?;
                return Ok(Track {
                    bvid: Some(bvid),
                    aid,
                    cid: Some(cid),
                    title,
                    author,
                    duration,
                    source: Source::Video,
                    room_id: None,
                    pages: vec![],
                    pubdate: 0,
        up_mid: 0,
                });
            }
        }

        let first = self
            .search(s, "video", 1)?
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("没搜到「{}」", s))?;
        Ok(first)
    }

    // ---------- 弹幕 / 播放上报 / 高能榜 ----------

    /// 原始文本 GET（弹幕 XML 这类非 JSON 接口）
    fn raw_text(&self, url: &str, referer: &str) -> Result<String> {
        for attempt in 0..3u32 {
            self.pace();
            let mut r = self
                .http
                .get(url)
                .header("Referer", referer)
                .header("User-Agent", UA);
            let ck = self.cookie();
            if !ck.is_empty() {
                r = r.header("Cookie", ck);
            }
            match r.send() {
                Ok(resp) => {
                    let status = resp.status();
                    if status.as_u16() == 412 || status.as_u16() == 403 {
                        std::thread::sleep(Duration::from_millis(1500u64 * (attempt as u64 + 1)));
                        continue;
                    }
                    let raw = resp.bytes().unwrap_or_default();
                    if raw.is_empty() {
                        continue;
                    }
                    if let Ok(t) = std::str::from_utf8(&raw) {
                        if t.trim_start().starts_with('<') || t.trim_start().starts_with('{') {
                            return Ok(t.to_string());
                        }
                    }
                    // B 站弹幕 XML 用 raw deflate，reqwest 不自动解，自己来
                    for dec in ["zlib", "deflate"] {
                        let out = if dec == "zlib" {
                            let mut d = flate2::read::ZlibDecoder::new(&raw[..]);
                            let mut v = Vec::new();
                            d.read_to_end(&mut v).map(|_| v)
                        } else {
                            let mut d = flate2::read::DeflateDecoder::new(&raw[..]);
                            let mut v = Vec::new();
                            d.read_to_end(&mut v).map(|_| v)
                        };
                        if let Ok(v) = out {
                            if let Ok(t) = String::from_utf8(v) {
                                return Ok(t);
                            }
                        }
                    }
                    bail!("弹幕响应无法解码（{} 字节）", raw.len())
                }
                Err(e) => {
                    if attempt == 2 {
                        bail!("请求失败: {}", e);
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(800));
        }
        bail!("弹幕接口无响应（可能被限流）")
    }

    /// 普通 POST 表单（心跳这类不需要 wbi 的写接口）
    fn form_post(
        &self,
        base: &str,
        referer: &str,
        params: &[(String, String)],
    ) -> Result<Value> {
        self.pace();
        let body = params
            .iter()
            .map(|(k, v)| {
                format!(
                    "{}={}",
                    urlencoding::encode(k),
                    urlencoding::encode(v)
                )
            })
            .collect::<Vec<_>>()
            .join("&");
        let origin = referer
            .split('/')
            .take(3)
            .collect::<Vec<_>>()
            .join("/");
        let mut r = self
            .http
            .post(base)
            .header("Referer", referer)
            .header("Origin", origin)
            .header("User-Agent", UA)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body(body);
        let ck = self.cookie();
        if !ck.is_empty() {
            r = r.header("Cookie", ck);
        }
        let text = r.send()?.text().unwrap_or_default();
        serde_json::from_str::<Value>(&text)
            .with_context(|| format!("接口返回非 JSON（可能触发风控）: {}", text.chars().take(80).collect::<String>()))
    }

    /// wbi 签名的 POST：query 与 body 带同一批参数（B 站写接口按这个口径校验）
    fn signed_post(
        &self,
        base: &str,
        referer: &str,
        params: Vec<(String, String)>,
    ) -> Result<Value> {
        let (p, q) = self.wbi_sign(params)?;
        let url = format!("{}{}{}", base, if base.contains('?') { "&" } else { "?" }, q);
        self.pace();
        let body = p
            .iter()
            .map(|(k, v)| format!("{}={}", urlencoding::encode(k), urlencoding::encode(v)))
            .collect::<Vec<_>>()
            .join("&");
        let origin = referer.split('/').take(3).collect::<Vec<_>>().join("/");
        let mut r = self
            .http
            .post(&url)
            .header("Referer", referer)
            .header("Origin", origin)
            .header("User-Agent", UA)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body(body);
        let ck = self.cookie();
        if !ck.is_empty() {
            r = r.header("Cookie", ck);
        }
        let text = r.send()?.text().unwrap_or_default();
        serde_json::from_str::<Value>(&text).with_context(|| {
            format!(
                "接口返回非 JSON（可能触发风控）: {}",
                text.chars().take(80).collect::<String>()
            )
        })
    }

    /// 视频弹幕 XML（每 360 秒一段，segment_index 从 1 开始）
    pub fn dm_xml(&self, cid: i64, aid: i64, segment: u32) -> Result<String> {
        let url = format!(
            "https://api.bilibili.com/x/v1/dm/list.so?type=1&oid={}&pid={}&segment_index={}",
            cid, aid, segment
        );
        self.raw_text(&url, "https://www.bilibili.com/")
    }

    /// 直播弹幕连接信息：token + 可用 comet 节点
    pub fn live_danmu_info(&self, room: i64) -> Result<(String, Vec<String>)> {
        let params = vec![
            ("id".to_string(), room.to_string()),
            ("type".to_string(), "0".to_string()),
        ];
        let q = self.signed_query(params)?;
        let url = format!(
            "https://api.live.bilibili.com/xlive/web-room/v1/index/getDanmuInfo?{}",
            q
        );
        let v = self.data(&url, &format!("https://live.bilibili.com/{}", room))?;
        let token = v["token"].as_str().unwrap_or("").to_string();
        let hosts: Vec<String> = v["host_list"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|h| h["host"].as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        if token.is_empty() || hosts.is_empty() {
            bail!("getDanmuInfo 没有返回 token（未登录或风控）");
        }
        Ok((token, hosts))
    }

    /// 视频播放心跳：B 站靠它统计播放与写历史
    pub fn video_heartbeat(&self, aid: i64, cid: i64, played: u64) -> Result<()> {
        if !self.logged_in() || self.mid == 0 {
            return Ok(());
        }
        let csrf = self
            .csrf()
            .ok_or_else(|| anyhow!("未登录（:login 扫码后才有 bili_jct）"))?;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let start = now.saturating_sub(played);
        let params = vec![
            ("aid".to_string(), aid.to_string()),
            ("cid".to_string(), cid.to_string()),
            ("mid".to_string(), self.mid.to_string()),
            ("type".to_string(), "3".to_string()),
            ("sub_type".to_string(), "0".to_string()),
            ("played_time".to_string(), played.to_string()),
            ("real_played_time".to_string(), played.to_string()),
            ("realtime".to_string(), played.to_string()),
            ("start_ts".to_string(), start.to_string()),
            ("dt".to_string(), "2".to_string()),
            ("last_play_progress_time".to_string(), played.to_string()),
            ("play_type".to_string(), "0".to_string()),
            ("csrf".to_string(), csrf),
        ];
        let v = self.form_post(
            "https://api.bilibili.com/x/click-interface/web/heartbeat",
            REF_VIDEO,
            &params,
        )?;
        if v["code"].as_i64() != Some(0) {
            bail!(
                "心跳失败 {}: {}",
                v["code"],
                v["message"].as_str().unwrap_or("")
            );
        }
        Ok(())
    }

    /// 直播进房（计入观看，并写直播历史）
    pub fn live_entry(&self, room: i64) -> Result<()> {
        if !self.logged_in() {
            return Ok(());
        }
        let csrf = self
            .csrf()
            .ok_or_else(|| anyhow!("未登录（:login 扫码后才有 bili_jct）"))?;
        let params = vec![
            ("room_id".to_string(), room.to_string()),
            ("platform".to_string(), "web".to_string()),
            ("csrf".to_string(), csrf),
        ];
        let v = self.form_post(
            "https://api.live.bilibili.com/xlive/web-room/v1/index/roomEntryAction",
            &format!("https://live.bilibili.com/{}", room),
            &params,
        )?;
        if v["code"].as_i64() != Some(0) {
            bail!("进房失败 {}: {}", v["code"], v["message"].as_str().unwrap_or(""));
        }
        Ok(())
    }

    /// 发直播弹幕
    pub fn live_send(&self, room: i64, text: &str) -> Result<String> {
        let csrf = self
            .csrf()
            .ok_or_else(|| anyhow!("未登录（:login 扫码后才有 bili_jct）"))?;
        let rnd = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
            .to_string();
        let params = vec![
            ("bubble".to_string(), "0".to_string()),
            ("color".to_string(), "16777215".to_string()),
            ("fontsize".to_string(), "25".to_string()),
            ("mode".to_string(), "1".to_string()),
            ("msg".to_string(), text.to_string()),
            ("rnd".to_string(), rnd),
            ("roomid".to_string(), room.to_string()),
            ("csrf".to_string(), csrf.clone()),
            ("csrf_token".to_string(), csrf),
            ("web_location".to_string(), "444.8".to_string()),
        ];
        let v = self.signed_post(
            "https://api.live.bilibili.com/msg/send",
            &format!("https://live.bilibili.com/{}", room),
            params,
        )?;
        let code = v["code"].as_i64().unwrap_or(-1);
        if code != 0 {
            bail!("发送失败 {}: {}", code, v["message"].as_str().unwrap_or(""));
        }
        Ok("直播弹幕已发送".into())
    }

    /// 发视频弹幕（progress 为秒）
    pub fn video_send(&self, cid: i64, bvid: &str, progress: u64, text: &str) -> Result<String> {
        let csrf = self
            .csrf()
            .ok_or_else(|| anyhow!("未登录（:login 扫码后才有 bili_jct）"))?;
        let params = vec![
            ("type".to_string(), "1".to_string()),
            ("oid".to_string(), cid.to_string()),
            ("msg".to_string(), text.to_string()),
            ("color".to_string(), "16777215".to_string()),
            ("fontsize".to_string(), "25".to_string()),
            ("pool".to_string(), "0".to_string()),
            ("mode".to_string(), "1".to_string()),
            ("progress".to_string(), progress.to_string()),
            ("plat".to_string(), "1".to_string()),
            ("csrf".to_string(), csrf),
            ("bvid".to_string(), bvid.to_string()),
            ("web_location".to_string(), "333.1007".to_string()),
        ];
        let v = self.signed_post(
            "https://api.bilibili.com/x/v2/dm/post",
            &format!("https://www.bilibili.com/video/{}", bvid),
            params,
        )?;
        let code = v["code"].as_i64().unwrap_or(-1);
        if code != 0 {
            let msg = match code {
                36701 => "弹幕包含被禁止的内容".to_string(),
                36702 => "弹幕超过 100 字".to_string(),
                36703 => "该视频禁止发送弹幕".to_string(),
                _ => v["message"].as_str().unwrap_or("").to_string(),
            };
            bail!("发送失败 {}: {}", code, msg);
        }
        Ok("视频弹幕已发送".into())
    }

    /// 直播间高能榜（只读）：返回 (名字, 分数, 名次)，另附自己的名次
    pub fn live_gold_rank(&self, room: i64) -> Result<(Vec<(String, i64, i64)>, Option<(String, i64, i64)>)> {
        let url = format!(
            "https://api.live.bilibili.com/xlive/general-interface/v1/rank/queryContributionRank?room_id={}&ruid={}&page=1&page_size=10",
            room, self.mid
        );
        let v = self.data(&url, &format!("https://live.bilibili.com/{}", room))?;
        let mut list: Vec<(String, i64, i64)> = Vec::new();
        if let Some(items) = v["item"].as_array() {
            for it in items {
                list.push((
                    it["name"].as_str().unwrap_or("?").to_string(),
                    it["score"].as_i64().unwrap_or(0),
                    it["rank"].as_i64().unwrap_or(0),
                ));
            }
        }
        let own = v["own_info"]["name"].as_str().map(|n| {
            (
                n.to_string(),
                v["own_info"]["score"].as_i64().unwrap_or(0),
                v["own_info"]["rank"].as_i64().unwrap_or(-1),
            )
        });
        Ok((list, own))
    }
}


fn strip_html(s: &str) -> String {
    let mut out = String::new();
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out.replace("&amp;", "&")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
}

fn parse_duration_str(s: &str) -> u64 {
    let parts: Vec<u64> = s.split(':').filter_map(|p| p.parse().ok()).collect();
    match parts.len() {
        3 => parts[0] * 3600 + parts[1] * 60 + parts[2],
        2 => parts[0] * 60 + parts[1],
        1 => parts[0],
        _ => 0,
    }
}

fn video_from_popular(v: &Value) -> Track {
    Track {
        bvid: v["bvid"].as_str().map(String::from),
        aid: v["aid"].as_i64().unwrap_or(0),
        cid: None,
        title: v["title"].as_str().unwrap_or("").to_string(),
        author: v["owner"]["name"].as_str().unwrap_or("").to_string(),
        duration: v["duration"].as_u64().unwrap_or(0),
        source: Source::Video,
        room_id: None,
        pages: vec![],
        pubdate: 0,
        up_mid: 0,
    }
}

fn video_from_search(v: &Value) -> Track {
    Track {
        bvid: v["bvid"].as_str().map(String::from),
        aid: v["aid"].as_i64().unwrap_or(0),
        cid: None,
        title: strip_html(v["title"].as_str().unwrap_or("")),
        author: strip_html(v["author"].as_str().unwrap_or("")),
        duration: parse_duration_str(v["duration"].as_str().unwrap_or("0:00")),
        source: Source::Video,
        room_id: None,
        pages: vec![],
        pubdate: 0,
        up_mid: 0,
    }
}

fn live_from_search(v: &Value) -> Track {
    Track {
        bvid: None,
        aid: 0,
        cid: None,
        title: strip_html(v["title"].as_str().unwrap_or("")),
        author: strip_html(v["uname"].as_str().unwrap_or("")),
        duration: 0,
        source: Source::Live,
        room_id: v["roomid"].as_i64(),
        pages: vec![],
        pubdate: 0,
        up_mid: 0,
    }
}

pub fn fmt_time(secs: u64) -> String {
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    if h > 0 {
        format!("{}:{:02}:{:02}", h, m, s)
    } else {
        format!("{:02}:{:02}", m, s)
    }
}

#[derive(Clone, Debug)]
pub struct SeasonRef {
    pub mid: i64,
    pub season_id: i64,
    pub title: String,
    pub count: u64,
}

#[derive(Clone, Debug)]
pub struct SeriesRef {
    pub mid: i64,
    pub series_id: i64,
    pub title: String,
    pub count: u64,
}

impl Api {
    fn csrf(&self) -> Option<String> {
        let c = self.cookie();
        c.split("bili_jct=")
            .nth(1)
            .map(|s| s.split(';').next().unwrap_or("").trim().to_string())
            .filter(|s| !s.is_empty())
    }

    pub fn logged_in(&self) -> bool {
        self.cookie().contains("SESSDATA=")
    }

    fn post_form(&self, url: &str, form: &[(&str, String)], referer: &str) -> Result<Value> {
        let resp = self
            .http
            .post(url)
            .header("Referer", referer)
            .header("Origin", referer.trim_end_matches('/'))
            .header("Cookie", self.cookie())
            .form(form)
            .send()
            .with_context(|| format!("请求失败: {url}"))?;
        let text = resp.text().unwrap_or_default();
        let v: Value = serde_json::from_str(&text)
            .map_err(|_| anyhow!("接口返回非 JSON（可能被风控/未登录）"))?;
        let code = v["code"].as_i64().unwrap_or(-1);
        if code != 0 {
            bail!("bilibili 接口错误 {}: {}", code, v["message"].as_str().unwrap_or("unknown"));
        }
        Ok(v["data"].clone())
    }

    /// 点赞 / 取消点赞
    pub fn like(&self, aid: i64, on: bool) -> Result<()> {
        let csrf = self.csrf().ok_or_else(|| anyhow!("未登录，先登录（:login）"))?;
        self.post_form(
            "https://api.bilibili.com/x/web-interface/archive/like",
            &[
                ("aid", aid.to_string()),
                ("like", if on { "1".into() } else { "2".into() }),
                ("csrf", csrf),
                ("jsonp", "jsonp".into()),
            ],
            REF_VIDEO,
        )?;
        Ok(())
    }

    /// 收藏 / 取消收藏到指定收藏夹
    pub fn fav_deal(&self, aid: i64, add: bool, media_id: i64) -> Result<()> {
        let csrf = self.csrf().ok_or_else(|| anyhow!("未登录，先登录（:login）"))?;
        let mut form: Vec<(&str, String)> = vec![
            ("rid", aid.to_string()),
            ("type", "2".into()),
            ("csrf", csrf),
        ];
        if add {
            form.push(("add_media_ids", media_id.to_string()));
        } else {
            form.push(("del_media_ids", media_id.to_string()));
        }
        self.post_form(
            "https://api.bilibili.com/x/v3/fav/resource/deal",
            &form,
            REF_VIDEO,
        )?;
        Ok(())
    }

    /// UP 主投稿（先试签名接口，失败退回老接口）
    pub fn space_videos(&self, mid: i64, page: u32) -> Result<Vec<Track>> {
        let parse = |d: &Value| -> Vec<Track> {
            d["list"]["vlist"]
                .as_array()
                .cloned()
                .unwrap_or_default()
                .iter()
                .map(|v| Track {
                    bvid: v["bvid"].as_str().map(String::from),
                    aid: v["aid"].as_i64().unwrap_or(0),
                    cid: None,
                    title: strip_html(v["title"].as_str().unwrap_or("")),
                    author: strip_html(v["author"].as_str().unwrap_or("")),
                    duration: parse_duration_str(v["length"].as_str().unwrap_or("0:00")),
                    source: Source::Video,
                    room_id: None,
                    pages: vec![],
                    pubdate: v["created"].as_u64().unwrap_or(0),
                    up_mid: v["mid"].as_i64().unwrap_or(0),
                })
                .collect()
        };

        let params = vec![
            ("mid".to_string(), mid.to_string()),
            ("ps".to_string(), "50".to_string()),
            ("pn".to_string(), page.to_string()),
            ("order".to_string(), "pubdate".to_string()),
            ("platform".to_string(), "web".to_string()),
            ("web_location".to_string(), "1550101".to_string()),
        ];
        if let Ok(q) = self.signed_query(params) {
            let url = format!("https://api.bilibili.com/x/space/wbi/arc/search?{}", q);
            for ck in [self.cookie(), self.cookie_base()] {
                if let Ok(d) = self.data_ck(&url, REF_VIDEO, &ck) {
                    let v = parse(&d);
                    if !v.is_empty() {
                        return Ok(v);
                    }
                }
                std::thread::sleep(std::time::Duration::from_millis(400));
            }
        }
        let old = format!(
            "https://api.bilibili.com/x/space/arc/search?mid={}&pn={}&ps=30&order=pubdate",
            mid, page
        );
        let d = self.data(&old, REF_VIDEO)?;
        Ok(parse(&d))
    }

    /// UP 的合集 + 播放列表（series）
    pub fn space_seasons(&self, mid: i64) -> Result<(Vec<SeasonRef>, Vec<SeriesRef>)> {
        let url = format!(
            "https://api.bilibili.com/x/polymer/web-space/seasons_series_list?mid={}&page_num=1&page_size=20",
            mid
        );
        let d = self.data(&url, REF_VIDEO)?;
        let il = &d["items_lists"];
        let seasons = il["seasons_list"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .iter()
            .map(|x| SeasonRef {
                mid,
                season_id: x["meta"]["season_id"].as_i64().unwrap_or(0),
                title: x["meta"]["name"].as_str().unwrap_or("").to_string(),
                count: x["meta"]["total"].as_u64().unwrap_or(0),
            })
            .collect();
        let series: Vec<SeriesRef> = il["series_list"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .iter()
            .map(|x| SeriesRef {
                mid,
                series_id: x["meta"]["series_id"].as_i64().unwrap_or(0),
                title: x["meta"]["name"].as_str().unwrap_or("").to_string(),
                count: x["meta"]["total"].as_u64().unwrap_or(0),
            })
            .collect();
        let mut series = series;
        if series.is_empty() {
            series = il["page"]
                .as_array()
                .cloned()
                .unwrap_or_default()
                .iter()
                .map(|x| SeriesRef {
                    mid,
                    series_id: x["meta"]["series_id"].as_i64().unwrap_or(0),
                    title: x["meta"]["name"].as_str().unwrap_or("").to_string(),
                    count: x["meta"]["total"].as_u64().unwrap_or(0),
                })
                .collect();
        }
        Ok((seasons, series))
    }

    pub fn season_archives(&self, mid: i64, season_id: i64, page: u32) -> Result<Vec<Track>> {
        let url = format!(
            "https://api.bilibili.com/x/polymer/web-space/seasons_archives_list?mid={}&season_id={}&page_num={}&page_size=100",
            mid, season_id, page
        );
        let d = self.data(&url, REF_VIDEO)?;
        Ok(d["archives"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .iter()
            .map(|a| Track {
                bvid: a["bvid"].as_str().map(String::from),
                aid: a["aid"].as_i64().unwrap_or(0),
                cid: None,
                title: strip_html(a["title"].as_str().unwrap_or("")),
                author: String::new(),
                duration: a["duration"].as_u64().unwrap_or(0),
                source: Source::Video,
                room_id: None,
                pages: vec![],
                pubdate: a["pubdate"].as_u64().unwrap_or(0),
                up_mid: mid,
            })
            .collect())
    }

    /// 播放列表（series）
    pub fn series_archives(&self, mid: i64, series_id: i64, page: u32) -> Result<Vec<Track>> {
        let url = format!(
            "https://api.bilibili.com/x/series/archives?mid={}&series_id={}&pn={}&ps=100",
            mid, series_id, page
        );
        let d = self.data(&url, REF_VIDEO)?;
        Ok(d["archives"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .iter()
            .map(|a| Track {
                bvid: a["bvid"].as_str().map(String::from),
                aid: a["aid"].as_i64().unwrap_or(0),
                cid: None,
                title: strip_html(a["title"].as_str().unwrap_or("")),
                author: String::new(),
                duration: a["duration"].as_u64().unwrap_or(0),
                source: Source::Video,
                room_id: None,
                pages: vec![],
                pubdate: a["pubdate"].as_u64().unwrap_or(0),
                up_mid: mid,
            })
            .collect())
    }

    /// 某个视频所属的合集
    pub fn video_season(&self, bvid: &str) -> Result<Option<(SeasonRef, Vec<Track>)>> {
        let url = format!("https://api.bilibili.com/x/web-interface/view?bvid={}", bvid);
        let d = self.data(&url, REF_VIDEO)?;
        let se = &d["ugc_season"];
        if se.is_null() || se["sections"].is_null() {
            return Ok(None);
        }
        let mid = se["mid"].as_i64().unwrap_or(0);
        let season_id = se["id"].as_i64().unwrap_or(0);
        let mut tracks = Vec::new();
        for sec in se["sections"].as_array().cloned().unwrap_or_default() {
            for ep in sec["episodes"].as_array().cloned().unwrap_or_default() {
                tracks.push(Track {
                    bvid: ep["bvid"].as_str().map(String::from),
                    aid: ep["arc"]["aid"].as_i64().unwrap_or(0),
                    cid: ep["cid"].as_i64(),
                    title: strip_html(ep["title"].as_str().unwrap_or("")),
                    author: String::new(),
                    duration: ep["arc"]["duration"].as_u64().unwrap_or(0),
                    source: Source::Video,
                    room_id: None,
                    pages: vec![],
                    pubdate: ep["arc"]["pubdate"].as_u64().unwrap_or(0),
                    up_mid: ep["arc"]["author"]["mid"].as_i64().unwrap_or(0),
                });
            }
        }
        Ok(Some((
            SeasonRef {
                mid,
                season_id,
                title: se["title"].as_str().unwrap_or("合集").to_string(),
                count: tracks.len() as u64,
            },
            tracks,
        )))
    }

    /// 按关键词找 UP 主
    /// 当前登录用户昵称与 mid
    pub fn nav_info(&self) -> Result<(String, i64)> {
        let d = self.data("https://api.bilibili.com/x/web-interface/nav", REF_VIDEO)?;
        if d["isLogin"].as_bool() != Some(true) {
            return Err(anyhow!("未登录"));
        }
        Ok((
            d["uname"].as_str().unwrap_or("").to_string(),
            d["mid"].as_i64().unwrap_or(0),
        ))
    }

    pub fn search_users(&self, kw: &str) -> Result<Vec<(i64, String)>> {
        let params = vec![
            ("search_type".to_string(), "bili_user".to_string()),
            ("keyword".to_string(), kw.to_string()),
            ("page".to_string(), "1".to_string()),
        ];
        let q = self.signed_query(params)?;
        let url = format!("https://api.bilibili.com/x/web-interface/wbi/search/type?{}", q);
        let mut last = None;
        for ck in [self.cookie_base(), self.cookie()] {
            match self.data_ck(&url, REF_VIDEO, &ck) {
                Ok(d) => {
                    return Ok(d["result"]
                        .as_array()
                        .cloned()
                        .unwrap_or_default()
                        .iter()
                        .map(|u| {
                            (
                                u["mid"].as_i64().unwrap_or(0),
                                strip_html(u["uname"].as_str().unwrap_or("")),
                            )
                        })
                        .collect())
                }
                Err(e) => last = Some(e),
            }
        }
        Err(last.unwrap_or_else(|| anyhow!("搜索 UP 主失败")))
    }
}

impl Api {
    /// 视频详情：含分P列表、发布时间、aid
    pub fn video_detail(&self, bvid: &str) -> Result<Track> {
        if let Some(t) = self.detail_cache.lock().unwrap().get(bvid) {
            return Ok(t.clone());
        }
        let d = self.data(
            &format!("https://api.bilibili.com/x/web-interface/view?bvid={}", bvid),
            REF_VIDEO,
        )?;
        let pages: Vec<Page> = d["pages"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .iter()
            .map(|p| Page {
                page: p["page"].as_i64().unwrap_or(1),
                cid: p["cid"].as_i64().unwrap_or(0),
                part: p["part"].as_str().unwrap_or("").to_string(),
                duration: p["duration"].as_u64().unwrap_or(0),
            })
            .collect();
        let t = Track {
            bvid: Some(d["bvid"].as_str().unwrap_or(bvid).to_string()),
            aid: d["aid"].as_i64().unwrap_or(0),
            cid: Some(d["cid"].as_i64().unwrap_or(0)),
            title: d["title"].as_str().unwrap_or("").to_string(),
            author: d["owner"]["name"].as_str().unwrap_or("").to_string(),
            duration: d["duration"].as_u64().unwrap_or(0),
            source: Source::Video,
            room_id: None,
            pages,
            pubdate: d["pubdate"].as_u64().unwrap_or(0),
            up_mid: d["owner"]["mid"].as_i64().unwrap_or(0),
        };
        self.detail_cache
            .lock()
            .unwrap()
            .insert(bvid.to_string(), t.clone());
        Ok(t)
    }

    /// 关注动态里的视频更新
    pub fn dynamic_videos(&self, page: u32) -> Result<Vec<Track>> {
        let url = format!(
            "https://api.bilibili.com/x/polymer/web-dynamic/v1/feed/all?type=video&page={}&offset=",
            page
        );
        let d = self.data(&url, REF_VIDEO)?;
        Ok(d["items"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .iter()
            .filter_map(|it| {
                let major = &it["modules"]["module_dynamic"]["major"];
                let arc = if major["archive"].is_object() {
                    &major["archive"]
                } else {
                    return None;
                };
                let bvid = arc["bvid"].as_str()?.to_string();
                Some(Track {
                    bvid: Some(bvid.clone()),
                    aid: 0,
                    cid: None,
                    title: strip_html(arc["title"].as_str().unwrap_or("")),
                    author: strip_html(it["modules"]["module_author"]["name"].as_str().unwrap_or("")),
                    duration: parse_duration_str(arc["duration_text"].as_str().unwrap_or("0:00")),
                    source: Source::Video,
                    room_id: None,
                    pages: vec![],
                    pubdate: it["modules"]["module_author"]["pub_ts"].as_u64().unwrap_or(0),
                    up_mid: it["modules"]["module_author"]["mid"].as_i64().unwrap_or(0),
                })
            })
            .collect())
    }

    /// 关注动态里正在直播的 UP
    /// 关注 UP 中正在直播的直播间。
    /// 三路来源依次尝试：直播关注 FeedList / 关注动态 feed_list / 动态卡片 LIVE_RCMD
    #[allow(dead_code)]
    pub fn dynamic_lives(&self) -> Result<Vec<Track>> {
        Ok(self.dynamic_lives_dbg()?.0)
    }

    /// 返回 (直播间列表, 每路来源的诊断日志)
    pub fn dynamic_lives_dbg(&self) -> Result<(Vec<Track>, Vec<String>)> {
        let mut rooms: Vec<LiveRoom> = Vec::new();
        let mut seen: std::collections::HashSet<i64> = std::collections::HashSet::new();
        let mut log = Vec::new();

        // 1) 直播关注列表：page_size=10 是 web 端稳定可用的取值（>10 可能静默返回空）
        for (variant, size, extra) in [(0u8, 10u32, ""), (1u8, 10u32, "&platform=web")] {
            if !rooms.is_empty() {
                break;
            }
            for page in 1..=10u32 {
                let url = format!(
                    "https://api.live.bilibili.com/xlive/web-ucenter/v1/xfetter/FeedList?page={}&page_size={}{}",
                    page, size, extra
                );
                match self.data(&url, REF_LIVE) {
                    Ok(d) => {
                        let before = seen.len();
                        let mut one = Vec::new();
                        walk_live(&d, &mut one);
                        for r in one {
                            if seen.insert(r.room_id) {
                                rooms.push(r);
                            }
                        }
                        log.push(format!(
                            "FeedList{}{} p{}: +{}",
                            if variant == 1 { "(platform=web)" } else { "" },
                            size,
                            page,
                            seen.len() - before
                        ));
                        if seen.len() == before {
                            break;
                        }
                    }
                    Err(e) => {
                        log.push(format!("FeedList p{} 失败: {}", page, e));
                        break;
                    }
                }
            }
        }
        // 2) 关注动态里的直播列表（老接口，结构 data.list[]）
        if rooms.is_empty() {
            for page in 1..=3u32 {
                let url = format!(
                    "https://api.live.bilibili.com/relation/v1/feed/feed_list?page={}&page_size=20",
                    page
                );
                match self.data(&url, REF_LIVE) {
                    Ok(d) => {
                        let before = seen.len();
                        let mut one = Vec::new();
                        walk_live(&d, &mut one);
                        for r in one {
                            if seen.insert(r.room_id) {
                                rooms.push(r);
                            }
                        }
                        log.push(format!("feed_list p{}: +{}", page, seen.len() - before));
                        if seen.len() == before {
                            break;
                        }
                    }
                    Err(e) => {
                        log.push(format!("feed_list p{} 失败: {}", page, e));
                        break;
                    }
                }
            }
        }

        // 3) 动态卡片里的直播推荐（只返回部分，但可作为兜底）
        let url = "https://api.bilibili.com/x/polymer/web-dynamic/v1/feed/all?type=all&page=1";
        match self.data(url, REF_VIDEO) {
            Ok(d) => {
                let before = seen.len();
                for it in d["items"].as_array().cloned().unwrap_or_default() {
                    let ty = it["type"].as_str().unwrap_or("");
                    let major = &it["modules"]["module_dynamic"]["major"];
                    let mty = major["type"].as_str().unwrap_or("");
                    if !ty.contains("LIVE") && !mty.contains("LIVE") {
                        continue;
                    }
                    let author =
                        strip_html(it["modules"]["module_author"]["name"].as_str().unwrap_or(""));
                    if let Some(content) = major["live_rcmd"]["content"].as_str() {
                        if let Ok(v) = serde_json::from_str::<Value>(content) {
                            let mut one = Vec::new();
                            collect_live(&v, &author, &mut one);
                            for r in one {
                                if seen.insert(r.room_id) {
                                    rooms.push(r);
                                }
                            }
                        }
                    }
                    let mut one = Vec::new();
                    collect_live(major, &author, &mut one);
                    for r in one {
                        if seen.insert(r.room_id) {
                            rooms.push(r);
                        }
                    }
                }
                log.push(format!("动态卡片: +{}", seen.len() - before));
            }
            Err(e) => log.push(format!("动态卡片失败: {}", e)),
        }

        let mut out = Vec::new();
        for r in rooms {
            if r.room_id <= 0 {
                continue;
            }
            out.push(Track {
                bvid: None,
                aid: 0,
                cid: None,
                title: if r.title.is_empty() {
                    "直播中".to_string()
                } else {
                    r.title.chars().take(80).collect()
                },
                author: r.uname,
                duration: 0,
                source: Source::Live,
                room_id: Some(r.room_id),
                pages: vec![],
                pubdate: 0,
                up_mid: r.uid,
            });
        }
        Ok((out, log))
    }
}

struct LiveRoom {
    room_id: i64,
    uid: i64,
    uname: String,
    title: String,
}

/// 从任意 JSON 子树里收集直播间节点（room_id + uname + title）
fn walk_live(v: &Value, out: &mut Vec<LiveRoom>) {
    collect_live(v, "", out);
    if let Value::Object(m) = v {
        for (_, x) in m {
            if x.is_object() || x.is_array() {
                walk_live(x, out);
            }
        }
    } else if let Value::Array(a) = v {
        for x in a {
            walk_live(x, out);
        }
    }
}

fn collect_live(v: &Value, author_hint: &str, out: &mut Vec<LiveRoom>) {
    let room = v.get("room_id").and_then(|x| x.as_i64())
        .or_else(|| v.get("roomid").and_then(|x| x.as_i64()))
        .or_else(|| v.get("roomId").and_then(|x| x.as_i64()));
    let room = match room {
        Some(r) if r > 0 => r,
        _ => return,
    };
    let uid = v.get("uid").and_then(|x| x.as_i64())
        .or_else(|| v.get("mid").and_then(|x| x.as_i64()))
        .unwrap_or(0);
    let uname = v.get("uname").and_then(|x| x.as_str())
        .or_else(|| v.get("name").and_then(|x| x.as_str()))
        .filter(|s| !s.is_empty())
        .map(|s| strip_html(s))
        .unwrap_or_else(|| author_hint.to_string());
    let title = v.get("title").and_then(|x| x.as_str())
        .map(|s| strip_html(s))
        .unwrap_or_default();
    out.push(LiveRoom { room_id: room, uid, uname, title });
}

#[allow(dead_code)]
fn find_i64(v: &Value, key: &str) -> Option<i64> {
    match v {
        Value::Object(m) => {
            if let Some(x) = m.get(key) {
                if let Some(n) = x.as_i64() {
                    return Some(n);
                }
                if let Some(s) = x.as_str() {
                    if let Ok(n) = s.parse::<i64>() {
                        return Some(n);
                    }
                }
            }
            for (_, val) in m {
                if let Some(n) = find_i64(val, key) {
                    return Some(n);
                }
            }
            None
        }
        Value::Array(a) => a.iter().find_map(|x| find_i64(x, key)),
        _ => None,
    }
}
