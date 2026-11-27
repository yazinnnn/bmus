//! cmus 风格 TUI：视图 + 队列 + 浏览导航（收藏夹 / UP 主 / 合集 / 播放列表）

use anyhow::Result;
use image::GenericImageView;
use ratatui_image::picker::{Picker, ProtocolType};
use ratatui_image::protocol::StatefulProtocol;
use ratatui_image::StatefulImage;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::event::{DisableMouseCapture, EnableMouseCapture};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Gauge, Paragraph, Wrap};
use ratatui::{Frame, Terminal};
use std::collections::VecDeque;
use std::io::Stdout;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use unicode_width::UnicodeWidthChar;

use crate::api::{fmt_time, Api, Comment, DynItem, Source, Track};
use crate::danmaku::{DmEvent, DmKind, DmLine};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Repeat {
    Off,
    One,
    All,
    Shuffle,
}

impl Repeat {
    fn next(self) -> Self {
        match self {
            Repeat::Off => Repeat::One,
            Repeat::One => Repeat::All,
            Repeat::All => Repeat::Shuffle,
            Repeat::Shuffle => Repeat::Off,
        }
    }
    fn label(self) -> &'static str {
        match self {
            Repeat::Off => "关闭",
            Repeat::One => "单曲循环",
            Repeat::All => "列表循环",
            Repeat::Shuffle => "列表随机",
        }
    }
}

