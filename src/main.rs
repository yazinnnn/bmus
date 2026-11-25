mod api;
mod config;
mod danmaku;
mod db;
mod download;
mod mpv;
mod mpv_lib;
mod player;
mod qrlogin;
mod tui;

use anyhow::{anyhow, Result};
use api::{fmt_time, Api, Source, Track};
use clap::{Parser, Subcommand};
use config::Config;
use std::io::IsTerminal;

#[derive(Parser, Debug)]
#[command(name = "bmus", version, about = "cmus 风格的 bilibili 终端播放器（只拉音频）")]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// 打开 TUI（等同不带子命令）
    Tui,
    /// 立即播放（前台 mpv，Ctrl-C 退出）
    Play {
        /// bvid / av号 / 完整链接 / 直播链接
        target: String,
        /// 取最低音轨（省流）
        #[arg(long)]
        low: bool,
    },
    /// 只输出纯音频直链（不播放，方便管道给 mpv/ffplay）
    Url {
        target: String,
        #[arg(long)]
        low: bool,
    },
    /// 搜索视频/直播
    Search {
        keyword: String,
        #[arg(long, default_value = "video")]
        kind: String,
        #[arg(long, default_value_t = 1)]
        page: u32,
        #[arg(long)]
        json: bool,
    },
    /// 热门视频
    Popular {
        #[arg(long, default_value_t = 20)]
        limit: u32,
        #[arg(long)]
        json: bool,
    },
    /// 播放/列出直播间
    Live {
        /// 直播间号
        room: String,
        #[arg(long)]
        low: bool,
        /// 只打印流地址
        #[arg(long)]
        url_only: bool,
    },
    /// 实测某个直播间各画质档的音频码率（需要 ffmpeg）
    Livetest {
        room: i64,
        /// 每档采样秒数
        #[arg(long, default_value_t = 15)]
        secs: u64,
    },
    /// 自检：播放后端 / 依赖 / 登录态
    Doctor,
    /// 调试：关注 UP 的直播列表
    Dynlives {
        /// 打印原始 JSON
        #[arg(long)]
        raw: bool,
    },
    /// 调试：播放列表分页
    Series {
        mid: i64,
        sid: i64,
        #[arg(default_value_t = 1)]
        page: u32,
    },
    /// 查看视频信息（分P、发布时间）
    Info {
        target: String,
    },
    /// 下载音频并转码（opus > m4a(aac) > mp3）
    Dl {
        /// bvid / av号 / 完整链接
        target: String,
        /// 分P序号（从 1 开始）
        #[arg(long, short = 'p')]
        page: Option<usize>,
        /// 只下指定分P（默认全部）
        #[arg(long)]
        only_page: Option<usize>,
        /// 覆盖下载目录
        #[arg(long)]
        dir: Option<String>,
        /// 忽略已下载记录，强制重新下载
        #[arg(long)]
        force: bool,
    },
    /// 扫码登录并保存 SESSDATA
    Login,
    /// 登出（清空本地登录凭证）
    Logout,
    /// 查看当前登录状态
    Status,
    /// 收藏夹
    Fav {
        /// 列出收藏夹
        #[arg(long)]
        folders: bool,
        /// 指定收藏夹 id
        #[arg(long)]
        id: Option<i64>,
        #[arg(long)]
        json: bool,
    },
}

