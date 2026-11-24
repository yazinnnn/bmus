//! cmus 风格 TUI：视图 + 队列 + 浏览导航（收藏夹 / UP 主 / 合集 / 播放列表）

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Gauge, Paragraph, Wrap};
use ratatui::{Frame, Terminal};
use std::collections::VecDeque;
use std::io::Stdout;
use std::time::Duration;
use unicode_width::UnicodeWidthChar;

use crate::api::{fmt_time, Api, Source, Track};

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

/// 后台任务：网络请求全部放到 worker 线程，避免卡住 UI
pub enum Job {
    Cfg(crate::config::Config),
    Search { kind: String, kw: String, page: u32, target: usize },
    More { kind: ViewKind, page: u32, kw: String, sid: i64, skind: u8, smid: i64 },
    Play { track: Track, low: bool, live_low: bool },
    Dynamic,
}

pub enum JobOut {
    Fatal(String),
    Search { target: usize, kw: String, items: Vec<Track>, err: Option<String> },
    More { page: u32, items: Vec<Track>, err: Option<String> },
    Play { track: Track, urls: Vec<String>, label: String, err: Option<String> },
    Dynamic { lives: Vec<Track>, videos: Vec<Track>, err: Option<String>, log: Vec<String> },
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
                        ViewKind::Popular | ViewKind::Queue => anyhow::bail!("该视图不支持翻页"),
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
}

#[derive(Clone, Debug)]
pub enum Entry {
    Track(Track),
    Folder { id: i64, title: String, count: u64 },
    Season { mid: i64, season_id: i64, title: String, count: u64 },
    Series { mid: i64, series_id: i64, title: String, count: u64 },
    Up { mid: i64, name: String },
}

impl Entry {
    fn title(&self) -> String {
        match self {
            Entry::Track(t) => t.title.clone(),
            Entry::Folder { title, .. } => title.clone(),
            Entry::Season { title, .. } => title.clone(),
            Entry::Series { title, .. } => format!("[播放列表] {}", title),
            Entry::Up { name, .. } => name.clone(),
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
        }
    }