/// 发送目标：视频（cid + 当前进度秒）或直播间
#[derive(Clone, Debug)]
pub enum SayTarget {
    Video { cid: i64, bvid: String, progress: u64 },
    Live { room: i64 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ImageMode {
    Off,
    Native,
    Chars,
}

/// 已解码并缩放好的缩略图：rgb 每像素 3 字节，h 为偶数（两行像素合一格）
#[derive(Clone)]
pub(crate) struct Thumb {
    w: u32,
    h: u32,
    rgb: Vec<u8>,
}

#[derive(Clone)]
enum LikeTarget {
    Video(i64),
    Dyn(String),
    Comment { rpid: i64, ctx: CommentCtx },
}

/// 后台任务：网络请求全部放到 worker 线程，避免卡住 UI
pub enum Job {
    Cfg(crate::config::Config),
    Search { kind: String, kw: String, page: u32, target: usize },
    More { kind: ViewKind, page: u32, kw: String, sid: i64, skind: u8, smid: i64 },
    Play { track: Track, low: bool, live_low: bool },
    #[allow(dead_code)]
    Dynamic,
    /// 直播进房（计入观看 + 直播历史）
    LiveEntry { room: i64 },
    /// 视频播放心跳（计入播放 + 历史）
    Heartbeat { aid: i64, cid: i64, pos: u64 },
    /// 发送弹幕（:say 确认后）
    Say { text: String, target: SayTarget },
    /// 直播间高能榜（只读）
    Rank { room: i64 },
    /// 动态全类型 feed
    DynamicAll { offset: String },
    /// 评论列表（root=Some 表示拉某条评论的回复）
    Comments { ctype: i32, oid: String, page: u32, ctx: CommentCtx, root: Option<i64> },
    /// 发评论（确认后）
    CommentAdd { text: String, ctx: CommentCtx, root: i64, parent: i64, label: String },
    /// 点赞评论（确认后）
    CommentLike { rpid: i64, on: bool, ctx: CommentCtx, label: String },
    /// 点赞动态（确认后）
    DynLike { id: String, on: bool, label: String },
    /// 拉取并解码动态图片缩略图
    DynImages { key: String, urls: Vec<String> },
    /// 把当前动态的图片保存到缓存并用系统看图工具打开
    OpenImages { urls: Vec<String> },
}

pub enum JobOut {
    Fatal(String),
    Search { target: usize, kw: String, items: Vec<Track>, err: Option<String> },
    More { page: u32, items: Vec<Track>, err: Option<String> },
    Play { track: Track, urls: Vec<String>, label: String, err: Option<String> },
    #[allow(dead_code)]
    Dynamic { lives: Vec<Track>, videos: Vec<Track>, err: Option<String>, log: Vec<String> },
    /// 通用提示（发送结果 / 进房失败等）
    Note(String),
    /// 弹幕发送成功：立即本地回显，不必等接口缓存刷新
    DmSent { text: String, at: f64, label: String },
    DynamicAll { items: Vec<DynItem>, lives: Vec<Track>, next_offset: String, has_more: bool, err: Option<String> },
    Comments { items: Vec<Comment>, has_more: bool, ctx: CommentCtx, root: Option<i64>, err: Option<String> },
    DynImages { key: String, images: Vec<Vec<u8>>, err: Option<String> },
    Rank { list: Vec<(String, i64, i64)>, own: Option<(String, i64, i64)> },
}

fn worker(cfg: crate::config::Config, rx: std::sync::mpsc::Receiver<Job>, tx: std::sync::mpsc::Sender<JobOut>) {
    let mut api = match Api::new(&cfg) {
        Ok(a) => a,
        Err(e) => {
            let _ = tx.send(JobOut::Fatal(format!("初始化 API 失败: {}", e)));
            return;
        }
    };
    while let Ok(job) = rx.recv() {
        match job {
            Job::Dynamic => {
                let (lives, log) = api.dynamic_lives_dbg().unwrap_or((vec![], vec![]));
                let out = match api.dynamic_videos(1) {
                    Ok(v) => JobOut::Dynamic { lives, videos: v, err: None, log },
                    Err(e) => JobOut::Dynamic {
                        lives,
                        videos: vec![],
                        err: Some(format!("视频动态加载失败: {}", e)),
                        log,
                    },
                };
                let _ = tx.send(out);
            }
            Job::Cfg(c) => {
                if let Ok(a) = Api::new(&c) {
                    api = a;
                }
            }
            Job::Search { kind, kw, page, target } => {
                let r = api.search(&kw, &kind, page);
                let out = match r {
                    Ok(items) => JobOut::Search { target, kw: kw.clone(), items, err: None },
                    Err(e) => JobOut::Search { target, kw: kw.clone(), items: vec![], err: Some(format!("{}", e)) },
                };
                let _ = tx.send(out);
            }
            Job::More { kind, page, kw, sid, skind, smid } => {
                let r: anyhow::Result<Vec<Track>> = (|| {
                    Ok(match kind {
                        ViewKind::Search => api.search(&kw, "video", page)?,
                        ViewKind::Live => api.search(&kw, "live_room", page)?,
                        ViewKind::Fav => {
                            if sid <= 0 {
                                anyhow::bail!("先进入某个收藏夹再翻页");
                            }
                            api.fav_items(sid, page)?
                        }
                        ViewKind::Space => match skind {
                            1 => api.season_archives(smid, sid, page)?,
                            2 => api.series_archives(smid, sid, page)?,
                            _ => {
                                if sid <= 0 {
                                    anyhow::bail!("先进入某个 UP 主页再翻页");
                                }
                                api.space_videos(sid, page)?
                            }
                        },
                        ViewKind::Dynamic => api.dynamic_videos(page)?,
                        ViewKind::Popular | ViewKind::Queue | ViewKind::Danmaku | ViewKind::Comment => anyhow::bail!("该视图不支持翻页"),
                    })
                })();
                let mut r = r;
                if let Err(e) = &r {
                    let m = format!("{}", e);
                    if m.contains("-799") || m.contains("412") || m.contains("限流") {
                        std::thread::sleep(std::time::Duration::from_secs(25));
                        r = (|| {
                            Ok(match kind {
                                ViewKind::Search => api.search(&kw, "video", page)?,
                                ViewKind::Live => api.search(&kw, "live_room", page)?,
                                ViewKind::Fav => api.fav_items(sid, page)?,
                                ViewKind::Space => match skind {
                                    1 => api.season_archives(smid, sid, page)?,
                                    2 => api.series_archives(smid, sid, page)?,
                                    _ => api.space_videos(sid, page)?,
                                },
                                ViewKind::Dynamic => api.dynamic_videos(page)?,
                                _ => anyhow::bail!("该视图不支持翻页"),
                            })
                        })();
                    }
                }
                let out = match r {
                    Ok(items) => JobOut::More { page, items, err: None },
                    Err(e) => JobOut::More { page, items: vec![], err: Some(format!("{}", e)) },
                };
                let _ = tx.send(out);
            }
            Job::Play { mut track, low, live_low } => {
                if track.pages.is_empty() {
                    if let Some(b) = track.bvid.clone() {
                        if let Ok(d) = api.video_detail(&b) {
                            track.pages = d.pages;
                            if track.pubdate == 0 {
                                track.pubdate = d.pubdate;
                            }
                            if track.aid == 0 {
                                track.aid = d.aid;
                            }
                            if track.cid.is_none() {
                                track.cid = track.pages.first().map(|p| p.cid);
                            }
                            if track.author.is_empty() {
                                track.author = d.author;
                            }
                            if track.up_mid == 0 {
                                track.up_mid = d.up_mid;
                            }
                        }
                    }
                }
                let r = match track.source {
                    Source::Video => api.video_audio_quality(&track, low),
                    Source::Live => {
                        let room = track.room_id.unwrap_or(0);
                        match api.live_info(room) {
                            Ok((_, _, true)) => api.live_stream(room, live_low),
                            Ok((title, _, false)) => Err(anyhow::anyhow!("未开播: {}", title)),
                            Err(e) => Err(e),
                        }
                    }
                };
                let mut r = r;
                if let Err(e) = &r {
                    let m = format!("{}", e);
                    if m.contains("-799") || m.contains("412") || m.contains("限流") {
                        std::thread::sleep(std::time::Duration::from_secs(20));
                        r = match track.source {
                            Source::Video => api.video_audio_quality(&track, low),
                            Source::Live => api.live_stream(track.room_id.unwrap_or(0), live_low),
                        };
                    }
                }
                let out = match r {
                    Ok(s) => JobOut::Play { track, urls: s.urls, label: s.label, err: None },
                    Err(e) => JobOut::Play { track, urls: vec![], label: String::new(), err: Some(format!("{}", e)) },
                };
                let _ = tx.send(out);
            }
            Job::LiveEntry { room } => {
                if let Err(e) = api.live_entry(room) {
                    let _ = tx.send(JobOut::Note(format!("进房上报: {}", e)));
                }
            }
            Job::Heartbeat { aid, cid, pos } => {
                // 失败静默：心跳不是关键路径，别刷屏
                let _ = api.video_heartbeat(aid, cid, pos);
            }
            Job::Say { text, target } => {
                match target {
                    SayTarget::Video { cid, bvid, progress } => match api.video_send(cid, &bvid, progress, &text) {
                        Ok(m) => {
                            let _ = tx.send(JobOut::DmSent { text, at: progress as f64, label: m });
                        }
                        Err(e) => { let _ = tx.send(JobOut::Note(format!("{}", e))); }
                    },
                    SayTarget::Live { room } => {
                        let r = api.live_send(room, &text);
                        let _ = tx.send(JobOut::Note(match r {
                            Ok(m) => m,
                            Err(e) => format!("{}", e),
                        }));
                    }
                }
            }
            Job::Rank { room } => {
                let out = match api.live_gold_rank(room) {
                    Ok((list, own)) => JobOut::Rank { list, own },
                    Err(e) => JobOut::Note(format!("高能榜: {}", e)),
                };
                let _ = tx.send(out);
            }
            Job::DynamicAll { offset } => {
                let first = offset.is_empty();
                let (r, lives) = match api.dynamic_items(&offset) {
                    Ok(r) => (Ok(r), if first { api.dynamic_lives_dbg().unwrap_or_default().0 } else { vec![] }),
                    Err(e) => (Err(e), vec![]),
                };
                let out = match r {
                    Ok((items, next_offset, has_more)) => JobOut::DynamicAll {
                        items, lives, next_offset, has_more, err: None,
                    },
                    Err(e) => JobOut::DynamicAll {
                        items: vec![], lives, next_offset: offset, has_more: false, err: Some(format!("{}", e)),
                    },
                };
                let _ = tx.send(out);
            }
            Job::Comments { ctype, oid, page, ctx, root } => {
                let out = match root {
                    Some(r) => match api.comment_replies(ctype, &oid, r, page) {
                        Ok(items) => JobOut::Comments { items, has_more: false, ctx, root, err: None },
                        Err(e) => JobOut::Comments { items: vec![], has_more: false, ctx, root, err: Some(format!("{}", e)) },
                    },
                    None => match api.comments(ctype, &oid, page) {
                        Ok((items, has_more)) => JobOut::Comments { items, has_more, ctx, root, err: None },
                        Err(e) => JobOut::Comments { items: vec![], has_more: false, ctx, root, err: Some(format!("{}", e)) },
                    },
                };
                let _ = tx.send(out);
            }
            Job::CommentAdd { text, ctx, root, parent, label } => {
                let r = api.comment_add(ctx.ctype, &ctx.oid, &text, root, parent);
                let _ = tx.send(JobOut::Note(match r {
                    Ok(m) => format!("{} → {}", label, m),
                    Err(e) => format!("{}", e),
                }));
            }
            Job::CommentLike { rpid, on, ctx, label } => {
                let r = api.comment_like(ctx.ctype, &ctx.oid, rpid, on);
                let _ = tx.send(JobOut::Note(match r {
                    Ok(()) => if on { format!("已点赞评论 · {}", label) } else { format!("已取消点赞 · {}", label) },
                    Err(e) => format!("{}", e),
                }));
            }
            Job::DynLike { id, on, label } => {
                let r = api.dynamic_like(&id, on);
                let _ = tx.send(JobOut::Note(match r {
                    Ok(()) => if on { format!("已点赞动态 · {}", label) } else { format!("已取消点赞 · {}", label) },
                    Err(e) => format!("{}", e),
                }));
            }
            Job::DynImages { key, urls } => {
                let mut images = Vec::new();
                let mut err = None;
                for url in urls.iter().take(4) {
                    match api.fetch_image(url) {
                        Ok(bytes) => images.push(bytes),
                        Err(e) => { err = Some(format!("{}", e)); break; }
                    }
                }
                let _ = tx.send(JobOut::DynImages { key, images, err });
            }
            Job::OpenImages { urls } => {
                let out = match open_images_external(&api, &urls) {
                    Ok(s) => s,
                    Err(e) => format!("打开图片失败: {}", e),
                };
                let _ = tx.send(JobOut::Note(out));
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewKind {
    Search,
    Popular,
    Fav,
    Live,
    Queue,
    Space,
    Dynamic,
    Danmaku,
    Comment,
}

#[derive(Clone, Debug)]
pub enum Entry {
    Track(Track),
    Folder { id: i64, title: String, count: u64 },
    Season { mid: i64, season_id: i64, title: String, count: u64 },
    Series { mid: i64, series_id: i64, title: String, count: u64 },
    Up { mid: i64, name: String },
    Dyn(DynItem),
    Comment(Comment),
}

impl Entry {
    fn title(&self) -> String {
        match self {
            Entry::Track(t) => t.title.clone(),
            Entry::Folder { title, .. } => title.clone(),
            Entry::Season { title, .. } => title.clone(),
            Entry::Series { title, .. } => format!("[播放列表] {}", title),
            Entry::Up { name, .. } => name.clone(),
            Entry::Dyn(d) => {
                let head = if d.text.is_empty() { d.title.clone() } else { d.text.clone() };
                format!("[{}] {} · {}", d.kind, d.author, head)
            }
            Entry::Comment(c) => format!("{}：{}", c.user, c.text),
        }
    }

    fn icon(&self) -> &'static str {
        match self {
            Entry::Track(t) if t.source == Source::Live => "◉",
            Entry::Track(_) => "♪",
            Entry::Folder { .. } => "▤",
            Entry::Season { .. } => "▣",
            Entry::Series { .. } => "☰",
            Entry::Up { .. } => "◍",
            Entry::Dyn(d) => match d.kind.as_str() {
                "视频" => "♪",
                "图文" => "▣",
                "文章" => "▤",
                "直播" => "◉",
                "转发" => "↪",
                _ => "◍",
            },
            Entry::Comment(_) => "•",
        }
    }

    fn sub(&self) -> String {
        match self {
            Entry::Track(t) => t.author.clone(),
            Entry::Folder { count, .. } => format!("{} 个视频", count),
            Entry::Season { count, .. } => format!("合集 {} 集", count),
            Entry::Series { count, .. } => format!("播放列表 {} 集", count),
            Entry::Up { mid, .. } => format!("mid {}", mid),
            Entry::Dyn(d) => {
                let mut s = format!(
                    "{} · {}{} · 💬{}",
                    d.time,
                    if d.liked { "已赞♥" } else { "♥" },
                    d.like,
                    d.comment
                );
                if d.mid > 0 {
                    s.push_str(&format!(" · uid {}", d.mid));
                }
                s
            }
            Entry::Comment(c) => {
                let tag = if c.liked { "已赞" } else { "赞" };
                let more = if c.replies.is_empty() {
                    String::new()
                } else {
                    format!(" · ↳{}", c.replies.len())
                };
                format!("{} · {}{} · {} · #{}{}", c.time, tag, c.like, c.floor, c.rpid, more)
            }
        }
    }

    fn duration(&self) -> u64 {
        match self {
            Entry::Track(t) => t.duration,
            _ => 0,
        }
    }

    fn as_track(&self) -> Option<&Track> {
        match self {
            Entry::Track(t) => Some(t),
            _ => None,
        }
    }

    fn color(&self) -> Color {
        match self {
            Entry::Track(t) if t.source == Source::Live => Color::Magenta,
            Entry::Track(_) => Color::White,
            Entry::Folder { .. } => Color::Yellow,
            Entry::Season { .. } => Color::Cyan,
            Entry::Series { .. } => Color::Green,
            Entry::Up { .. } => Color::LightBlue,
            Entry::Dyn(d) => match d.kind.as_str() {
                "视频" => Color::White,
                "图文" => Color::Yellow,
                "文章" => Color::LightBlue,
                "直播" => Color::Magenta,
                _ => Color::DarkGray,
            },
            Entry::Comment(c) => if c.liked { Color::LightRed } else { Color::Gray },
        }
    }
}

pub struct View {
    pub name: &'static str,
    pub kind: ViewKind,
    pub items: Vec<Entry>,
    pub sel: usize,
    pub offset: usize,
    pub hint: String,
    pub keyword: String,
    pub stack: Vec<(String, Vec<Entry>)>,
    /// 当前分页（从 1 开始）
    pub page: u32,
    /// 分页所属对象（收藏夹 id / UP mid / 合集 id / 播放列表 id）
    pub source_id: i64,
    /// 0 无 1 合集 2 播放列表
    pub source_kind: u8,
    /// 合集/播放列表所属 UP
    pub source_mid: i64,
    /// 正在加载的页码（避免连按 > 重复请求同一页）
    pub pending_page: u32,
    pub has_more: bool,
}

/// 评论区目标：视频(aid) / 图片动态(相簿) / 文章 / 动态
#[derive(Debug, Clone)]
pub(crate) struct CommentCtx {
    ctype: i32,
    oid: String,
    label: String,
}

pub struct App {
    pub cfg: crate::config::Config,
    pub api: Api,
    pub player: crate::player::Player,
    pub views: Vec<View>,
    pub view_idx: usize,
    pub queue: VecDeque<Track>,
    pub current: Option<Track>,
    pub status: String,
    pub input: Option<(String, String)>,
    pub help: bool,
    pub should_quit: bool,
    last_eof: bool,
    pub volume: u32,
    pub low: bool,
    pub live_low: bool,
    urls: Vec<String>,
    url_idx: usize,
    play_at: Option<std::time::Instant>,
    stream_label: String,
    want_login: bool,
    fav_id: Option<i64>,
    /// 当前播放的是第几个分P（0-based）
    cur_page: Option<usize>,
    dl_rx: Option<std::sync::mpsc::Receiver<String>>,
    repeat: Repeat,
    job_tx: std::sync::mpsc::Sender<Job>,
    job_rx: std::sync::mpsc::Receiver<JobOut>,
    /// 弹幕：当前轨道收到的行（视频按进度过滤显示，直播实时）
    dm: Vec<DmLine>,
    dm_sel: usize,
    dm_offset: usize,
    dm_rx: Option<std::sync::mpsc::Receiver<DmEvent>>,
    dm_stop: Option<Arc<AtomicBool>>,
    dm_note: String,
    /// 待确认的发送：(正文, 目标描述, 目标)
    say_pending: Option<(String, String, SayTarget)>,
    last_say: Option<std::time::Instant>,
    say_window: Option<(std::time::Instant, u32)>,
    /// 播放心跳状态
    report: Option<Report>,
    /// 弹幕视图是否自动跟屏
    dm_follow: bool,
    /// “自动跟屏”按钮的点击区域
    follow_btn: Rect,
    /// 卡顿检测：上次观察到的播放位置与时间
    stall_mark: Option<(std::time::Instant, f64)>,
    /// 上次自动重连时间（冷却，避免疯狂重试）
    recover_at: Option<std::time::Instant>,
    /// 本次 Play 是卡顿恢复触发的，不要再重建弹幕线程
    recovering: bool,
    /// 评论区当前目标与分页
    cmt_ctx: Option<CommentCtx>,
    cmt_root: Option<i64>,
    cmt_parent: Option<i64>,
    cmt_page: u32,
    cmt_has_more: bool,
    /// 待确认的评论：(正文, 目标描述)
    cmt_pending: Option<(String, String)>,
    /// 待确认的点赞：(目标描述, 目标)
    like_pending: Option<(String, LikeTarget)>,
    /// 动态翻页游标
    dyn_offset: String,
    /// 首页拉到的关注直播间数量
    dyn_live_count: usize,
    /// 评论页顶部展示的动态详情（从动态进入评论时）
    cmt_dyn: Option<DynItem>,
    /// 图片渲染模式（i 键循环：关/原生/字符）
    image_mode: ImageMode,
    /// 终端协议选择器
    picker: Picker,
    /// 原生协议的图片渲染状态（多图）
    img_protocols: Vec<Box<dyn StatefulProtocol>>,
    /// 当前动态的缩略图（字符回退用，多图）
    cmt_thumbs: Vec<Thumb>,
}

struct Report {
    aid: i64,
    cid: i64,
    live: bool,
    last_pos: f64,
    last_sent: std::time::Instant,
}

impl App {
    pub fn new(cfg: crate::config::Config) -> Result<Self> {
        let api = Api::new(&cfg)?;
        let player = crate::player::Player::new(&cfg)?;
        let low = cfg.audio_quality == "lowest";
        let live_low = cfg.live_low;
        let volume = cfg.volume;
        let image_preview = cfg.image_preview;

        let (job_tx, job_rx_jobs) = std::sync::mpsc::channel::<Job>();
        let (out_tx, job_rx) = std::sync::mpsc::channel::<JobOut>();
        {
            let cfg0 = cfg.clone();
            std::thread::spawn(move || worker(cfg0, job_rx_jobs, out_tx));
        }
        let mut app = Self {
            cfg,
            api,
            player,
            views: vec![
                View { name: "搜索", kind: ViewKind::Search, items: vec![], sel: 0, offset: 0, hint: "按 / 输入关键词".into(), keyword: String::new(), stack: vec![], page: 1, source_id: 0, source_kind: 0, source_mid: 0, pending_page: 0, has_more: true },
                View { name: "热门", kind: ViewKind::Popular, items: vec![], sel: 0, offset: 0, hint: String::new(), keyword: String::new(), stack: vec![], page: 1, source_id: 0, source_kind: 0, source_mid: 0, pending_page: 0, has_more: true },
                View { name: "收藏", kind: ViewKind::Fav, items: vec![], sel: 0, offset: 0, hint: String::new(), keyword: String::new(), stack: vec![], page: 1, source_id: 0, source_kind: 0, source_mid: 0, pending_page: 0, has_more: true },
                View { name: "直播", kind: ViewKind::Live, items: vec![], sel: 0, offset: 0, hint: "按 / 搜索直播间".into(), keyword: String::new(), stack: vec![], page: 1, source_id: 0, source_kind: 0, source_mid: 0, pending_page: 0, has_more: true },
                View { name: "队列", kind: ViewKind::Queue, items: vec![], sel: 0, offset: 0, hint: "a 添加，d 删除，Enter 播放".into(), keyword: String::new(), stack: vec![], page: 1, source_id: 0, source_kind: 0, source_mid: 0, pending_page: 0, has_more: true },
                View { name: "UP/合集", kind: ViewKind::Space, items: vec![], sel: 0, offset: 0, hint: ":up <mid|关键词> / :seasons <mid> / :season <BV号>".into(), keyword: String::new(), stack: vec![], page: 1, source_id: 0, source_kind: 0, source_mid: 0, pending_page: 0, has_more: true },
                View { name: "动态", kind: ViewKind::Dynamic, items: vec![], sel: 0, offset: 0, hint: "关注 UP 的更新与直播（需登录）".into(), keyword: String::new(), stack: vec![], page: 1, source_id: 0, source_kind: 0, source_mid: 0, pending_page: 0, has_more: true },
                View { name: "弹幕", kind: ViewKind::Danmaku, items: vec![], sel: 0, offset: 0, hint: "按 8 查看；:say <文本> 给当前视频/直播发弹幕；:rank 看高能榜".into(), keyword: String::new(), stack: vec![], page: 1, source_id: 0, source_kind: 0, source_mid: 0, pending_page: 0, has_more: true },
                View { name: "评论", kind: ViewKind::Comment, items: vec![], sel: 0, offset: 0, hint: "9 打开当前视频评论；动态视图 Enter 看非视频动态评论；:cmt 发评论".into(), keyword: String::new(), stack: vec![], page: 1, source_id: 0, source_kind: 0, source_mid: 0, pending_page: 0, has_more: true },
            ],
            view_idx: 1,
            queue: VecDeque::new(),
            current: None,
            status: String::new(),
            input: None,
            help: false,
            should_quit: false,
            last_eof: false,
            volume,
            low,
            live_low,
            urls: Vec::new(),
            url_idx: 0,
            play_at: None,
            stream_label: String::new(),
            want_login: false,
            fav_id: None,
            cur_page: None,
            dl_rx: None,
            repeat: Repeat::Off,
            job_tx,
            job_rx,
            dm: Vec::new(),
            dm_sel: 0,
            dm_offset: 0,
            dm_rx: None,
            dm_stop: None,
            dm_note: String::new(),
            say_pending: None,
            last_say: None,
            say_window: None,
            report: None,
            dm_follow: true,
            follow_btn: Rect::default(),
            stall_mark: None,
            recover_at: None,
            recovering: false,
            cmt_ctx: None,
            cmt_root: None,
            cmt_parent: None,
            cmt_page: 1,
            cmt_has_more: false,
            cmt_pending: None,
            like_pending: None,
            dyn_offset: String::new(),
            dyn_live_count: 0,
            cmt_dyn: None,
            image_mode: if image_preview { ImageMode::Native } else { ImageMode::Off },
            picker: Picker::new((8, 16)),
            img_protocols: Vec::new(),
            cmt_thumbs: Vec::new(),
        };
        app.load_popular();
        app.load_fav();
        app.status = format!(
            "{}｜后端 {}",
            if app.cfg.logged_in() { "已登录".to_string() } else { "未登录（:login 扫码登录）".to_string() },
            app.player.kind()
        );
        Ok(app)
    }
}

pub fn run(cfg: crate::config::Config) -> Result<()> {
    let mut app = App::new(cfg)?;
    enable_raw_mode()?;
    let mut out = std::io::stdout();
    execute!(out, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(out);
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;
    app.init_image_protocol();

    let result = app.event_loop(&mut terminal);

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen, DisableMouseCapture)?;
    terminal.show_cursor()?;
    result
}

impl App {
    /// 探测终端图形协议；探测不到就留给字符回退路径
    fn init_image_protocol(&mut self) {
        if self.image_mode == ImageMode::Off {
            return;
        }
        // tmux 会破坏/扭曲图形协议（Sixel 需特定版本，Kitty 需 passthrough），
        // 与其套一层 tmux，不如在 tmux 内直接用字符回退。
        if std::env::var("TMUX").is_ok() {
            self.image_mode = ImageMode::Chars;
            self.status = "检测到 tmux：图片使用字符四分块模式".into();
            return;
        }
        let protocol = self.picker.guess_protocol();
        self.status = format!("图片协议：{:?}", protocol);
    }

    fn event_loop(&mut self, terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> Result<()> {
        loop {
            if self.want_login {
                self.want_login = false;
                self.do_login(terminal)?;
            }
            terminal.draw(|f| self.draw(f))?;
            self.player.tick();
            if event::poll(Duration::from_millis(150))? {
                match event::read()? {
                    Event::Key(k) => self.on_key(k)?,
                    Event::Mouse(m) => self.on_mouse(m),
                    _ => {}
                }
            }
            self.poll_jobs();
            self.poll_dm();
            self.check_playback();
            self.tick_report();
            self.check_dl();
            self.check_eof();
            if self.should_quit {
                return Ok(());
            }
        }
    }

    /// TUI 内扫码登录：临时退出全屏，登录后热更新 cookie
    fn do_login(&mut self, terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> Result<()> {
        execute!(terminal.backend_mut(), LeaveAlternateScreen, DisableMouseCapture)?;
        println!("扫码登录（Esc / Ctrl-C 取消）\n");
        let r = crate::qrlogin::login(&mut self.cfg);
        execute!(terminal.backend_mut(), EnterAlternateScreen)?;
        terminal.clear()?;
        match r {
            Ok(()) => {
                self.after_login_change();
                self.status = "登录成功，已刷新 Cookie（收藏/点赞可用）".into();
            }
            Err(e) => self.status = format!("登录失败: {}", e),
        }
        Ok(())
    }

    fn after_login_change(&mut self) {
        let _ = self.job_tx.send(Job::Cfg(self.cfg.clone()));
        match Api::new(&self.cfg) {
            Ok(api) => self.api = api,
            Err(e) => self.status = format!("重建 API 失败: {}", e),
        }
        self.player.set_cookie(&self.cfg.cookie());
        self.fav_id = None;
        self.load_fav();
        self.refresh_fav_id();
    }

    fn refresh_fav_id(&mut self) {
        if self.fav_id.is_some() || !self.api.logged_in() {
            return;
        }
        if let Ok(f) = self.api.fav_folders() {
            self.fav_id = f.first().map(|x| x.0);
        }
    }

    fn check_playback(&mut self) {
        let snap = self.player.snapshot();
        self.watch_stall(&snap);
        if let Some(err) = snap.error.clone() {
            if self.url_idx + 1 < self.urls.len() {
                self.url_idx += 1;
                let url = self.urls[self.url_idx].clone();
                let title = self.current.as_ref().map(|t| t.title.clone()).unwrap_or_default();
                let live = self.current.as_ref().map(|t| t.source == Source::Live).unwrap_or(false);
                self.player.play_url(&url, &title, live);
                self.status = format!("CDN 失败（{}），切换到备用 {}", err, self.url_idx);
                self.play_at = Some(std::time::Instant::now());
            } else {
                let tail = self.player.log_tail(2).replace('\n', " ");
                self.status = format!(
                    "mpv 播放失败: {}｜日志 {}｜{}",
                    err,
                    self.player.log_path().display(),
                    tail
                );
            }
        } else if snap.loaded && snap.error.is_none() {
            if self.play_at.take().is_some() {
                let title = self.current.as_ref().map(|t| t.title.clone()).unwrap_or_default();
                let part = self.part_label();
                self.status = if part.is_empty() {
                    format!("播放中 · {} · {}", title, self.stream_label)
                } else {
                    format!("播放中 · {} · {} · {}", title, part, self.stream_label)
                };
            }
        } else if let Some(at) = self.play_at {
            if self.current.is_some() && !snap.loaded && at.elapsed() > Duration::from_secs(5) {
                let tail = self.player.log_tail(2).replace('\n', " ");
                self.status = format!(
                    "mpv 5s 内未开始播放｜日志 {}｜{}",
                    self.player.log_path().display(),
                    tail
                );
            }
        }
    }

    fn check_eof(&mut self) {
        let snap = self.player.snapshot();
        if snap.eof && !self.last_eof && self.current.is_some() {
            self.auto_next();
        }
        self.last_eof = snap.eof;
    }

    // ---------------- 弹幕 / 上报 ----------------

    fn stop_dm(&mut self) {
        if let Some(s) = self.dm_stop.take() {
            s.store(true, Ordering::Relaxed);
        }
        self.dm_rx = None;
        self.report = None;
    }

    fn start_dm(&mut self, t: &Track) {
        self.stop_dm();
        self.dm.clear();
        self.dm_sel = 0;
        self.dm_offset = 0;
        self.dm_note.clear();

        let stop = Arc::new(AtomicBool::new(false));
        let (tx, rx) = std::sync::mpsc::channel::<DmEvent>();
        match t.source {
            Source::Live => {
                let room = t.room_id.unwrap_or(0);
                if room <= 0 {
                    self.dm_note = "直播间号无效".into();
                    return;
                }
                crate::danmaku::spawn_live(self.cfg.clone(), room, tx, stop.clone());
            }
            Source::Video => {
                let cid = self.current_video_cid(t);
                let aid = t.aid;
                if cid <= 0 || aid <= 0 {
                    self.dm_note = format!("缺少 cid/aid，无法拉弹幕（cid={} aid={}）", cid, aid);
                    return;
                }
                crate::danmaku::spawn_video(
                    self.cfg.clone(),
                    cid,
                    aid,
                    t.duration,
                    tx,
                    stop.clone(),
                );
            }
        }
        self.dm_stop = Some(stop);
        self.dm_rx = Some(rx);
    }

    /// 播放开始后登记心跳（视频走 heartbeat，直播走 roomEntryAction）
    fn start_report(&mut self, t: &Track) {
        self.report = None;
        if !self.cfg.logged_in() {
            return;
        }
        match t.source {
            Source::Live => {
                let room = t.room_id.unwrap_or(0);
                if room > 0 {
                    let _ = self.job_tx.send(Job::LiveEntry { room });
                    self.report = Some(Report {
                        aid: 0,
                        cid: 0,
                        live: true,
                        last_pos: 0.0,
                        last_sent: std::time::Instant::now(),
                    });
                }
            }
            Source::Video => {
                let aid = t.aid;
                let cid = self.current_video_cid(t);
                if aid > 0 && cid > 0 {
                    self.report = Some(Report {
                        aid,
                        cid,
                        live: false,
                        last_pos: 0.0,
                        last_sent: std::time::Instant::now(),
                    });
                }
            }
        }
    }

    /// 每 20 秒、且进度前进 ≥5 秒上报一次，避免空转刷播放
    fn tick_report(&mut self) {
        let snap = self.player.snapshot();
        let payload = match &mut self.report {
            Some(r)
                if !r.live
                    && snap.loaded
                    && !snap.paused
                    && snap.pos >= 0.0
                    && (snap.pos - r.last_pos) >= 5.0
                    && r.last_sent.elapsed() >= Duration::from_secs(20) =>
            {
                r.last_pos = snap.pos;
                r.last_sent = std::time::Instant::now();
                Some((r.aid, r.cid, snap.pos as u64))
            }
            _ => None,
        };
        if let Some((aid, cid, pos)) = payload {
            let _ = self.job_tx.send(Job::Heartbeat { aid, cid, pos });
        }
    }

    fn poll_dm(&mut self) {
        let mut new: Vec<DmLine> = Vec::new();
        let mut note: Option<String> = None;
        let mut ended = false;
        if let Some(rx) = &self.dm_rx {
            while let Ok(ev) = rx.try_recv() {
                match ev {
                    DmEvent::Lines(l) => new.extend(l),
                    DmEvent::Note(n) => note = Some(n),
                    DmEvent::End => ended = true,
                }
            }
        }
        if ended {
            self.dm_rx = None;
        }
        if let Some(n) = note {
            self.dm_note = n;
        }
        if new.is_empty() {
            return;
        }
        self.dm.extend(new);
        // 只保留最近 3000 条
        if self.dm.len() > 3000 {
            let cut = self.dm.len() - 3000;
            self.dm.drain(0..cut);
        }
        // 正在看弹幕页且开了自动跟屏，才跟随到底部
        if self.dm_follow && self.views[self.view_idx].kind == ViewKind::Danmaku {
            let n = self.dm_visible().len();
            self.dm_sel = n.saturating_sub(1);
        }
    }

    /// 当前应显示的弹幕下标（视频按播放进度过滤，直播全显示）
    fn dm_visible(&self) -> Vec<usize> {
        let pos = self.player.snapshot().pos + 0.3;
        if self.dm_live_mode() {
            (0..self.dm.len()).collect()
        } else {
            self.dm
                .iter()
                .enumerate()
                .filter(|(_, l)| l.at <= pos)
                .map(|(i, _)| i)
                .collect()
        }
    }

    fn dm_live_mode(&self) -> bool {
        matches!(self.current.as_ref().map(|t| t.source), Some(Source::Live))
    }

    /// 当前分P的 cid：按 cur_page 从 pages 里取，避免误用第一分P的 cid
    fn current_video_cid(&self, t: &Track) -> i64 {
        if let Some(pi) = self.cur_page {
            if let Some(p) = t.pages.get(pi) {
                if p.cid > 0 {
                    return p.cid;
                }
            }
        }
        t.cid.or_else(|| t.pages.first().map(|p| p.cid)).unwrap_or(0)
    }

    /// 多分P视频的当前分P标签，例如「P2 后篇」
    fn part_label(&self) -> String {
        let t = match self.current.as_ref() {
            Some(t) => t,
            None => return String::new(),
        };
        if t.pages.len() <= 1 {
            return String::new();
        }
        match self.cur_page.and_then(|i| t.pages.get(i)) {
            Some(p) => format!("P{} {}", p.page, fit(&p.part, 24)),
            None => String::new(),
        }
    }

    /// 写操作统一限频：≥8 秒一条，每小时 ≤30 条
    fn allow_write(&mut self) -> bool {
        if let Some(last) = self.last_say {
            if last.elapsed() < Duration::from_secs(8) {
                self.status = format!(
                    "操作太快：距上一次还差 {} 秒",
                    8 - last.elapsed().as_secs()
                );
                return false;
            }
        }
        let count = match self.say_window {
            Some((t, c)) if t.elapsed() < Duration::from_secs(3600) => c,
            _ => 0,
        };
        if count >= 30 {
            self.status = "已达到每小时 30 次上限，稍后再试".into();
            return false;
        }
        self.last_say = Some(std::time::Instant::now());
        self.say_window = Some((
            self.say_window
                .filter(|(t, _)| t.elapsed() < Duration::from_secs(3600))
                .map(|(t, _)| t)
                .unwrap_or_else(std::time::Instant::now),
            count + 1,
        ));
        true
    }

    /// 确认后真正发送弹幕
    fn commit_say(&mut self) {
        let (text, label, target) = match self.say_pending.take() {
            Some(p) => p,
            None => return,
        };
        if !self.allow_write() {
            return;
        }
        let _ = self.job_tx.send(Job::Say { text, target });
        self.status = format!("发送中 → {}", label);
    }

    fn cmd_say(&mut self, arg: &str) {
        if arg.trim().is_empty() {
            self.status = "用法：:say <弹幕内容>".into();
            return;
        }
        if !self.cfg.allow_send {
            self.status =
                "发送弹幕默认关闭：在 ~/.config/bmus/config.toml 把 allow_send 改为 true 后重启".into();
            return;
        }
        if !self.cfg.logged_in() {
            self.status = "需要登录（:login 扫码）".into();
            return;
        }
        let t = match self.current.clone() {
            Some(t) => t,
            None => {
                self.status = "没有正在播放的视频/直播".into();
                return;
            }
        };
        match t.source {
            Source::Live => {
                let room = t.room_id.unwrap_or(0);
                if room <= 0 {
                    self.status = "当前直播间号无效".into();
                    return;
                }
                let label = format!("直播间 {}（{}）", room, t.title);
                self.say_pending = Some((arg.trim().to_string(), label, SayTarget::Live { room }));
            }
            Source::Video => {
                let cid = self.current_video_cid(&t);
                let bvid = t.bvid.clone().unwrap_or_default();
                if cid <= 0 || bvid.is_empty() {
                    self.status = "当前视频缺少 cid/bvid".into();
                    return;
                }
                let pos = self.player.snapshot().pos.max(0.0) as u64;
                let part = self.part_label();
                let label = if part.is_empty() {
                    format!("{} @ {}", bvid, fmt_time(pos))
                } else {
                    format!("{} {} @ {}", bvid, part, fmt_time(pos))
                };
                self.say_pending =
                    Some((arg.trim().to_string(), label, SayTarget::Video { cid, bvid, progress: pos }));
            }
        }
    }

    // ---------------- 评论 / 动态互动 ----------------

    fn open_comments(&mut self, ctx: CommentCtx) {
        self.view_idx = 8;
        let v = &mut self.views[8];
        v.items.clear();
        v.sel = 0;
        v.offset = 0;
        v.page = 1;
        v.hint = ctx.label.clone();
        self.cmt_ctx = Some(ctx.clone());
        self.cmt_root = None;
        self.cmt_parent = None;
        self.cmt_page = 1;
        self.cmt_has_more = false;
        self.cmt_dyn = None;
        self.cmt_thumbs.clear();
        self.img_protocols.clear();
        self.status = format!("加载评论：{}", ctx.label);
        let _ = self.job_tx.send(Job::Comments {
            ctype: ctx.ctype,
            oid: ctx.oid.clone(),
            page: 1,
            ctx,
            root: None,
        });
    }

    /// 当前播放项 / 选中项的评论区
    fn open_current_comments(&mut self) {
        let ctx = if let Some(t) = self.current.clone() {
            if t.aid > 0 {
                Some(CommentCtx {
                    ctype: 1,
                    oid: t.aid.to_string(),
                    label: format!("视频 {}", fit(&t.title, 30)),
                })
            } else {
                None
            }
        } else {
            None
        };
        let ctx = match ctx {
            Some(c) => c,
            None => match self.selected_entry() {
                Some(Entry::Dyn(d)) => CommentCtx {
                    ctype: d.ctype,
                    oid: d.oid.clone(),
                    label: format!("{} {}", d.kind, fit(&d.author, 20)),
                },
                Some(Entry::Track(t)) => {
                    let aid = if t.aid > 0 {
                        Some(t.aid)
                    } else {
                        t.bvid.as_ref().and_then(|b| self.api.video_aid(b).ok()).filter(|a| *a > 0)
                    };
                    match aid {
                        Some(a) => CommentCtx {
                            ctype: 1,
                            oid: a.to_string(),
                            label: format!("视频 {}", fit(&t.title, 30)),
                        },
                        None => {
                            self.status = "该条目没有可用评论区".into();
                            return;
                        }
                    }
                }
                _ => {
                    self.status = "先播放/选中一个视频或动态，再按 9 看评论".into();
                    return;
                }
            },
        };
        self.open_comments(ctx);
    }

    fn open_dyn_comments(&mut self, d: &DynItem) {
        // feed 卡片有时不给正文/图片，用详情接口补齐
        let mut item = d.clone();
        if item.body.trim().is_empty() || item.image_urls.is_empty() {
            if let Ok((text, urls)) = self.api.dynamic_detail_text(&item.id) {
                if item.body.trim().is_empty() && !text.is_empty() {
                    item.body = text.clone();
                    if item.text.is_empty() {
                        item.text = fit(&text, 40);
                    }
                }
                if item.image_urls.is_empty() {
                    item.image_urls = urls;
                }
            }
        }
        self.open_comments(CommentCtx {
            ctype: item.ctype,
            oid: item.oid.clone(),
            label: format!("{} {} · {}", item.kind, item.author, fit(&item.text, 20)),
        });
        self.cmt_dyn = Some(item.clone());
        if self.image_mode != ImageMode::Off && !item.image_urls.is_empty() {
            let _ = self.job_tx.send(Job::DynImages {
                key: item.id.clone(),
                urls: item.image_urls.clone(),
            });
        }
    }

    fn load_comments_page(&mut self) {
        let ctx = match self.cmt_ctx.clone() {
            Some(c) => c,
            None => {
                self.status = "先进入评论区".into();
                return;
            }
        };
        if self.cmt_root.is_some() {
            self.status = "回复列表不支持翻页，Esc 返回主评论".into();
            return;
        }
        if !self.cmt_has_more {
            self.status = "评论没有下一页".into();
            return;
        }
        self.cmt_page += 1;
        let page = self.cmt_page;
        self.status = format!("加载评论第 {} 页…", page);
        let _ = self.job_tx.send(Job::Comments {
            ctype: ctx.ctype,
            oid: ctx.oid.clone(),
            page,
            ctx,
            root: None,
        });
    }

    fn load_comment_replies(&mut self, c: &Comment) {
        let ctx = match self.cmt_ctx.clone() {
            Some(c) => c,
            None => {
                self.status = "先进入评论区".into();
                return;
            }
        };
        self.cmt_root = Some(c.rpid);
        self.cmt_parent = Some(c.rpid);
        self.status = format!("加载回复 #{}…", c.rpid);
        let _ = self.job_tx.send(Job::Comments {
            ctype: ctx.ctype,
            oid: ctx.oid.clone(),
            page: 1,
            ctx,
            root: Some(c.rpid),
        });
    }

    fn back_to_comment_root(&mut self) {
        let ctx = match self.cmt_ctx.clone() {
            Some(c) => c,
            None => return,
        };
        self.cmt_root = None;
        self.cmt_parent = None;
        self.cmt_page = 1;
        self.status = "返回主评论…".into();
        let _ = self.job_tx.send(Job::Comments {
            ctype: ctx.ctype,
            oid: ctx.oid.clone(),
            page: 1,
            ctx,
            root: None,
        });
    }

    /// 对选中的评论发起回复：进入输入模式，回车后走 :cmt 确认框
    fn start_reply(&mut self) {
        if self.cmt_ctx.is_none() {
            self.status = "先进入评论区".into();
            return;
        }
        let c = match self.selected_entry() {
            Some(Entry::Comment(c)) => c,
            _ => {
                self.status = "选中一条评论后按 R 回复".into();
                return;
            }
        };
        self.cmt_root = Some(c.rpid);
        self.cmt_parent = Some(c.rpid);
        self.input = Some((":cmt ".into(), String::new()));
        self.status = format!("回复 @{}（输入内容后回车，再确认发送）", c.user);
    }

    fn cmd_cmt(&mut self, arg: &str) {
        if arg.trim().is_empty() {
            self.status = "用法：:cmt <评论内容>（回复评论请在评论视图 Enter 进入回复后使用）".into();
            return;
        }
        if !self.cfg.allow_send {
            self.status = "写操作关闭：在 config.toml 把 allow_send 改为 true 后重启".into();
            return;
        }
        if !self.cfg.logged_in() {
            self.status = "需要登录（:login 扫码）".into();
            return;
        }
        let ctx = match self.cmt_ctx.clone() {
            Some(c) => c,
            None => {
                self.status = "先按 9 打开评论，或在动态视图 Enter 进入评论区".into();
                return;
            }
        };
        let label = match self.cmt_root {
            Some(r) => format!("{} · 回复 #{}", ctx.label, r),
            None => format!("{} · 一级评论", ctx.label),
        };
        self.cmt_pending = Some((arg.trim().to_string(), label));
    }

    fn commit_cmt(&mut self) {
        let (text, label) = match self.cmt_pending.take() {
            Some(p) => p,
            None => return,
        };
        if !self.allow_write() {
            return;
        }
        let ctx = match self.cmt_ctx.clone() {
            Some(c) => c,
            None => return,
        };
        let root = self.cmt_root.unwrap_or(0);
        let parent = self.cmt_parent.unwrap_or(0);
        let _ = self.job_tx.send(Job::CommentAdd {
            text,
            ctx,
            root,
            parent,
            label: label.clone(),
        });
        self.status = format!("发表中 → {}", label);
    }

    fn commit_like(&mut self) {
        let (label, target) = match self.like_pending.take() {
            Some(p) => p,
            None => return,
        };
        if !self.cfg.logged_in() {
            self.status = "需要登录（:login 扫码）".into();
            return;
        }
        if !self.allow_write() {
            return;
        }
        match target {
            LikeTarget::Video(aid) => match self.api.like(aid, true) {
                Ok(()) => self.status = format!("已点赞视频 · {}", label),
                Err(e) => self.status = format!("{}", e),
            },
            LikeTarget::Dyn(id) => {
                let _ = self.job_tx.send(Job::DynLike { id, on: true, label: label.clone() });
                self.status = format!("点赞中 → {}", label);
            }
            LikeTarget::Comment { rpid, ctx } => {
                let _ = self.job_tx.send(Job::CommentLike {
                    rpid,
                    on: true,
                    ctx,
                    label: label.clone(),
                });
                self.status = format!("点赞中 → {}", label);
            }
        }
    }

    fn show_rank(&mut self) {
        let room = match self.current.as_ref() {
            Some(t) if t.source == Source::Live => t.room_id.unwrap_or(0),
            _ => 0,
        };
        if room <= 0 {
            self.status = "先播放一个直播间，再看高能榜".into();
            return;
        }
        let _ = self.job_tx.send(Job::Rank { room });
        self.status = "拉取高能榜…".into();
    }

    // ---------------- 加载 ----------------

    fn load_popular(&mut self) {
        self.status = "加载热门…".into();
        match self.api.popular(50) {
            Ok(items) => {
                self.views[1].items = items.into_iter().map(Entry::Track).collect();
                self.views[1].sel = 0;
                self.views[1].offset = 0;
                self.status = format!("热门 {} 条", self.views[1].items.len());
            }
            Err(e) => self.status = format!("热门加载失败: {}", e),
        }
    }

    fn load_fav(&mut self) {
        self.views[2].stack.clear();
        match self.api.fav_folders() {
            Ok(folders) => {
                if folders.is_empty() {
                    self.views[2].items.clear();
                    self.views[2].hint = "没有收藏夹（或未登录）".into();
                    return;
                }
                self.fav_id = Some(folders[0].0);
                self.views[2].items = folders
                    .into_iter()
                    .map(|(id, title, count)| Entry::Folder { id, title, count })
                    .collect();
                self.views[2].hint = "Enter 进入收藏夹".into();
            }
            Err(e) => self.views[2].hint = format!("{}", e),
        }
    }

    fn do_search(&mut self, kw: &str) {
        let kind = if self.views[self.view_idx].kind == ViewKind::Live {
            "live_room"
        } else {
            "video"
        };
        let idx = if kind == "live_room" { 3 } else { 0 };
        self.status = format!("搜索「{}」…", kw);
        let _ = self.job_tx.send(Job::Search {
            kind: kind.to_string(),
            kw: kw.to_string(),
            page: 1,
            target: idx,
        });
    }

    fn reload(&mut self) {
        match self.views[self.view_idx].kind {
            ViewKind::Search => {
                let kw = self.views[0].keyword.clone();
                if kw.is_empty() {
                    self.status = "先按 / 搜索关键词".into();
                } else {
                    self.do_search(&kw);
                }
            }
            ViewKind::Popular => self.load_popular(),
            ViewKind::Fav => self.load_fav(),
            ViewKind::Live => {
                let kw = self.views[3].keyword.clone();
                if kw.is_empty() {
                    self.status = "在直播栏按 / 搜索直播间".into();
                } else {
                    self.do_search(&kw);
                }
            }
            ViewKind::Queue => self.sync_queue_view(),
            ViewKind::Space => self.status = "用 :up / :seasons / :season 重新载入".into(),
            ViewKind::Dynamic => self.load_dynamic(),
            ViewKind::Danmaku => {
                self.status = "弹幕视图：:say 发弹幕 / :rank 看高能榜 / Enter 跳到该时间点".into()
            }
            ViewKind::Comment => match self.cmt_ctx.clone() {
                Some(ctx) => self.open_comments(ctx),
                None => self.open_current_comments(),
            },
        }
    }

    fn sync_queue_view(&mut self) {
        self.views[4].items = self.queue.iter().cloned().map(Entry::Track).collect();
        let len = self.views[4].items.len();
        if self.views[4].sel >= len {
            self.views[4].sel = len.saturating_sub(1);
        }
    }

    // ---------------- 浏览导航 ----------------

    fn push_browse(&mut self, title: String, items: Vec<Entry>) {
        let items = keep_playable(items);
        let v = &mut self.views[self.view_idx];
        let old_hint = v.hint.clone();
        let old_items = std::mem::take(&mut v.items);
        v.stack.push((old_hint, old_items));
        v.items = items;
        v.hint = title;
        v.sel = 0;
        v.offset = 0;
    }

    fn pop_browse(&mut self) -> bool {
        let v = &mut self.views[self.view_idx];
        if let Some((hint, items)) = v.stack.pop() {
            v.items = items;
            v.hint = hint;
            v.sel = 0;
            v.offset = 0;
            true
        } else {
            false
        }
    }

    fn selected_entry(&self) -> Option<Entry> {
        let v = &self.views[self.view_idx];
        v.items.get(v.sel).cloned()
    }

    fn activate_selected(&mut self) {
        let e = match self.selected_entry() {
            Some(e) => e,
            None => return,
        };
        match e {
            Entry::Track(t) => {
                if self.views[self.view_idx].kind == ViewKind::Queue {
                    let idx = self.views[self.view_idx].sel;
                    if let Some(t2) = self.queue.remove(idx) {
                        self.play_track(t2);
                        self.sync_queue_view();
                        return;
                    }
                }
                self.play_track(t);
            }
            Entry::Folder { id, title, .. } => {
                self.status = format!("打开收藏夹 {}", title);
                match self.api.fav_items(id, 1) {
                    Ok(items) => {
                        let n = items.len();
                        self.push_browse(
                            format!("收藏夹·{}（{}）", title, n),
                            items.into_iter().map(Entry::Track).collect(),
                        );
                        let v = &mut self.views[self.view_idx];
                        v.source_id = id;
                        v.source_kind = 0;
                        v.page = 1;
                    }
                    Err(e) => self.status = format!("{}", e),
                }
            }
            Entry::Season { mid, season_id, title, .. } => {
                self.status = format!("载入合集 {}", title);
                match self.api.season_archives(mid, season_id, 1) {
                    Ok(items) => {
                        let n = items.len();
                        self.push_browse(
                            format!("{}（{}）", title, n),
                            items.into_iter().map(Entry::Track).collect(),
                        );
                        let v = &mut self.views[self.view_idx];
                        v.source_id = season_id;
                        v.source_kind = 1;
                        v.source_mid = mid;
                        v.page = 1;
                    }
                    Err(e) => self.status = format!("{}", e),
                }
            }
            Entry::Series { mid, series_id, title, .. } => {
                self.status = format!("载入播放列表 {}", title);
                match self.api.series_archives(mid, series_id, 1) {
                    Ok(items) => {
                        let n = items.len();
                        self.push_browse(
                            format!("播放列表·{}（{}）", title, n),
                            items.into_iter().map(Entry::Track).collect(),
                        );
                        let v = &mut self.views[self.view_idx];
                        v.source_id = series_id;
                        v.source_kind = 2;
                        v.source_mid = mid;
                        v.page = 1;
                    }
                    Err(e) => self.status = format!("{}", e),
                }
            }
            Entry::Up { mid, name } => self.open_up_home(mid, &name),
            Entry::Dyn(d) => {
                if d.kind == "视频" {
                    if let Some(bv) = &d.bvid {
                        match self.api.resolve(bv) {
                            Ok(t) => self.play_track(t),
                            Err(e) => self.status = format!("{}", e),
                        }
                    } else {
                        self.status = "该动态没有可播放的 BV 号".into();
                    }
                } else {
                    self.open_dyn_comments(&d);
                }
            }
            Entry::Comment(c) => self.load_comment_replies(&c),
        }
    }

    // ---------------- 播放 ----------------

    fn play_track(&mut self, t: Track) {
        if t.bvid.as_deref().unwrap_or("").is_empty() && t.room_id.is_none() {
            self.status = format!("「{}」不是普通视频（课程/番剧/专栏），暂不支持播放", t.title);
            return;
        }
        self.status = format!("解析音轨: {}", t.title);
        let _ = self.job_tx.send(Job::Play {
            track: t,
            low: self.low,
            live_low: self.live_low,
        });
    }


    /// n 的顺序：当前视频下一分P → 队列下一首 → 当前视图下一项
    fn next_track(&mut self) {
        if let Some(t) = self.current.clone() {
            if let Some(pi) = self.cur_page {
                if pi + 1 < t.pages.len() {
                    self.jump_page(pi + 1);
                    return;
                }
            }
        }
        if let Some(t) = self.queue.pop_front() {
            self.play_track(t);
            self.sync_queue_view();
            return;
        }
        if self.repeat == Repeat::Shuffle {
            if self.play_random_in_view() {
                self.sync_queue_view();
                return;
            }
        }
        if self.play_next_in_view() {
            self.sync_queue_view();
            return;
        }
        // 列表循环 / 随机兜底
        match self.repeat {
            Repeat::All => {
                if self.play_view_index(0) {
                    self.sync_queue_view();
                    return;
                }
            }
            Repeat::Shuffle => {
                if self.play_random_in_view() {
                    self.sync_queue_view();
                    return;
                }
            }
            _ => {}
        }
        self.status = "没有下一首了（队列与当前视图都已到尾部）".into();
        self.player.stop();
        self.current = None;
        self.cur_page = None;
        self.stop_dm();
        self.sync_queue_view();
    }

    /// 切换到第 idx 个分P（0-based）
    fn jump_page(&mut self, idx: usize) {
        let t = match self.current.clone() {
            Some(t) => t,
            None => return,
        };
        let pg = match t.pages.get(idx) {
            Some(p) => p.clone(),
            None => {
                self.status = format!("没有第 {} 个分P", idx + 1);
                return;
            }
        };
        let mut nt = t.clone();
        nt.cid = Some(pg.cid);
        nt.duration = pg.duration;
        nt.aid = t.aid;
        self.play_track(nt);
        self.cur_page = Some(idx);
        self.status = format!("切换到 P{} {}", pg.page, pg.part);
    }

    /// 当前视图里，正在播放项的下一条
    fn play_next_in_view(&mut self) -> bool {
        let cur_key = match self.current.as_ref() {
            Some(t) => t.key(),
            None => return false,
        };
        let v = &self.views[self.view_idx];
        let pos = v.items.iter().position(|e| e.as_track().map(|t| t.key()) == Some(cur_key.clone()));
        let next = match pos {
            Some(p) => v.items.get(p + 1).and_then(|e| e.as_track().cloned()),
            None => v.items.first().and_then(|e| e.as_track().cloned()),
        };
        match next {
            Some(t) => {
                self.play_track(t);
                true
            }
            None => false,
        }
    }

    /// EOF 自动切换：单曲循环重播当前项
    fn auto_next(&mut self) {
        if self.repeat == Repeat::One {
            if let Some(t) = self.current.clone() {
                self.play_track(t);
                return;
            }
        }
        self.next_track();
    }

    fn play_view_index(&mut self, idx: usize) -> bool {
        let t = self.views[self.view_idx]
            .items
            .get(idx)
            .and_then(|e| e.as_track().cloned());
        match t {
            Some(t) => {
                self.play_track(t);
                true
            }
            None => false,
        }
    }

    fn play_random_in_view(&mut self) -> bool {
        let len = self.views[self.view_idx].items.len();
        if len == 0 {
            return false;
        }
        // 用时间纳秒做伪随机，省掉 rand 依赖
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos() as usize)
            .unwrap_or(0);
        let mut idx = seed % len;
        for _ in 0..len {
            if self.views[self.view_idx].items[idx].as_track().is_some() {
                return self.play_view_index(idx);
            }
            idx = (idx + 1) % len;
        }
        false
    }

    fn poll_jobs(&mut self) {
        while let Ok(out) = self.job_rx.try_recv() {
            match out {
                JobOut::Fatal(m) => self.status = m,
                JobOut::Search { target, kw, items, err } => {
                    if let Some(e) = err {
                        self.status = format!("搜索失败: {}", e);
                        continue;
                    }
                    let n = items.len();
                    let target = target.min(self.views.len() - 1);
                    let v = &mut self.views[target];
                    v.stack.clear();
                    v.items = keep_playable(items.into_iter().map(Entry::Track).collect());
                    v.keyword = kw.clone();
                    v.sel = 0;
                    v.offset = 0;
                    v.page = 1;
                    v.hint = format!("「{}」{} 条", kw, n);
                    self.view_idx = target;
                    self.status = format!("搜索完成：{} 条", n);
                }
                JobOut::More { page, items, err } => {
                    self.views[self.view_idx].pending_page = 0;
                    if let Some(e) = err {
                        self.status = format!("{}（可稍后按 > 重试）", e);
                        continue;
                    }
                    if items.is_empty() {
                        self.views[self.view_idx].has_more = false;
                        self.status = "已经是最后一页".into();
                        continue;
                    }
                    let n = items.len();
                    self.views[self.view_idx]
                        .items
                        .extend(keep_playable(items.into_iter().map(Entry::Track).collect()));
                    self.views[self.view_idx].page = page;
                    self.status = format!("第 {} 页 +{} 条", page, n);
                }
                JobOut::Dynamic { lives, videos, err, log } => {
                    if err.is_some() && lives.is_empty() && videos.is_empty() {
                        self.status = format!("动态加载失败: {}", err.unwrap());
                        continue;
                    }
                    let ln = lives.len();
                    let vn = videos.len();
                    let mut items: Vec<Entry> = Vec::new();
                    items.extend(keep_playable(lives.into_iter().map(Entry::Track).collect()));
                    items.extend(keep_playable(videos.into_iter().map(Entry::Track).collect()));
                    self.views[6].items = items;
                    self.views[6].page = 1;
                    self.views[6].sel = 0;
                    self.views[6].offset = 0;
                    self.views[6].hint = if log.is_empty() {
                        format!("直播中 {} 个 · 视频更新 {} 个", ln, vn)
                    } else {
                        format!("直播中 {} 个 · 视频更新 {} 个 · {}", ln, vn, log.join(" / "))
                    };
                    self.status = match err {
                        Some(e) => format!("直播 {}，视频 {}（{}）", ln, vn, e),
                        None => format!("动态：直播 {}，视频 {}（直播在最前）", ln, vn),
                    };
                }
                JobOut::Play { track, urls, label, err } => {
                    if let Some(e) = err {
                        self.status = format!("播放失败: {}", e);
                        continue;
                    }
                    self.urls = urls.clone();
                    self.url_idx = 0;
                    let url = urls.first().cloned().unwrap_or_default();
                    let live = track.source == Source::Live;
                    self.player.play_url(&url, &track.title, live);
                    self.player.set_loop(self.repeat == Repeat::One);
                    self.status = format!("加载中… {}", label);
                    self.cur_page = if track.pages.is_empty() {
                        None
                    } else {
                        Some(track.pages.iter().position(|p| Some(p.cid) == track.cid).unwrap_or(0))
                    };
                    self.stream_label = label.clone();
                    let track2 = track.clone();
                    self.current = Some(track);
                    self.last_eof = false;
                    self.play_at = Some(std::time::Instant::now());
                    if self.recovering {
                        self.recovering = false;
                        self.status = format!("已自动恢复播放 · {}", label);
                    } else {
                        self.start_dm(&track2);
                        self.start_report(&track2);
                    }
                    self.sync_queue_view();
                }
                JobOut::Note(m) => self.status = m,
                JobOut::DmSent { text, at, label } => {
                    self.dm.push(DmLine {
                        at,
                        user: "我".into(),
                        text: text.clone(),
                        color: 0xFF_FF_FF,
                        mode: 1,
                        kind: DmKind::Danmu,
                    });
                    if self.views[self.view_idx].kind == ViewKind::Danmaku && self.dm_follow {
                        let n = self.dm_visible().len();
                        self.dm_sel = n.saturating_sub(1);
                    }
                    self.status = format!("已发送 → {}｜{}", label, text);
                }
                JobOut::DynamicAll { items, lives, next_offset, has_more, err } => {
                    let first = self.dyn_offset.is_empty();
                    self.dyn_offset = next_offset;
                    let v = &mut self.views[6];
                    v.pending_page = 0;
                    if let Some(e) = err {
                        v.has_more = false;
                        self.status = format!("动态加载失败: {}", e);
                        return;
                    }
                    let live_n = lives.len();
                    if first {
                        self.dyn_live_count = live_n;
                        // 关注 UP 里正在直播的房间放最前
                        v.items.clear();
                        v.sel = 0;
                        v.offset = 0;
                        v.items.extend(lives.into_iter().map(Entry::Track));
                    }
                    if items.is_empty() {
                        v.has_more = false;
                        self.status = format!("动态已到尾页（共 {} 条）", v.items.len());
                    } else {
                        v.items.extend(items.into_iter().map(Entry::Dyn));
                        v.has_more = has_more;
                        self.status = format!(
                            "动态 {} 条 · 直播中 {} 个（> 下一页）",
                            v.items.len(), self.dyn_live_count
                        );
                    }
                }
                JobOut::Comments { items, has_more, ctx, root, err } => {
                    let v = &mut self.views[8];
                    if let Some(e) = err {
                        self.status = format!("评论加载失败: {}", e);
                    } else {
                        self.cmt_ctx = Some(ctx.clone());
                        self.cmt_root = root;
                        self.cmt_has_more = has_more;
                        if root.is_some() {
                            v.items.clear();
                            v.sel = 0;
                            v.offset = 0;
                        } else if self.cmt_page == 1 {
                            v.items.clear();
                            v.sel = 0;
                            v.offset = 0;
                        }
                        let n = items.len();
                        v.items.extend(items.into_iter().map(Entry::Comment));
                        v.hint = if let Some(r) = root {
                            format!("评论回复 · rpid {} · {}", r, ctx.label)
                        } else {
                            format!("{} · {} 条", ctx.label, n)
                        };
                        self.status = if root.is_some() {
                            format!("回复 {} 条", n)
                        } else {
                            format!("评论 {} 条（> 下一页）", n)
                        };
                    }
                }
                JobOut::DynImages { key, images, err } => {
                    let current = self.cmt_dyn.as_ref().map(|d| d.id.as_str()) == Some(key.as_str());
                    if current {
                        self.cmt_thumbs.clear();
                        self.img_protocols.clear();
                        let mut loaded = 0;
                        for bytes in images {
                            match image::load_from_memory(&bytes) {
                                Ok(img) => {
                                    if self.image_mode == ImageMode::Native
                                        && self.picker.protocol_type != ProtocolType::Halfblocks
                                    {
                                        self.img_protocols.push(self.picker.new_resize_protocol(img));
                                    } else if self.image_mode != ImageMode::Off {
                                        self.cmt_thumbs.push(thumb_from_img(&img));
                                    }
                                    loaded += 1;
                                }
                                Err(e) => self.status = format!("图片解码失败: {}", e),
                            }
                        }
                        if loaded > 0 {
                            self.status = if !self.img_protocols.is_empty() {
                                format!("图片：{} 张 · 原生 {:?} 协议", loaded, self.picker.protocol_type)
                            } else {
                                format!("图片：{} 张 · 字符四分块渲染", loaded)
                            };
                        }
                        if let Some(e) = err {
                            self.status = format!("图片拉取失败: {}（已加载 {} 张）", e, loaded);
                        }
                    }
                }
                JobOut::Rank { list, own } => {
                    let room = self
                        .current
                        .as_ref()
                        .filter(|t| t.source == Source::Live)
                        .and_then(|t| t.room_id)
                        .unwrap_or(0);
                    for (name, score, rank) in list {
                        self.dm.push(DmLine {
                            at: 0.0,
                            user: format!("#{}", rank),
                            text: format!("{}  {} 分", name, score),
                            color: 0xFF_D7_00,
                            mode: 2,
                            kind: DmKind::Rank,
                        });
                    }
                    let tail = match own {
                        Some((n, s, r)) => {
                            let rk = if r < 0 { "未上榜".to_string() } else { format!("第 {} 名", r) };
                            format!("自己（{}）{} · {} 分", n, rk, s)
                        }
                        None => "自己未上榜（高能榜按互动/消费计分，只听音频不会上榜）".into(),
                    };
                    self.status = if room > 0 {
                        format!("高能榜已载入（room {}）｜{}", room, tail)
                    } else {
                        "高能榜已载入".into()
                    };
                    if self.views[self.view_idx].kind == ViewKind::Danmaku {
                        let n = self.dm_visible().len();
                        self.dm_sel = n.saturating_sub(1);
                    }
                }
            }
        }
    }

    fn check_dl(&mut self) {
        if let Some(rx) = &self.dl_rx {
            match rx.try_recv() {
                Ok(msg) => {
                    self.status = msg;
                    self.dl_rx = None;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
                Err(_) => self.dl_rx = None,
            }
        }
    }

    /// 下载选中项（后台线程，完成后回显路径）
    fn download_selected(&mut self) {
        let t = match self.selected_entry().and_then(|e| e.as_track().cloned()) {
            Some(t) => t,
            None => {
                self.status = "选中项不是视频".into();
                return;
            }
        };
        if t.source == Source::Live {
            self.status = "直播不能下载".into();
            return;
        }
        let cfg = self.cfg.clone();
        let api = match Api::new(&cfg) {
            Ok(a) => a,
            Err(e) => {
                self.status = format!("{}", e);
                return;
            }
        };
        // 下载全部分P（None），不再只跟着当前播放的分P
        let idx: Option<usize> = None;
        let (tx, rx) = std::sync::mpsc::channel();
        self.dl_rx = Some(rx);
        self.status = format!("下载中: {}", t.title);
        std::thread::spawn(move || {
            let msg = match crate::download::download(&api, &cfg, &t, idx, None, false) {
                Ok(r) => {
                    let mut m = format!("下载完成: {} 个文件", r.saved.len());
                    if r.skipped > 0 {
                        m.push_str(&format!("，跳过 {} 个已最新分P", r.skipped));
                    }
                    if let Some(p) = r.saved.last() {
                        m.push_str(&format!(" → {}", p.parent().map(|d| d.display().to_string()).unwrap_or_default()));
                    }
                    m
                }
                Err(e) => format!("下载失败: {}", e),
            };
            let _ = tx.send(msg);
        });
    }

    /// 动态视图：正在直播的 UP 放最前，其次是视频更新
    fn load_dynamic(&mut self) {
        if !self.api.logged_in() {
            self.view_idx = 6;
            self.status = "动态需要登录（:login）".into();
            return;
        }
        self.view_idx = 6;
        let v = &mut self.views[6];
        v.page = 1;
        v.has_more = true;
        v.pending_page = 0;
        self.status = "加载动态（全部类型）…".into();
        self.dyn_offset.clear();
        let _ = self.job_tx.send(Job::DynamicAll { offset: String::new() });
    }

    /// 当前视图翻页（>, <）
    fn load_more(&mut self) {
        let (kind, next, kw, sid, skind, smid) = {
            let v = &self.views[self.view_idx];
            (v.kind, v.page + 1, v.keyword.clone(), v.source_id, v.source_kind, v.source_mid)
        };
        if kind == ViewKind::Comment {
            self.load_comments_page();
            return;
        }
        if kind == ViewKind::Dynamic {
            let v = &mut self.views[self.view_idx];
            if !v.has_more {
                self.status = "动态没有下一页了".into();
                return;
            }
            if v.pending_page != 0 {
                self.status = "上一页还在加载，稍等".into();
                return;
            }
            v.pending_page = next;
            v.page = next;
            self.status = format!("加载动态第 {} 页…", next);
            let offset = self.dyn_offset.clone();
            let _ = self.job_tx.send(Job::DynamicAll { offset });
            return;
        }
        if matches!(kind, ViewKind::Popular | ViewKind::Queue | ViewKind::Danmaku) {
            self.status = "该视图不支持翻页".into();
            return;
        }
        if self.views[self.view_idx].pending_page != 0 {
            self.status = "上一页还在加载，稍等".into();
            return;
        }
        self.views[self.view_idx].pending_page = next;
        self.status = format!("加载第 {} 页…", next);
        let _ = self.job_tx.send(Job::More { kind, page: next, kw, sid, skind, smid });
    }

    /// 打开 UP 主页：投稿 + 合集 + 播放列表（:up <mid> 与搜索选 UP 都走这里）
    fn open_up_home(&mut self, mid: i64, name: &str) {
        let mut items: Vec<Entry> = Vec::new();
        if let Ok(Some((room, live, title))) = self.api.up_live_room(mid) {
            items.push(Entry::Track(Track {
                bvid: None,
                aid: 0,
                cid: None,
                title: if live {
                    format!("直播间·直播中 · {}（room {}）", title, room)
                } else {
                    format!("直播间·未开播（room {}）", room)
                },
                author: name.to_string(),
                duration: 0,
                source: Source::Live,
                room_id: Some(room),
                pages: vec![],
                pubdate: 0,
                up_mid: mid,
            }));
        }
        let mut videos_n = 0;
        let mut ok_videos = true;
        match self.api.space_videos(mid, 1) {
            Ok(v) => {
                videos_n = v.len();
                items.extend(v.into_iter().map(Entry::Track));
            }
            Err(e) => {
                ok_videos = false;
                self.status = if self.api.logged_in() {
                    format!("投稿加载失败: {}", e)
                } else {
                    "投稿需登录（:login）；未登录只能看合集/播放列表".to_string()
                };
            }
        }
        let mut sn = 0;
        let mut rn = 0;
        match self.api.space_seasons(mid) {
            Ok((seasons, series)) => {
                sn = seasons.len();
                rn = series.len();
                for s in seasons {
                    items.push(Entry::Season { mid: s.mid, season_id: s.season_id, title: s.title, count: s.count });
                }
                for s in series {
                    items.push(Entry::Series { mid: s.mid, series_id: s.series_id, title: s.title, count: s.count });
                }
            }
            Err(e) => {
                if ok_videos {
                    self.status = format!("合集加载失败: {}", e);
                }
            }
        }
        if ok_videos {
            self.status = format!("UP {}（{}）：投稿 {} 条，合集 {}，播放列表 {}", mid, name, videos_n, sn, rn);
        }
        let n = items.len();
        self.push_browse(
            format!("UP·{}（{}）· 投稿 + 合集/播放列表", name, n),
            items,
        );
        let v = &mut self.views[self.view_idx];
        v.source_id = mid;
        v.source_kind = 0;
        v.page = 1;
    }

    fn enqueue(&mut self, t: Track) {
        self.status = format!("已加入队列: {}", t.title);
        self.queue.push_back(t);
        self.sync_queue_view();
    }

    fn enqueue_selected(&mut self) {
        if let Some(t) = self.selected_entry().and_then(|e| e.as_track().cloned()) {
            self.enqueue(t);
        }
    }

    // ---------------- 点赞 / 收藏 ----------------

    fn selected_aid(&mut self) -> Option<i64> {
        let t = self.selected_entry()?.as_track().cloned()?;
        if t.aid > 0 {
            return Some(t.aid);
        }
        let bvid = t.bvid?;
        match self.api.video_aid(&bvid) {
            Ok(a) if a > 0 => Some(a),
            _ => None,
        }
    }

    fn act_like(&mut self, _on: bool) {
        if !self.cfg.logged_in() {
            self.status = "需要登录（:login 扫码）".into();
            return;
        }
        let kind = self.views[self.view_idx].kind;
        let picked = match kind {
            ViewKind::Comment => match self.selected_entry() {
                Some(Entry::Comment(c)) => match self.cmt_ctx.clone() {
                    Some(ctx) => Some((
                        format!("评论 @{} · {}", c.user, fit(&c.text, 20)),
                        LikeTarget::Comment { rpid: c.rpid, ctx },
                    )),
                    None => None,
                },
                _ => None,
            },
            ViewKind::Dynamic => match self.selected_entry() {
                Some(Entry::Dyn(d)) => Some((
                    format!("动态 @{} · {}", d.author, fit(&d.text, 20)),
                    LikeTarget::Dyn(d.id.clone()),
                )),
                _ => None,
            },
            _ => self
                .selected_aid()
                .map(|a| (format!("视频 aid {}", a), LikeTarget::Video(a))),
        };
        match picked {
            Some(p) => self.like_pending = Some(p),
            None => self.status = "当前选中项不支持点赞".into(),
        }
    }

    fn act_fav(&mut self, add: bool) {
        let aid = match self.selected_aid() {
            Some(a) => a,
            None => {
                self.status = "选中项没有 aid".into();
                return;
            }
        };
        if self.fav_id.is_none() {
            self.refresh_fav_id();
        }
        let fid = match self.fav_id {
            Some(f) => f,
            None => {
                self.status = "没有可用收藏夹（检查是否登录）".into();
                return;
            }
        };
        match self.api.fav_deal(aid, add, fid) {
            Ok(()) => self.status = if add { "已收藏".into() } else { "已取消收藏".into() },
            Err(e) => self.status = format!("{}", e),
        }
    }

    // ---------------- 命令 ----------------

    fn run_command(&mut self, line: &str) {
        let parts: Vec<&str> = line.splitn(2, ' ').collect();
        let cmd = parts[0];
        let arg = parts.get(1).map(|s| s.trim()).unwrap_or("");
        match cmd {
            "q" | "quit" | "wq" => self.should_quit = true,
            "clear" => {
                self.queue.clear();
                self.sync_queue_view();
                self.status = "队列已清空".into();
            }
            "vol" => {
                if let Ok(v) = arg.parse::<u32>() {
                    self.volume = v.min(150);
                    self.player.set_volume(self.volume);
                    self.status = format!("音量 {}", self.volume);
                }
            }
            "add" | "play" => match self.api.resolve(arg) {
                Ok(t) => {
                    if cmd == "play" {
                        self.play_track(t);
                    } else {
                        self.enqueue(t);
                    }
                }
                Err(e) => self.status = format!("{}", e),
            },
            "search" => self.do_search(arg),
            "live" => match arg.parse::<i64>() {
                Ok(room) => match self.api.resolve(&format!("https://live.bilibili.com/{}", room)) {
                    Ok(t) => self.play_track(t),
                    Err(e) => self.status = format!("{}", e),
                },
                Err(_) => self.do_search(arg),
            },
            "fav" => {
                if arg.is_empty() {
                    self.view_idx = 2;
                    self.load_fav();
                } else if let Ok(id) = arg.parse::<i64>() {
                    match self.api.fav_items(id, 1) {
                        Ok(items) => {
                            self.view_idx = 2;
                            let n = items.len();
                            self.push_browse(
                                format!("收藏夹 {}（{}）", id, n),
                                items.into_iter().map(Entry::Track).collect(),
                            );
                            let v = &mut self.views[2];
                            v.source_id = id;
                            v.page = 1;
                            self.status = "收藏夹已载入".into();
                        }
                        Err(e) => self.status = format!("{}", e),
                    }
                } else {
                    self.status = "用法：:fav 或 :fav <收藏夹id>".into();
                }
            }
            "up" => self.cmd_up(arg),
            "seasons" => {
                if let Ok(mid) = arg.parse::<i64>() {
                    self.cmd_seasons(mid);
                } else {
                    self.status = "用法：:seasons <mid>".into();
                }
            }
            "season" => self.cmd_season(arg),
            "dynamic" | "feed" | "dyn" => self.load_dynamic(),
            "comments" | "comment" if arg.is_empty() => self.open_current_comments(),
            "cmt" | "comment" | "comments" => self.cmd_cmt(arg),
            "reply" => self.start_reply(),
            "say" => self.cmd_say(arg),
            "rank" | "gold" => self.show_rank(),
            "like" => self.act_like(true),
            "star" | "collect" => self.act_fav(true),
            "unstar" | "uncollect" => self.act_fav(false),
            "dl" | "download" => self.download_selected(),
            "login" => {
                self.want_login = true;
            }
            "logout" => {
                self.cfg.sessdata.clear();
                self.cfg.bili_jct.clear();
                self.cfg.dede_user_id.clear();
                match self.cfg.save() {
                    Ok(()) => {
                        self.after_login_change();
                        self.views[2].items.clear();
                        self.views[2].hint.clear();
                        self.views[6].items.clear();
                        self.views[6].hint.clear();
                        self.fav_id = None;
                        self.status = "已登出（sessdata / bili_jct / DedeUserID 已清空）".into();
                    }
                    Err(e) => self.status = format!("写配置失败: {}", e),
                }
            }
            "" => {}
            other => self.status = format!("未知命令 :{}（? 查看帮助）", other),
        }
    }

    fn cmd_up(&mut self, arg: &str) {
        if arg.is_empty() {
            self.status = "用法：:up <mid> 或 :up <关键词>".into();
            return;
        }
        if let Ok(mid) = arg.parse::<i64>() {
            self.view_idx = 5;
            self.open_up_home(mid, &format!("mid {}", mid));
        } else {
            self.view_idx = 5;
            match self.api.search_users(arg) {
                Ok(users) => {
                    let n = users.len();
                    self.push_browse(
                        format!("UP 主搜索「{}」（{}）", arg, n),
                        users.into_iter().map(|(mid, name)| Entry::Up { mid, name }).collect(),
                    );
                    self.status = "Enter 进入某个 UP 的投稿/合集/播放列表".into();
                }
                Err(e) => self.status = format!("{}", e),
            }
        }
    }

    fn cmd_seasons(&mut self, mid: i64) {
        self.view_idx = 5;
        match self.api.space_seasons(mid) {
            Ok((seasons, series)) => {
                let mut items: Vec<Entry> = Vec::new();
                for s in seasons {
                    items.push(Entry::Season {
                        mid: s.mid,
                        season_id: s.season_id,
                        title: s.title,
                        count: s.count,
                    });
                }
                for s in series {
                    items.push(Entry::Series {
                        mid: s.mid,
                        series_id: s.series_id,
                        title: s.title,
                        count: s.count,
                    });
                }
                let n = items.len();
                self.push_browse(format!("UP mid {} 合集/播放列表（{}）", mid, n), items);
                self.status = format!("载入 {} 个合集/播放列表", n);
            }
            Err(e) => self.status = format!("{}", e),
        }
    }

    fn cmd_season(&mut self, arg: &str) {
        if arg.is_empty() {
            self.status = "用法：:season <BV号>".into();
            return;
        }
        let bvid = if arg.starts_with("BV") {
            arg.to_string()
        } else {
            match self.api.resolve(arg) {
                Ok(t) => t.bvid.unwrap_or_default(),
                Err(e) => {
                    self.status = format!("{}", e);
                    return;
                }
            }
        };
        match self.api.video_season(&bvid) {
            Ok(Some((season, items))) => {
                let n = items.len();
                self.view_idx = 5;
                self.push_browse(
                    format!("{}（{}）", season.title, n),
                    items.into_iter().map(Entry::Track).collect(),
                );
                self.status = format!("载入合集「{}」{} 集", season.title, n);
            }
            Ok(None) => self.status = "该视频不属于任何合集".into(),
            Err(e) => self.status = format!("{}", e),
        }
    }

    // ---------------- 按键 ----------------

    fn on_key(&mut self, k: KeyEvent) -> Result<()> {
        if let Some((_, buf)) = self.input.clone() {
            match k.code {
                KeyCode::Esc => {
                    self.input = None;
                }
                KeyCode::Enter => {
                    let text = buf.trim().to_string();
                    let label = self.input.as_ref().map(|(l, _)| l.clone()).unwrap_or_default();
                    let is_cmd = label.starts_with(':');
                    self.input = None;
                    if is_cmd {
                        if label.starts_with(":cmt ") {
                            self.cmd_cmt(&text);
                        } else {
                            self.run_command(&text);
                        }
                    } else if !text.is_empty() {
                        self.do_search(&text);
                    }
                }
                KeyCode::Backspace => {
                    if let Some((l, mut b)) = self.input.take() {
                        b.pop();
                        self.input = Some((l, b));
                    }
                }
                KeyCode::Char(c) => {
                    if let Some((l, mut b)) = self.input.take() {
                        b.push(c);
                        self.input = Some((l, b));
                    }
                }
                _ => {}
            }
            return Ok(());
        }

        if let Some((_, label, _)) = self.say_pending.clone() {
            match k.code {
                KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => self.commit_say(),
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                    self.say_pending = None;
                    self.status = "已取消发送".into();
                }
                _ => {}
            }
            let _ = label;
            return Ok(());
        }

        if self.cmt_pending.is_some() {
            match k.code {
                KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => self.commit_cmt(),
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                    self.cmt_pending = None;
                    self.status = "已取消发表".into();
                }
                _ => {}
            }
            return Ok(());
        }

        if self.like_pending.is_some() {
            match k.code {
                KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => self.commit_like(),
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                    self.like_pending = None;
                    self.status = "已取消点赞".into();
                }
                _ => {}
            }
            return Ok(());
        }

        if self.help {
            self.help = false;
            return Ok(());
        }

        match k.code {
            KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                self.should_quit = true
            }
            KeyCode::Char('q') => self.should_quit = true,
            KeyCode::Esc => {
                if self.views[self.view_idx].kind == ViewKind::Comment && self.cmt_root.is_some() {
                    self.back_to_comment_root();
                } else if !self.pop_browse() {
                    self.status = "已在顶层".into();
                }
            }
            KeyCode::Char('1') => self.view_idx = 0,
            KeyCode::Char('2') => self.view_idx = 1,
            KeyCode::Char('3') => self.view_idx = 2,
            KeyCode::Char('4') => self.view_idx = 3,
            KeyCode::Char('5') => {
                self.sync_queue_view();
                self.view_idx = 4;
            }
            KeyCode::Char('6') => self.view_idx = 5,
            KeyCode::Char('7') => self.load_dynamic(),
            KeyCode::Char('9') => self.open_current_comments(),
            KeyCode::Char('o') => {
                if self.views[self.view_idx].kind == ViewKind::Comment {
                    let urls = self
                        .cmt_dyn
                        .as_ref()
                        .map(|d| d.image_urls.clone())
                        .unwrap_or_default();
                    if urls.is_empty() {
                        self.status = "当前动态没有图片".into();
                    } else {
                        self.status = format!("正在用系统看图工具打开 {} 张图片…", urls.len().min(4));
                        let _ = self.job_tx.send(Job::OpenImages { urls });
                    }
                }
            }
            KeyCode::Char('i') => {
                self.image_mode = match self.image_mode {
                    ImageMode::Off => ImageMode::Native,
                    ImageMode::Native => ImageMode::Chars,
                    ImageMode::Chars => ImageMode::Off,
                };
                self.img_protocols.clear();
                self.cmt_thumbs.clear();
                self.status = match self.image_mode {
                    ImageMode::Off => "图片预览：关".into(),
                    ImageMode::Native => format!("图片预览：原生协议 {:?}（重新进入动态评论生效）", self.picker.protocol_type),
                    ImageMode::Chars => "图片预览：字符四分块（重新进入动态评论生效）".into(),
                };
            }
            KeyCode::Tab => self.view_idx = (self.view_idx + 1) % self.views.len(),
            KeyCode::BackTab => self.view_idx = (self.view_idx + self.views.len() - 1) % self.views.len(),
            KeyCode::Down | KeyCode::Char('j') => self.move_sel(1),
            KeyCode::Up | KeyCode::Char('k') => self.move_sel(-1),
            KeyCode::PageDown => self.move_sel(10),
            KeyCode::PageUp => self.move_sel(-10),
            KeyCode::Home | KeyCode::Char('g') => {
                if self.views[self.view_idx].kind == ViewKind::Danmaku {
                    self.dm_sel = 0;
                } else {
                    self.views[self.view_idx].sel = 0;
                }
            }
            KeyCode::End | KeyCode::Char('G') => {
                if self.views[self.view_idx].kind == ViewKind::Danmaku {
                    self.dm_sel = self.dm_visible().len().saturating_sub(1);
                } else {
                    let len = self.views[self.view_idx].items.len();
                    self.views[self.view_idx].sel = len.saturating_sub(1);
                }
            }
            KeyCode::Enter | KeyCode::Char('x') => {
                if self.views[self.view_idx].kind == ViewKind::Danmaku {
                    self.jump_to_dm()
                } else {
                    self.activate_selected()
                }
            }
            KeyCode::Char('a') => self.enqueue_selected(),
            KeyCode::Char('d') => self.remove_selected(),
            KeyCode::Char('D') => {
                self.queue.clear();
                self.sync_queue_view();
                self.status = "队列已清空".into();
            }
            KeyCode::Char('c') => {
                if self.views[self.view_idx].kind == ViewKind::Dynamic {
                    match self.selected_entry() {
                        Some(Entry::Dyn(d)) => self.open_dyn_comments(&d),
                        _ => self.status = "选中一条动态后按 c 看评论".into(),
                    }
                } else {
                    self.act_fav(true);
                }
            }
            KeyCode::Char('C') => self.act_fav(false),
            KeyCode::Char('v') => self.act_like(true),
            KeyCode::Char('w') => self.download_selected(),
            KeyCode::Char(' ') => self.player.toggle_pause(),
            KeyCode::Char('s') => {
                self.player.stop();
                self.current = None;
                self.stop_dm();
                self.status = "已停止".into();
            }
            KeyCode::Char('8') => self.view_idx = 7,
            KeyCode::Char('t') => {
                let cur = self.view_idx;
                self.view_idx = 7;
                if cur != 7 {
                    let n = self.dm_visible().len();
                    self.dm_sel = n.saturating_sub(1);
                }
            }
            KeyCode::Char('n') => self.next_track(),
            KeyCode::Char('p') => {
                if let Some(t) = self.current.clone() {
                    self.play_track(t);
                }
            }
            KeyCode::Char('l') | KeyCode::Right => self.player.seek_relative(10.0),
            KeyCode::Char('h') | KeyCode::Left => self.player.seek_relative(-10.0),
            KeyCode::Char('.') => self.player.seek_relative(60.0),
            KeyCode::Char(',') => self.player.seek_relative(-60.0),
            KeyCode::Char('>') => self.load_more(),
            KeyCode::Char('<') => self.status = "翻页用 >（下一页），已加载的页不会丢弃".into(),
            KeyCode::Char(']') => {
                if let Some(pi) = self.cur_page {
                    self.jump_page(pi + 1);
                } else {
                    self.status = "当前视频没有多分P".into();
                }
            }
            KeyCode::Char('[') => {
                if let Some(pi) = self.cur_page {
                    if pi == 0 {
                        self.status = "已是第一个分P".into();
                    } else {
                        self.jump_page(pi - 1);
                    }
                } else {
                    self.status = "当前视频没有多分P".into();
                }
            }
            KeyCode::Char('+') | KeyCode::Char('=') => {
                self.volume = (self.volume + 5).min(150);
                self.player.set_volume(self.volume);
            }
            KeyCode::Char('-') => {
                self.volume = self.volume.saturating_sub(5).max(0);
                self.player.set_volume(self.volume);
            }
            KeyCode::Char('r') => self.reload(),
            KeyCode::Char('R') => {
                if self.views[self.view_idx].kind == ViewKind::Comment {
                    self.start_reply();
                } else {
                    self.repeat = self.repeat.next();
                    self.player.set_loop(self.repeat == Repeat::One);
                    self.status = format!("播放模式：{}（作用于当前视图）", self.repeat.label());
                }
            }
            KeyCode::Char('L') => {
                let live = self
                    .current
                    .as_ref()
                    .map(|t| t.source == Source::Live)
                    .unwrap_or(self.views[self.view_idx].kind == ViewKind::Live);
                if live {
                    self.live_low = !self.live_low;
                    self.status = if self.live_low {
                        "直播省流：最低画质(250)".into()
                    } else {
                        "直播高画质".into()
                    };
                } else {
                    self.low = !self.low;
                    self.status = if self.low {
                        "省流：最低音轨".into()
                    } else {
                        "高音质：最高音轨".into()
                    };
                }
            }
            KeyCode::Char('/') => {
                let label = if self.views[self.view_idx].kind == ViewKind::Live {
                    "搜索直播间: "
                } else {
                    "搜索视频: "
                };
                self.input = Some((label.into(), String::new()));
            }
            KeyCode::Char(':') => self.input = Some((":".into(), String::new())),
            KeyCode::Char('?') => self.help = true,
            KeyCode::Char('f') => {
                self.dm_follow = !self.dm_follow;
                self.status = if self.dm_follow {
                    "弹幕自动跟屏：开".into()
                } else {
                    "弹幕自动跟屏：关（j/k 翻看历史）".into()
                };
            }
            _ => {}
        }
        Ok(())
    }

    fn on_mouse(&mut self, m: MouseEvent) {
        if let MouseEventKind::Down(MouseButton::Left) = m.kind {
            let btn = self.follow_btn;
            if btn.width > 0
                && m.column >= btn.x
                && m.column < btn.x + btn.width
                && m.row == btn.y
            {
                self.dm_follow = !self.dm_follow;
                self.status = if self.dm_follow {
                    "弹幕自动跟屏：开".into()
                } else {
                    "弹幕自动跟屏：关（j/k 翻看历史）".into()
                };
            }
        }
    }

    /// 播放卡住（位置 12 秒不动）时先切备用 CDN，再不行重新解析地址
    fn watch_stall(&mut self, snap: &crate::mpv::Snapshot) {
        let playing = self.current.is_some() && snap.loaded && !snap.paused && !snap.eof;
        if !playing {
            self.stall_mark = None;
            return;
        }
        match self.stall_mark {
            Some((at, pos)) if (snap.pos - pos).abs() > 0.5 => {
                self.stall_mark = Some((std::time::Instant::now(), snap.pos));
                let _ = at;
            }
            Some((at, pos)) if at.elapsed() >= Duration::from_secs(12) => {
                if std::env::var("BMUS_DEBUG_STALL").is_ok() {
                    eprintln!("stall detected: {:.2}s at pos {:.2}", at.elapsed().as_secs_f64(), pos);
                }
                if let Some(t) = self.recover_at {
                    if t.elapsed() < Duration::from_secs(20) {
                        return;
                    }
                }
                self.recover_at = Some(std::time::Instant::now());
                // 先换备用 CDN，不动弹幕/上报
                if self.url_idx + 1 < self.urls.len() {
                    self.url_idx += 1;
                    let url = self.urls[self.url_idx].clone();
                    let title = self.current.as_ref().map(|t| t.title.clone()).unwrap_or_default();
                    let live = self.current.as_ref().map(|t| t.source == Source::Live).unwrap_or(false);
                    self.player.play_url(&url, &title, live);
                    self.status = format!("播放卡顿 {}s，已切换备用 CDN #{}", at.elapsed().as_secs(), self.url_idx + 1);
                    self.play_at = Some(std::time::Instant::now());
                    self.stall_mark = Some((std::time::Instant::now(), pos));
                } else if let Some(t) = self.current.clone() {
                    self.recovering = true;
                    self.status = "播放卡顿，重新解析地址…".into();
                    self.play_track(t);
                    self.stall_mark = None;
                    self.play_at = Some(std::time::Instant::now());
                }
            }
            Some(_) => {}
            None => {
                self.stall_mark = Some((std::time::Instant::now(), snap.pos));
            }
        }
    }

    fn move_sel(&mut self, delta: isize) {
        if self.views[self.view_idx].kind == ViewKind::Danmaku {
            let n = self.dm_visible().len();
            if n == 0 {
                return;
            }
            self.dm_sel = (self.dm_sel as isize + delta).clamp(0, n as isize - 1) as usize;
            return;
        }
        let v = &mut self.views[self.view_idx];
        if v.items.is_empty() {
            return;
        }
        let new = v.sel as isize + delta;
        v.sel = new.clamp(0, v.items.len() as isize - 1) as usize;
    }

    fn jump_to_dm(&mut self) {
        if self.dm_live_mode() {
            self.status = "直播弹幕没有时间轴，无法跳转".into();
            return;
        }
        let vis = self.dm_visible();
        let idx = match vis.get(self.dm_sel) {
            Some(&i) => i,
            None => return,
        };
        let at = self.dm[idx].at;
        let pos = self.player.snapshot().pos;
        self.player.seek_relative(at - pos);
        self.status = format!("跳到弹幕时间点 {}", fmt_time(at.max(0.0) as u64));
    }

    fn remove_selected(&mut self) {
        let v = &self.views[self.view_idx];
        let idx = v.sel;
        match v.kind {
            ViewKind::Queue => {
                if idx < self.queue.len() {
                    if let Some(t) = self.queue.remove(idx) {
                        self.status = format!("已移除: {}", t.title);
                    }
                    self.sync_queue_view();
                }
            }
            _ => {
                if idx < v.items.len() {
                    let t = v.items[idx].title();
                    self.views[self.view_idx].items.remove(idx);
                    self.status = format!("已移除: {}", t);
                }
            }
        }
    }

    // ---------------- 绘制 ----------------

    fn draw(&mut self, f: &mut Frame) {
        let area = f.size();
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Min(5),
                Constraint::Length(3),
                Constraint::Length(1),
                Constraint::Length(1),
            ])
            .split(area);

        self.draw_now_playing(f, chunks[0]);
        self.draw_body(f, chunks[1]);
        self.draw_gauge(f, chunks[2]);
        self.draw_status(f, chunks[3]);
        if let Some((label, buf)) = &self.input {
            let x = chunks[3]
                .x
                .saturating_add(1)
                .saturating_add(unicode_width::UnicodeWidthStr::width(label.as_str()) as u16)
                .saturating_add(unicode_width::UnicodeWidthStr::width(buf.as_str()) as u16);
            f.set_cursor(x.min(chunks[3].right().saturating_sub(1)), chunks[3].y);
        }
        self.draw_hints(f, chunks[4]);

        if self.say_pending.is_some() {
            self.draw_say_dialog(f);
        }
        if self.cmt_pending.is_some() {
            self.draw_cmt_dialog(f);
        }
        if self.like_pending.is_some() {
            self.draw_like_dialog(f);
        }
        if self.help {
            self.draw_help(f, area);
        }
    }

    fn draw_now_playing(&self, f: &mut Frame, area: Rect) {
        let text = match &self.current {
            Some(t) => {
                let tag = if t.source == Source::Live { "直播" } else { "视频" };
                vec![Line::from(vec![
                    Span::styled("▶ ", Style::default().fg(Color::Green)),
                    Span::styled(format!("[{}] ", tag), Style::default().fg(Color::Magenta)),
                    Span::styled(t.title.clone(), Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
                    Span::styled(
                        format!(
                            "{}{}",
                            match (&self.cur_page, t.pages.len()) {
                                (Some(i), n) if n > 1 => format!("  P{}/{}", i + 1, n),
                                _ => String::new(),
                            },
                            format!("  — {}", t.author)
                        ),
                        Style::default().fg(Color::DarkGray),
                    ),
                ])]
            }
            None => vec![Line::from(Span::styled(
                "▶ 未在播放",
                Style::default().fg(Color::DarkGray),
            ))],
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::DarkGray))
            .title(Span::styled(" bmus ", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)));
        f.render_widget(Paragraph::new(text).block(block), area);
    }