fn which(p: &str) -> bool {
    if p.contains('/') {
        return std::path::Path::new(p).exists();
    }
    std::env::var("PATH")
        .map(|paths| {
            paths
                .split(':')
                .any(|d| std::path::Path::new(d).join(p).exists())
        })
        .unwrap_or(false)
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let mut cfg = Config::load();

    match cli.cmd.unwrap_or(Cmd::Tui) {
        Cmd::Tui => {
            if !std::io::stdout().is_terminal() {
                eprintln!("bmus 默认是 TUI，需要在真实终端（pty）里运行。");
                eprintln!("脚本/管道场景请用子命令：bmus play <BV号> | bmus url <BV号> | bmus search <关键词>");
                return Ok(());
            }
            tui::run(cfg)
        }
        Cmd::Livetest { room, secs } => {
            let api = Api::new(&cfg)?;
            if !which(&cfg.ffmpeg_path) {
                return Err(anyhow!("需要 ffmpeg 才能测音频码率"));
            }
            for qn in [250i64, 400, 1000, 2000, 4000, 10000] {
                let stream = match api.live_stream_qn(room, qn, false) {
                    Ok(s) => s,
                    Err(e) => {
                        println!("qn {:>5}: 取流失败 {}", qn, e);
                        continue;
                    }
                };
                let url = &stream.urls[0];
                let out = std::env::temp_dir().join(format!("bili_qn{}.m4a", qn));
                let st = std::process::Command::new(&cfg.ffmpeg_path)
                    .args([
                        "-y", "-hide_banner", "-loglevel", "error",
                        "-headers", "Referer: https://live.bilibili.com/\r\n",
                        "-i", url, "-t", &secs.to_string(), "-vn", "-c:a", "copy",
                    ])
                    .arg(&out)
                    .status();
                match st {
                    Ok(st) if st.success() => {
                        let dur = std::process::Command::new(&cfg.ffmpeg_path)
                            .args(["-hide_banner", "-i"])
                            .arg(&out)
                            .args(["-f", "null", "-"])
                            .output()
                            .ok()
                            .map(|o| {
                                String::from_utf8_lossy(&o.stderr)
                                    .lines()
                                    .find(|l| l.trim_start().starts_with("Duration"))
                                    .map(|l| l.to_string())
                                    .unwrap_or_default()
                            })
                            .unwrap_or_default();
                        let size = std::fs::metadata(&out).map(|m| m.len()).unwrap_or(0) as f64;
                        let secs_f = secs as f64;
                        println!(
                            "qn {:>5}  {}  约 {:.0} kbps（{} 字节 / {}s） {}",
                            qn,
                            stream.label,
                            size * 8.0 / secs_f / 1000.0,
                            size as u64,
                            secs,
                            if dur.is_empty() { "".to_string() } else { dur }
                        );
                        let _ = std::fs::remove_file(&out);
                    }
                    _ => println!("qn {:>5}: ffmpeg 抓流失败（可能未登录/直播间需要权限）", qn),
                }
            }
            Ok(())
        }
        Cmd::Doctor => {
            println!("bmus {}", env!("CARGO_PKG_VERSION"));
            println!("配置: {}", config::config_path().display());
            println!(
                "登录: {}",
                if !cfg.sessdata.is_empty() { "已登录" } else { "未登录" }
            );
            println!("mpv_path: {} ({})", cfg.mpv_path,
                if which(&cfg.mpv_path) { "找到" } else { "缺失" });
            match mpv_lib::probe() {
                Ok(p) => println!("libmpv: 可用（{}）", p),
                Err(e) => println!("libmpv: 不可用（{}）", e),
            }
            println!("ffmpeg: {} ({})", cfg.ffmpeg_path,
                if which(&cfg.ffmpeg_path) { "找到" } else { "缺失（下载转码会失败）" });
            println!("实际后端: 见 TUI 状态栏 / backend={}", cfg.backend);
            Ok(())
        }
        Cmd::Dynlives { raw } => {
            let api = Api::new(&cfg)?;
            if raw {
                for (label, url) in [
                    (
                        "FeedList",
                        "https://api.live.bilibili.com/xlive/web-ucenter/v1/xfetter/FeedList?page=1&page_size=20&platform=web",
                    ),
                    (
                        "feed_list",
                        "https://api.live.bilibili.com/relation/v1/feed/feed_list?page=1&page_size=20",
                    ),
                ] {
                    println!("===== {}", label);
                    match api.data(url, "https://live.bilibili.com/") {
                        Ok(d) => println!("{}", serde_json::to_string_pretty(&d)?),
                        Err(e) => println!("失败: {}", e),
                    }
                }
                return Ok(());
            }
            let (lives, log) = api.dynamic_lives_dbg()?;
            for l in &log {
                println!("[来源] {}", l);
            }
            println!("直播中 {} 个", lives.len());
            for t in &lives {
                println!(
                    "room {}  mid {}  {:<16} {}",
                    t.room_id.unwrap_or(0),
                    t.up_mid,
                    t.author,
                    t.title
                );
            }
            Ok(())
        }
        Cmd::Series { mid, sid, page } => {
            let api = Api::new(&cfg)?;
            let t0 = std::time::Instant::now();
            let items = api.series_archives(mid, sid, page)?;
            eprintln!("OK {} 条，用时 {:?}", items.len(), t0.elapsed());
            for t in &items {
                println!("{} {}", t.bvid.clone().unwrap_or_default(), t.title);
            }
            Ok(())
        }
        Cmd::Info { target } => {
            let api = Api::new(&cfg)?;
            let t = if target.starts_with("BV") || target.starts_with("av") || target.contains("BV1") {
                match api.resolve(&target) {
                    Ok(t) => t,
                    Err(e) => return Err(e),
                }
            } else {
                api.resolve(&target)?
            };
            println!("{}", t.title);
            println!("  作者 {}\taid {}\t时长 {}\t发布 {}", t.author, t.aid, fmt_time(t.duration), t.pubdate);
            println!("  分P {} 个", t.pages.len());
            for p in &t.pages {
                println!("   P{} cid={} {} ({})", p.page, p.cid, p.part, fmt_time(p.duration));
            }
            Ok(())
        }
        Cmd::Dl { target, page, only_page, dir, force } => {
            let api = Api::new(&cfg)?;
            let t = api.resolve(&target)?;
            let idx = only_page.or(page).map(|p| p.saturating_sub(1));
            let r = download::download(&api, &cfg, &t, idx, dir.as_deref(), force)?;
            for p in &r.saved {
                println!("{}", p.display());
            }
            if r.skipped > 0 {
                eprintln!("{} 个分P已是最新，跳过", r.skipped);
            }
            Ok(())
        }
        Cmd::Login => qrlogin::login(&mut cfg),
        Cmd::Logout => {
            if cfg.sessdata.is_empty() {
                println!("当前未登录，无需登出");
                return Ok(());
            }
            cfg.sessdata.clear();
            cfg.bili_jct.clear();
            cfg.dede_user_id.clear();
            cfg.save()?;
            println!("已登出（{} 中的 sessdata/bili_jct/DedeUserID 已清空）", config::config_path().display());
            Ok(())
        }
        Cmd::Status => {
            println!("配置: {}", config::config_path().display());
            if !cfg.logged_in() {
                println!("登录: 未登录（bmus login 扫码）");
            } else {
                let uid = if cfg.dede_user_id.is_empty() {
                    "-".to_string()
                } else {
                    cfg.dede_user_id.clone()
                };
                let name = Api::new(&cfg)
                    .ok()
                    .and_then(|a| a.nav_info().ok())
                    .map(|(n, _)| n)
                    .unwrap_or_else(|| "-".to_string());
                println!("登录: 已登录 · {}（uid {}）", name, uid);
                println!("SESSDATA: {}…（{} 字符）", &cfg.sessdata[..cfg.sessdata.len().min(8)], cfg.sessdata.len());
            }
            Ok(())
        }
        Cmd::Play { target, low } => {
            let api = Api::new(&cfg)?;
            let t = api.resolve(&target)?;
            let stream = match t.source {
                Source::Video => api.video_audio_quality(&t, low)?,
                Source::Live => api.live_stream(t.room_id.unwrap_or(0), low)?,
            };
            eprintln!(
                "{} → {}（{}）",
                t.title,
                stream.label,
                if stream.audio_only { "纯音频，不拉视频流" } else { "直播为复用流 + 禁用视频" }
            );
            let url = stream.urls.first().cloned().unwrap_or_default();
            mpv::play_blocking(&cfg.mpv_path, &url, &cfg.cookie(), t.source == Source::Live)
        }
        Cmd::Url { target, low } => {
            let api = Api::new(&cfg)?;
            let t = api.resolve(&target)?;
            let stream = match t.source {
                Source::Video => api.video_audio_quality(&t, low)?,
                Source::Live => api.live_stream(t.room_id.unwrap_or(0), low)?,
            };
            eprintln!("{} [{}]", t.title, stream.label);
            println!("{}", stream.urls.first().cloned().unwrap_or_default());
            Ok(())
        }
        Cmd::Search { keyword, kind, page, json } => {
            let api = Api::new(&cfg)?;
            let items = api.search(&keyword, &kind, page)?;
            print_tracks(&items, json)
        }
        Cmd::Popular { limit, json } => {
            let api = Api::new(&cfg)?;
            let items = api.popular(limit)?;
            print_tracks(&items, json)
        }
        Cmd::Live { room, low, url_only } => {
            let api = Api::new(&cfg)?;
            let id: i64 = room
                .trim()
                .rsplit('/')
                .next()
                .unwrap_or("")
                .parse()
                .map_err(|_| anyhow!("直播间号无效: {}", room))?;
            let (title, author, living) = api.live_info(id)?;
            if !living {
                return Err(anyhow!("{} 未开播", title));
            }
            let stream = api.live_stream(id, low)?;
            if url_only {
                println!("{}", stream.urls.first().cloned().unwrap_or_default());
                return Ok(());
            }
            eprintln!("{} · {} [{}]", author, title, stream.label);
            mpv::play_blocking(
                &cfg.mpv_path,
                &stream.urls.first().cloned().unwrap_or_default(),
                &cfg.cookie(),
                true,
            )
        }
        Cmd::Fav { folders, id, json } => {
            let api = Api::new(&cfg)?;
            if folders {
                for (fid, title, count) in api.fav_folders()? {
                    println!("{:>12}  {:>5}  {}", fid, count, title);
                }
                return Ok(());
            }
            let fid = match id {
                Some(i) => i,
                None => api
                    .fav_folders()?
                    .first()
                    .map(|f| f.0)
                    .ok_or_else(|| anyhow!("没有收藏夹"))?,
            };
            let items = api.fav_items(fid, 1)?;
            print_tracks(&items, json)
        }
    }
}

fn print_tracks(items: &[Track], json: bool) -> Result<()> {
    if json {
        let v: Vec<serde_json::Value> = items
            .iter()
            .map(|t| {
                serde_json::json!({
                    "bvid": t.bvid,
                    "aid": t.aid,
                    "room_id": t.room_id,
                    "title": t.title,
                    "author": t.author,
                    "duration": t.duration,
                    "kind": match t.source { Source::Video => "video", Source::Live => "live" },
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&v)?);
        return Ok(());
    }
    for (i, t) in items.iter().enumerate() {
        let id = t.bvid.clone().unwrap_or_else(|| {
            t.room_id.map(|r| format!("live:{}", r)).unwrap_or_else(|| format!("av{}", t.aid))
        });
        let mark = if t.source == Source::Live { "◉" } else { "♪" };
        println!(
            "{:>2}. {} {:>12}  {:>7}  {}  — {}",
            i + 1,
            mark,
            id,
            fmt_time(t.duration),
            truncate(&t.title, 46),
            t.author
        );
    }
    Ok(())
}

fn truncate(s: &str, n: usize) -> String {
    let mut out: String = s.chars().take(n).collect();
    if s.chars().count() > n {
        out.push('…');
    }
    out
}