    fn sub(&self) -> String {
        match self {
            Entry::Track(t) => t.author.clone(),
            Entry::Folder { count, .. } => format!("{} 个视频", count),
            Entry::Season { count, .. } => format!("合集 {} 集", count),
            Entry::Series { count, .. } => format!("播放列表 {} 集", count),
            Entry::Up { mid, .. } => format!("mid {}", mid),
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
}

impl App {
    pub fn new(cfg: crate::config::Config) -> Result<Self> {
        let api = Api::new(&cfg)?;
        let player = crate::player::Player::new(&cfg)?;
        let low = cfg.audio_quality == "lowest";
        let live_low = cfg.live_low;
        let volume = cfg.volume;

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
    execute!(out, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(out);
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;

    let result = app.event_loop(&mut terminal);

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    result
}

impl App {
    fn event_loop(&mut self, terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> Result<()> {
        loop {
            if self.want_login {
                self.want_login = false;
                self.do_login(terminal)?;
            }
            terminal.draw(|f| self.draw(f))?;
            self.player.tick();
            if event::poll(Duration::from_millis(150))? {
                if let Event::Key(k) = event::read()? {
                    self.on_key(k)?;
                }
            }
            self.poll_jobs();
            self.check_playback();
            self.check_dl();
            self.check_eof();
            if self.should_quit {
                return Ok(());
            }
        }
    }

    /// TUI 内扫码登录：临时退出全屏，登录后热更新 cookie
    fn do_login(&mut self, terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> Result<()> {
        execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
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
                self.status = format!("播放中 · {} · {}", title, self.stream_label);
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
                    self.stream_label = label;
                    self.current = Some(track);
                    self.last_eof = false;
                    self.play_at = Some(std::time::Instant::now());
                    self.sync_queue_view();
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
        self.status = "加载动态…".into();
        let _ = self.job_tx.send(Job::Dynamic);
    }

    /// 当前视图翻页（>, <）
    fn load_more(&mut self) {
        let (kind, next, kw, sid, skind, smid) = {
            let v = &self.views[self.view_idx];
            (v.kind, v.page + 1, v.keyword.clone(), v.source_id, v.source_kind, v.source_mid)
        };
        if matches!(kind, ViewKind::Popular | ViewKind::Queue) {
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
        let aid = match self.selected_aid() {
            Some(a) => a,
            None => {
                self.status = "选中项没有 aid".into();
                return;
            }
        };
        match self.api.like(aid, true) {
            Ok(()) => self.status = "已发送点赞（重复点赞不影响）".into(),
            Err(e) => {
                let m = format!("{}", e);
                if m.contains("65004") || m.contains("已经") {
                    self.status = "已点过赞".into();
                } else {
                    self.status = m;
                }
            }
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
            "dynamic" | "feed" => self.load_dynamic(),
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
                    let is_cmd = self.input.as_ref().map(|(l, _)| l.starts_with(':')).unwrap_or(false);
                    self.input = None;
                    if is_cmd {
                        self.run_command(&text);
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
                if !self.pop_browse() {
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
            KeyCode::Tab => self.view_idx = (self.view_idx + 1) % self.views.len(),
            KeyCode::BackTab => self.view_idx = (self.view_idx + self.views.len() - 1) % self.views.len(),
            KeyCode::Down | KeyCode::Char('j') => self.move_sel(1),
            KeyCode::Up | KeyCode::Char('k') => self.move_sel(-1),
            KeyCode::PageDown => self.move_sel(10),
            KeyCode::PageUp => self.move_sel(-10),
            KeyCode::Home | KeyCode::Char('g') => self.views[self.view_idx].sel = 0,
            KeyCode::End | KeyCode::Char('G') => {
                let len = self.views[self.view_idx].items.len();
                self.views[self.view_idx].sel = len.saturating_sub(1);
            }
            KeyCode::Enter | KeyCode::Char('x') => self.activate_selected(),
            KeyCode::Char('a') => self.enqueue_selected(),
            KeyCode::Char('d') => self.remove_selected(),
            KeyCode::Char('D') => {
                self.queue.clear();
                self.sync_queue_view();
                self.status = "队列已清空".into();
            }
            KeyCode::Char('c') => self.act_fav(true),
            KeyCode::Char('C') => self.act_fav(false),
            KeyCode::Char('v') => self.act_like(true),
            KeyCode::Char('w') => self.download_selected(),
            KeyCode::Char(' ') => self.player.toggle_pause(),
            KeyCode::Char('s') => {
                self.player.stop();
                self.current = None;
                self.status = "已停止".into();
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
                self.repeat = self.repeat.next();
                self.player.set_loop(self.repeat == Repeat::One);
                self.status = format!("播放模式：{}（作用于当前视图）", self.repeat.label());
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
            _ => {}
        }
        Ok(())
    }

    fn move_sel(&mut self, delta: isize) {
        let v = &mut self.views[self.view_idx];
        if v.items.is_empty() {
            return;
        }
        let new = v.sel as isize + delta;
        v.sel = new.clamp(0, v.items.len() as isize - 1) as usize;
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

        let v = &mut self.views[self.view_idx];
        let inner_h = cols[1].height.saturating_sub(2) as usize;
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

        let width = cols[1].width.saturating_sub(4) as usize;
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
            cols[1],
        );
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
        let label = format!(
            "{}  {} / {}   vol {}{}{}",
            state,
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

    fn draw_hints(&self, f: &mut Frame, area: Rect) {
        let hint = " Enter 播放/进入  a 入队  space 暂停  n 下一首  [/] 分P  > 翻页  ,/. ±1min  c 收藏  v 点赞  w 下载  / 搜索  : 命令  ? 帮助  q 退出";
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(hint, Style::default().fg(Color::DarkGray)))),
            area,
        );
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
            Line::from("  1-7/Tab   切换视图 7=动态    g/G       首行/末行"),
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