    fn draw_body(&mut self, f: &mut Frame, area: Rect) {
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Length(14), Constraint::Min(20)])
            .split(area);

        let lines: Vec<Line> = self
            .views
            .iter()
            .enumerate()
            .map(|(i, v)| {
                let style = if i == self.view_idx {
                    Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::Gray)
                };
                Line::from(Span::styled(format!(" {}. {}", i + 1, v.name), style))
            })
            .collect();
        f.render_widget(
            Paragraph::new(lines).block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::DarkGray))
                    .title(Span::styled(" 视图 ", Style::default().fg(Color::Gray))),
            ),
            cols[0],
        );

        if self.views[self.view_idx].kind == ViewKind::Danmaku {
            self.draw_danmaku(f, cols[1]);
            return;
        }
        // 评论视图里，如果来源是某条动态，上方固定显示动态正文 + 图片
        let dyn_detail = if self.views[self.view_idx].kind == ViewKind::Comment {
            self.cmt_dyn.clone()
        } else {
            None
        };
        let list_area = if let Some(d) = dyn_detail {
            let detail_w = cols[1].width.saturating_sub(4) as usize;
            let native = !self.img_protocols.is_empty();
            let thumb_rows = self.cmt_thumbs.iter().map(|t| t.h as usize / 2).sum::<usize>();
            let count = if native { self.img_protocols.len() } else { self.cmt_thumbs.len() };
            let has_image = count > 0;
            let dl = dyn_detail_lines(&d, detail_w, has_image);
            let text_h = (dl.len() + 2)
                .min(if has_image { 12 } else { cols[1].height.saturating_sub(6) as usize })
                .max(3) as u16;
            f.render_widget(
                Paragraph::new(dl).block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(Color::DarkGray))
                        .title(Span::styled(" 动态正文 ", Style::default().fg(Color::Cyan))),
                ),
                Rect { height: text_h, ..cols[1] },
            );
            if has_image {
                let img_limit = if native { 24usize } else { thumb_rows }.min(u16::MAX as usize);
                let img_h = cols[1]
                    .height
                    .saturating_sub(text_h + 5)
                    .min(img_limit as u16)
                    .max(4);
                let parts = Layout::default()
                    .direction(Direction::Vertical)
                    .constraints([
                        Constraint::Length(text_h),
                        Constraint::Length(img_h),
                        Constraint::Min(4),
                    ])
                    .split(cols[1]);
                let iblock = Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::DarkGray))
                    .title(Span::styled(format!(" 图片 {} 张 ", count), Style::default().fg(Color::Cyan)));
                let inner = iblock.inner(parts[1]);
                f.render_widget(iblock, parts[1]);
                let grid_n = if count > 1 { 2 } else { 1 };
                let grid_w = inner.width / grid_n as u16;
                let mut cells = Vec::new();
                for c in 0..grid_n {
                    let r = Rect {
                        x: inner.x + grid_w * c as u16,
                        width: if c + 1 == grid_n { inner.width - grid_w * c as u16 } else { grid_w },
                        ..inner
                    };
                    cells.push(r);
                }
                for i in 0..count {
                    let col = i % grid_n;
                    let rows_n = (count + grid_n - 1) / grid_n;
                    let row_i = i / grid_n;
                    let cell_h = cells[col].height / rows_n as u16;
                    let area = Rect {
                        y: cells[col].y + cell_h * row_i as u16,
                        height: if row_i + 1 == rows_n {
                            cells[col].height - cell_h * row_i as u16
                        } else {
                            cell_h
                        },
                        ..cells[col]
                    };
                    if native {
                        if let Some(proto) = self.img_protocols.get_mut(i) {
                            f.render_stateful_widget(StatefulImage::new(None), area, proto);
                        }
                    } else if let Some(t) = self.cmt_thumbs.get(i) {
                        f.render_widget(
                            Paragraph::new(thumb_lines(
                                t,
                                area.width.saturating_sub(2) as usize,
                                area.height.saturating_sub(2) as usize,
                            )),
                            area,
                        );
                    }
                }
                parts[2]
            } else {
                let parts = Layout::default()
                    .direction(Direction::Vertical)
                    .constraints([Constraint::Length(text_h), Constraint::Min(4)])
                    .split(cols[1]);
                parts[1]
            }
        } else {
            cols[1]
        };
        let v = &mut self.views[self.view_idx];
        let inner_h = list_area.height.saturating_sub(2) as usize;
        if v.sel < v.offset {
            v.offset = v.sel;
        }
        if inner_h > 0 && v.sel >= v.offset + inner_h {
            v.offset = v.sel + 1 - inner_h;
        }
        let depth = if v.stack.is_empty() {
            String::new()
        } else {
            format!(" [{}]", v.stack.len())
        };
        let title = if v.hint.is_empty() {
            format!(" {}{} ({}) ", v.name, depth, v.items.len())
        } else {
            format!(" {}{} ({}) · {} ", v.name, depth, v.items.len(), v.hint)
        };

        let width = list_area.width.saturating_sub(4) as usize;
        let mut lines: Vec<Line> = Vec::new();
        if v.items.is_empty() {
            lines.push(Line::from(Span::styled(
                "  (空)",
                Style::default().fg(Color::DarkGray),
            )));
        }
        for (i, e) in v.items.iter().enumerate().skip(v.offset).take(inner_h) {
            let cur = i == v.sel;
            let time = if e.duration() > 0 {
                fmt_time(e.duration())
            } else {
                "--:--".into()
            };
            let time_w = 6;
            let sub_w = 14.min(width / 4);
            let title_w = width.saturating_sub(sub_w + time_w + 7);
            let row = format!(
                "{}{} {} {} {}",
                if cur { "▶ " } else { "  " },
                e.icon(),
                fit(&e.title(), title_w),
                fit(&e.sub(), sub_w),
                time
            );
            let style = if cur {
                Style::default().fg(Color::Black).bg(Color::Cyan).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(e.color())
            };
            lines.push(Line::from(Span::styled(row, style)));
        }
        f.render_widget(
            Paragraph::new(lines).block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::DarkGray))
                    .title(Span::styled(title, Style::default().fg(Color::Gray))),
            ),
            list_area,
        );
    }

    fn draw_danmaku(&mut self, f: &mut Frame, area: Rect) {
        let vis = self.dm_visible();
        let inner_h = area.height.saturating_sub(2) as usize;
        if self.dm_sel >= vis.len() {
            self.dm_sel = vis.len().saturating_sub(1);
        }
        if self.dm_sel < self.dm_offset {
            self.dm_offset = self.dm_sel;
        }
        if inner_h > 0 && self.dm_sel >= self.dm_offset + inner_h {
            self.dm_offset = self.dm_sel + 1 - inner_h;
        }
        let mode = if self.dm_live_mode() { "直播" } else { "视频" };
        let title = if self.dm_note.is_empty() {
            format!(" 弹幕 · {} · {} 条 · Enter 跳到该时间点 ", mode, vis.len())
        } else {
            format!(" 弹幕 · {} · {} 条 · {} ", mode, vis.len(), self.dm_note)
        };

        let width = area.width.saturating_sub(4) as usize;
        let mut lines: Vec<Line> = Vec::new();
        if vis.is_empty() {
            lines.push(Line::from(Span::styled(
                if self.current.is_some() {
                    "  (等待弹幕…)"
                } else {
                    "  (先播放视频或直播)"
                },
                Style::default().fg(Color::DarkGray),
            )));
        }
        for (i, &idx) in vis.iter().enumerate().skip(self.dm_offset).take(inner_h) {
            let l = &self.dm[idx];
            let cur = i == self.dm_sel;
            let tag = match l.kind {
                DmKind::Gift => "[礼]",
                DmKind::SuperChat => "[SC]",
                DmKind::Rank => "[榜]",
                DmKind::Danmu => match l.mode {
                    4 => "[底]",
                    5 => "[顶]",
                    _ => "",
                },
            };
            let ts = if self.dm_live_mode() {
                "     ".to_string()
            } else {
                format!("{:>5}", fmt_time(l.at.max(0.0) as u64))
            };
            let who = if l.user.is_empty() {
                String::new()
            } else {
                format!("{}: ", l.user)
            };
            let head = format!("{} {}{}{}", ts, tag, who, l.text);
            let row = fit(&head, width);
            let base = if l.color == 0 || l.color == 0xFF_FF_FF {
                Color::Gray
            } else {
                Color::Rgb(
                    ((l.color >> 16) & 0xFF) as u8,
                    ((l.color >> 8) & 0xFF) as u8,
                    (l.color & 0xFF) as u8,
                )
            };
            let color = match l.kind {
                DmKind::Gift => Color::Rgb(255, 193, 7),
                DmKind::SuperChat => Color::Rgb(255, 102, 153),
                DmKind::Rank => Color::Yellow,
                _ => base,
            };
            let style = if cur {
                Style::default().fg(Color::Black).bg(Color::Cyan).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(color)
            };
            lines.push(Line::from(Span::styled(row, style)));
        }
        f.render_widget(
            Paragraph::new(lines).block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::DarkGray))
                    .title(Span::styled(title, Style::default().fg(Color::Gray))),
            ),
            area,
        );
    }

    fn draw_confirm_dialog(&self, f: &mut Frame, title: &str, text: &str, label: &str, verb: &str) {
        let area = f.size();
        let w = 72.min(area.width.saturating_sub(4));
        let h = 4;
        let popup = Rect {
            x: (area.width.saturating_sub(w)) / 2,
            y: area.height.saturating_sub(9),
            width: w,
            height: h,
        };
        f.render_widget(Clear, popup);
        let para = Paragraph::new(vec![
            Line::from(vec![
                Span::styled("内容: ", Style::default().fg(Color::Yellow)),
                Span::styled(text.to_string(), Style::default().fg(Color::White)),
            ]),
            Line::from(Span::styled(
                format!("目标: {}", label),
                Style::default().fg(Color::Gray),
            )),
            Line::from(Span::styled(
                format!("y / Enter {}    n / Esc 取消", verb),
                Style::default().fg(Color::DarkGray),
            )),
        ])
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Yellow))
                .title(Span::styled(title, Style::default().fg(Color::Yellow))),
        );
        f.render_widget(para, popup);
    }

    fn draw_say_dialog(&self, f: &mut Frame) {
        if let Some((text, label, _)) = &self.say_pending {
            self.draw_confirm_dialog(f, " 确认发送弹幕 ", text, label, "发送");
        }
    }

    fn draw_cmt_dialog(&self, f: &mut Frame) {
        if let Some((text, label)) = &self.cmt_pending {
            self.draw_confirm_dialog(f, " 确认发表评论 ", text, label, "发表");
        }
    }

    fn draw_like_dialog(&self, f: &mut Frame) {
        if let Some((label, _)) = &self.like_pending {
            self.draw_confirm_dialog(f, " 确认点赞 ", label, "点赞内容", "点赞");
        }
    }

    fn draw_gauge(&self, f: &mut Frame, area: Rect) {
        let snap = self.player.snapshot();
        let dur = if snap.dur > 0.0 {
            snap.dur
        } else {
            self.current.as_ref().map(|t| t.duration as f64).unwrap_or(0.0)
        };
        let ratio = if dur > 0.0 { (snap.pos / dur).clamp(0.0, 1.0) } else { 0.0 };
        let state = if self.current.is_none() {
            "idle"
        } else if !snap.loaded {
            "loading"
        } else if snap.paused {
            "paused"
        } else {
            "playing"
        };
        let part = self.part_label();
        let label = format!(
            "{}  {}{} / {}   vol {}{}{}",
            state,
            if part.is_empty() { String::new() } else { format!("[{}] ", part) },
            fmt_time(snap.pos.max(0.0) as u64),
            if dur > 0.0 { fmt_time(dur as u64) } else { "--:--".into() },
            self.volume,
            match self.current.as_ref().map(|t| t.source) {
                Some(Source::Live) if self.live_low => "  [直播省流]",
                Some(Source::Video) if self.low => "  [省流]",
                _ => "",
            },
            match self.repeat {
                Repeat::Off => "",
                Repeat::One => "  [单曲]",
                Repeat::All => "  [循环]",
                Repeat::Shuffle => "  [随机]",
            }
        );
        let gauge = Gauge::default()
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::DarkGray)),
            )
            .gauge_style(Style::default().fg(Color::Cyan).bg(Color::Black))
            .ratio(ratio)
            .label(label);
        f.render_widget(gauge, area);
    }

    fn draw_status(&self, f: &mut Frame, area: Rect) {
        let text = if let Some((label, buf)) = &self.input {
            Line::from(vec![
                Span::styled(label.clone(), Style::default().fg(Color::Yellow)),
                Span::styled(buf.clone(), Style::default().fg(Color::White)),
                Span::styled("▏", Style::default().fg(Color::Yellow)),
            ])
        } else {
            Line::from(Span::styled(
                format!(" {}", self.status),
                Style::default().fg(Color::Gray),
            ))
        };
        f.render_widget(Paragraph::new(text), area);
    }

    fn draw_hints(&mut self, f: &mut Frame, area: Rect) {
        let img_tag = match self.image_mode {
            ImageMode::Off => "",
            ImageMode::Native if self.picker.protocol_type != ProtocolType::Halfblocks => "[图:原生] ",
            ImageMode::Native => "[图:字符] ",
            ImageMode::Chars => "[图:字符] ",
        };
        let hint = format!("{}{}", img_tag, "Enter 播放  a 入队  space 暂停  n 下一首  [/] 分P  8 弹幕  :say 发弹幕  > 翻页  c 收藏  v 点赞  w 下载  / 搜索  : 命令  ? 帮助  q 退出");
        let danmaku = self.views[self.view_idx].kind == ViewKind::Danmaku;
        let label = if self.dm_follow { " 自动跟屏:开 " } else { " 自动跟屏:关 " };
        let label_w = if danmaku {
            unicode_width::UnicodeWidthStr::width(label) as u16
        } else {
            0
        };
        let text = fit(&hint, area.width.saturating_sub(label_w) as usize);
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(text, Style::default().fg(Color::DarkGray)))),
            area,
        );
        if danmaku && label_w > 0 && area.width > label_w {
            let r = Rect {
                x: area.right().saturating_sub(label_w),
                y: area.y,
                width: label_w,
                height: 1,
            };
            let style = if self.dm_follow {
                Style::default().fg(Color::Black).bg(Color::Cyan).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::Gray).bg(Color::DarkGray)
            };
            f.render_widget(Paragraph::new(Span::styled(label, style)), r);
            self.follow_btn = r;
        } else {
            self.follow_btn = Rect::default();
        }
    }

    fn draw_help(&self, f: &mut Frame, area: Rect) {
        let w = 76.min(area.width);
        let h = 25.min(area.height);
        let popup = Rect {
            x: (area.width.saturating_sub(w)) / 2,
            y: (area.height.saturating_sub(h)) / 2,
            width: w,
            height: h,
        };
        f.render_widget(Clear, popup);
        let text = vec![
            Line::from(Span::styled(" 快捷键", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD))),
            Line::from(""),
            Line::from("  j/k ↑/↓   上下移动          Enter/x   播放选中 / 进入目录"),
            Line::from("  Esc       返回上一层         a         加入播放队列"),
            Line::from("  d/D       删除 / 清空队列    space     暂停/继续"),
            Line::from("  s         停止               n/p       下一首 / 重播"),
            Line::from("  h/l ←/→   ±10s    ,/. ±60s   +/-       音量"),
            Line::from("  c / C     收藏 / 取消收藏    v         点赞（接口原样）"),
            Line::from("  L         省流开关           r         刷新          w  下载(音频转码)\n  R         播放模式(单曲/循环/随机)  [/] 分P  >/< 翻页"),
            Line::from("  1-8/Tab   切换视图 7=动态 8=弹幕  g/G    首行/末行"),
            Line::from("  [/] 切换分P                   >/<       视图翻页(>)
  / 搜索    q 退出"),
            Line::from(""),
            Line::from(Span::styled(" 命令（: 开头）", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD))),
            Line::from("  :login / :logout                扫码登录 / 登出"),
            Line::from("  :up <mid>  :up <关键词>          看 UP 主投稿 / 找 UP 主"),
            Line::from("  :seasons <mid>                  看该 UP 的合集和播放列表"),
            Line::from("  :season <BV号>                  看某视频所属合集"),
            Line::from("  :fav [收藏夹id]                 收藏夹列表 / 指定收藏夹"),
            Line::from("  :like :star :unstar            点赞 / 收藏（选中项）
  :dl <BV|链接> [P]               下载音频（opus>m4a>mp3）"),
            Line::from("  :play <BV|链接>  :add <BV>  :search <词>  :live <房间号>"),
            Line::from(""),
            Line::from(Span::styled(" 弹幕 / 数据", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD))),
            Line::from("  t / 8            打开弹幕视图（Enter 跳到该条时间点）"),
            Line::from("  :say <文本>      给当前视频/直播发弹幕（需 allow_send=true）"),
            Line::from("  :rank            看当前直播间高能榜（只读）"),
            Line::from("  播放自动上报：视频每 20s 心跳，直播进房即计观看"),
            Line::from("  默认 8s/条、30 条/小时；失败不自动重试，避免风控"),
        ];
        f.render_widget(
            Paragraph::new(text)
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(Color::Cyan))
                        .title(" 帮助 "),
                )
                .wrap(Wrap { trim: false })
                .style(Style::default().bg(Color::Black)),
            popup,
        );
    }
}

/// 丢掉没有 bvid 也没有直播间的条目（课程/番剧/已失效视频），避免翻页后点开报 -400
fn keep_playable(items: Vec<Entry>) -> Vec<Entry> {
    items
        .into_iter()
        .filter(|e| match e {
            Entry::Track(t) => {
                t.room_id.is_some()
                    || t.bvid.as_deref().map(|b| !b.is_empty()).unwrap_or(false)
            }
            _ => true,
        })
        .collect()
}

/// 把动态图片下载到缓存目录，然后交给系统默认看图工具（不占用终端渲染）
fn open_images_external(api: &Api, urls: &[String]) -> Result<String> {
    if urls.is_empty() {
        anyhow::bail!("该动态没有图片");
    }
    let dir = dirs::cache_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("bmus")
        .join("images");
    std::fs::create_dir_all(&dir)?;
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let mut files = Vec::new();
    for (i, url) in urls.iter().take(4).enumerate() {
        let bytes = api.fetch_image(url)?;
        let lower = url.to_ascii_lowercase();
        let ext = if lower.contains(".png") {
            "png"
        } else if lower.contains(".webp") {
            "webp"
        } else {
            "jpg"
        };
        let path = dir.join(format!("{}-{}.{}", stamp, i + 1, ext));
        std::fs::write(&path, &bytes)?;
        files.push(path);
    }
    let mut cmd = if let Ok(viewer) = std::env::var("BMUS_IMAGE_VIEWER") {
        let mut parts = viewer.split_whitespace();
        let mut c = Command::new(parts.next().unwrap_or("xdg-open"));
        for p in parts {
            c.arg(p);
        }
        c
    } else if cfg!(target_os = "macos") {
        Command::new("open")
    } else if cfg!(target_os = "windows") {
        Command::new("explorer")
    } else {
        Command::new("xdg-open")
    };
    for f in &files {
        cmd.arg(f);
    }
    match cmd.spawn() {
        Ok(_) => Ok(format!(
            "已用系统看图工具打开 {} 张图片（缓存：{}）",
            files.len(),
            dir.display()
        )),
        Err(e) => {
            // xdg-open 缺失时再试 gio open（GNOME/KDE 常见）
            let mut fallback = Command::new("gio");
            fallback.arg("open");
            for f in &files {
                fallback.arg(f);
            }
            match fallback.spawn() {
                Ok(_) => Ok(format!("已用 gio 打开 {} 张图片", files.len())),
                Err(_) => Err(anyhow::anyhow!(
                    "启动看图工具失败: {}；可设置 BMUS_IMAGE_VIEWER 指定程序",
                    e
                )),
            }
        }
    }
}

/// 把已解码图片缩放成四分块渲染用的像素尺寸（每格 2x2 像素）
fn thumb_from_img(img: &image::DynamicImage) -> Thumb {
    let (ow, oh) = img.dimensions();
    let scale = (400.0 / ow as f32).min(120.0 / oh as f32).min(1.0);
    let mut w = ((ow as f32 * scale).round() as u32).max(2);
    let mut h = ((oh as f32 * scale).round() as u32).max(2);
    if w % 2 != 0 { w += 1; }
    if h % 2 != 0 { h += 1; }
    let rgb = img.to_rgb8();
    let small = image::imageops::resize(&rgb, w, h, image::imageops::FilterType::Lanczos3);
    Thumb { w, h, rgb: small.into_raw() }
}

/// 四分块字符：bit0=左上 bit1=右上 bit2=左下 bit3=右下（前景色填充位）
fn quad_char(mask: u8) -> char {
    match mask {
        0 => ' ', 1 => '▘', 2 => '▝', 3 => '▀', 4 => '▖', 5 => '▌', 6 => '▞', 7 => '▛',
        8 => '▗', 9 => '▚', 10 => '▐', 11 => '▜', 12 => '▄', 13 => '▙', 14 => '▟', _ => '█',
    }
}

fn rgb_avg(sum: [i64; 3], n: i64) -> [i32; 3] {
    if n == 0 {
        return [0, 0, 0];
    }
    [
        (sum[0] / n).clamp(0, 255) as i32,
        (sum[1] / n).clamp(0, 255) as i32,
        (sum[2] / n).clamp(0, 255) as i32,
    ]
}

fn rgb_color(c: [i32; 3]) -> Color {
    Color::Rgb(c[0] as u8, c[1] as u8, c[2] as u8)
}

/// 为一个 2x2 像素块挑最优的两色 + 字符（穷举 16 种填充组合）
fn pick_quad(q: [[i32; 3]; 4]) -> (char, Color, Color) {
    let mut best = (i64::MAX, ' ', Color::Reset, Color::Reset);
    for mask in 0u8..16 {
        let (mut fs, mut bs) = ([0i64; 3], [0i64; 3]);
        let (mut fnn, mut bnn) = (0i64, 0i64);
        for (i, c) in q.iter().enumerate() {
            if mask & (1u8 << i) != 0 {
                for k in 0..3 { fs[k] += c[k] as i64; }
                fnn += 1;
            } else {
                for k in 0..3 { bs[k] += c[k] as i64; }
                bnn += 1;
            }
        }
        let fg = rgb_avg(fs, fnn);
        let bg = rgb_avg(bs, bnn);
        let mut err = 0i64;
        for (i, c) in q.iter().enumerate() {
            let t = if mask & (1u8 << i) != 0 { fg } else { bg };
            for k in 0..3 {
                let d = c[k] as i64 - t[k] as i64;
                err += d * d;
            }
        }
        if err < best.0 {
            best = (err, quad_char(mask), rgb_color(fg), rgb_color(bg));
        }
    }
    (best.1, best.2, best.3)
}

/// 缩略图渲染：每格 2x2 像素，前景/背景两色自适应
fn thumb_lines(t: &Thumb, max_cols: usize, max_rows: usize) -> Vec<Line<'static>> {
    let half_cols = t.w as usize / 2;
    let half_rows = t.h as usize / 2;
    let cols = half_cols.min(max_cols.max(1));
    let rows = half_rows.min(max_rows.max(1));
    let mut out = Vec::with_capacity(rows);
    for cy in 0..rows {
        let mut spans = Vec::with_capacity(cols);
        for cx in 0..cols {
            let sx = if cols < half_cols { (cx * half_cols / cols) * 2 } else { cx * 2 };
            let sy = if rows < half_rows { (cy * half_rows / rows) * 2 } else { cy * 2 };
            let px = |dx: usize, dy: usize| -> [i32; 3] {
                let i = ((sy + dy) * t.w as usize + sx + dx) * 3;
                [t.rgb[i] as i32, t.rgb[i + 1] as i32, t.rgb[i + 2] as i32]
            };
            let (ch, fg, bg) = pick_quad([px(0, 0), px(1, 0), px(0, 1), px(1, 1)]);
            spans.push(Span::styled(ch.to_string(), Style::default().fg(fg).bg(bg)));
        }
        out.push(Line::from(spans));
    }
    out
}

/// 动态详情块：正文 + 图片提示 + 统计
fn dyn_detail_lines(d: &DynItem, width: usize, has_image: bool) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    let head = format!("[{}] {} · {}", d.kind, d.author, d.time);
    out.push(Line::from(Span::styled(
        fit(&head, width),
        Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
    )));
    if !d.title.is_empty() {
        for l in wrap_lines(&d.title, width) {
            out.push(Line::from(Span::styled(
                l,
                Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
            )));
        }
    }
    for para in d.body.split('\n') {
        if para.is_empty() {
            continue;
        }
        for l in wrap_lines(para, width) {
            out.push(Line::from(Span::styled(fit(&l, width), Style::default().fg(Color::Gray))));
        }
    }
    if d.images > 0 {
        let hint = if has_image {
            format!("🖼 图片 {} 张（已渲染第 1 张）", d.images)
        } else if d.image_urls.is_empty() {
            "🖼 图片（接口未返回缩略图链接）".into()
        } else {
            format!("🖼 图片 {} 张 · {}", d.images, fit(&d.image_urls[0], width.saturating_sub(14)))
        };
        out.push(Line::from(Span::styled(
            fit(&hint, width),
            Style::default().fg(Color::DarkGray),
        )));
    }
    let stat = format!(
        "♥ {} · 💬 {} · uid {} · 动态 {}",
        d.like, d.comment, d.mid, d.id
    );
    out.push(Line::from(Span::styled(
        fit(&stat, width),
        Style::default().fg(Color::DarkGray),
    )));
    out
}

/// 按显示宽度折行（中文按 2 列计）
fn wrap_lines(s: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![s.to_string()];
    }
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut used = 0;
    for c in s.chars() {
        let w = c.width().unwrap_or(1);
        if used + w > width && !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
            used = 0;
        }
        cur.push(c);
        used += w;
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

fn fit(s: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut used = 0;
    for c in s.chars() {
        let w = c.width().unwrap_or(1);
        if used + w > width {
            break;
        }
        out.push(c);
        used += w;
    }
    let pad = width.saturating_sub(used);
    out.push_str(&" ".repeat(pad));
    out
}


