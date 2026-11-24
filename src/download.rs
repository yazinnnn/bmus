//! 下载 + 转码：优先 opus，其次 m4a(aac)，最后 mp3；码率上限可配（默认 192kbps）。
//! 支持多分P、写入元数据、按 pubdate 去重、UP 目录与 uid 的稳定映射（SQLite）。

use anyhow::{anyhow, bail, Context, Result};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::api::{Api, Source, Track, UA};
use crate::config::Config;
use crate::db::Db;

fn sanitize(name: &str) -> String {
    let s: String = name
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            c if (c as u32) < 0x20 => '_',
            c => c,
        })
        .collect();
    s.trim().trim_matches('.').to_string()
}

/// unix 秒 -> 北京时间 YYYYMMDD
fn upload_date(ts: u64) -> String {
    if ts == 0 {
        return "NA".into();
    }
    let days = (ts / 86400) as i64 + 8 * 3600 / 86400;
    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{:04}{:02}{:02}", y, m, d)
}

fn truncate_bytes(s: &str, n: usize) -> String {
    let mut out = String::new();
    for c in s.chars() {
        if out.len() + c.len_utf8() > n {
            break;
        }
        out.push(c);
    }
    out
}

/// 渲染 yt-dlp 风格模板
fn render(template: &str, date: &str, title: &str, id: &str, ext: &str) -> String {
    let mut out = String::new();
    let mut rest = template;
    while let Some(i) = rest.find("%(") {
        out.push_str(&rest[..i]);
        let after = &rest[i + 2..];
        match after.find(')') {
            Some(j) => {
                let field = &after[..j];
                let val = if field == "upload_date" {
                    date.to_string()
                } else if field == "id" || field == "display_id" {
                    id.to_string()
                } else if field == "ext" {
                    ext.to_string()
                } else if let Some(n) = field.strip_prefix("title").and_then(|f| {
                    f.strip_prefix('.')
                        .and_then(|f| f.strip_suffix('B'))
                        .and_then(|n| n.parse::<usize>().ok())
                }) {
                    truncate_bytes(title, n)
                } else if field == "title" {
                    title.to_string()
                } else {
                    String::new()
                };
                out.push_str(&val);
                rest = &after[j + 1..];
                if let Some(stripped) = rest.strip_prefix('s') {
                    rest = stripped;
                }
            }
            None => {
                out.push_str(&rest[i..]);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
}

fn expand_home(p: &str) -> PathBuf {
    if let Some(rest) = p.strip_prefix("~/") {
        if let Some(h) = dirs::home_dir() {
            return h.join(rest);
        }
    }
    PathBuf::from(p)
}

fn have(bin: &str) -> bool {
    Command::new(bin)
        .arg("-version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn run_ffmpeg(bin: &str, args: &[String]) -> bool {
    match Command::new(bin)
        .args(args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .output()
    {
        Ok(out) => {
            if !out.status.success() {
                let err = String::from_utf8_lossy(&out.stderr);
                eprintln!("ffmpeg 失败: {}", err.lines().last().unwrap_or(""));
            }
            out.status.success()
        }
        Err(e) => {
            eprintln!("ffmpeg 无法执行: {}", e);
            false
        }
    }
}

fn fetch(url: &str, referer: &str, cookie: &str, dst: &Path) -> Result<()> {
    let client = reqwest::blocking::Client::builder()
        .user_agent(UA)
        .timeout(std::time::Duration::from_secs(600))
        .build()?;
    let mut req = client
        .get(url)
        .header("Referer", referer)
        .header("Origin", "https://www.bilibili.com");
    if !cookie.is_empty() {
        req = req.header("Cookie", cookie);
    }
    let mut resp = req.send().context("请求音频流失败")?;
    if !resp.status().is_success() {
        bail!("音频流 HTTP {}", resp.status());
    }
    let mut f = fs::File::create(dst)?;
    resp.copy_to(&mut f)?;
    f.flush()?;
    Ok(())
}

fn meta_args(title: &str, author: &str, url: &str, date: &str, bvid: &str) -> Vec<String> {
    vec![
        "-metadata".into(), format!("title={}", title),
        "-metadata".into(), format!("artist={}", author),
        "-metadata".into(), format!("album={}", author),
        "-metadata".into(), format!("album_artist={}", author),
        "-metadata".into(), format!("date={}", date),
        "-metadata".into(), format!("comment={}", url),
        "-metadata".into(), format!("description={}", url),
        "-metadata".into(), format!("purl={}", url),
        "-metadata".into(), format!("source={}", url),
        "-metadata".into(), format!("bvid={}", bvid),
    ]
}

pub struct DlResult {
    pub saved: Vec<PathBuf>,
    pub skipped: usize,
}

/// 下载音频（全部分P 或指定一P），写入元数据并按 pubdate 去重
pub fn download(
    api: &Api,
    cfg: &Config,
    track: &Track,
    page: Option<usize>,
    out_dir: Option<&str>,
    force: bool,
) -> Result<DlResult> {
    if track.source == Source::Live {
        bail!("直播不支持下载");
    }
    let bvid = track
        .bvid
        .clone()
        .ok_or_else(|| anyhow!("缺少 bvid"))?;
    // 始终补全详情：分P、作者、up mid、发布时间
    let base_track = match api.video_detail(&bvid) {
        Ok(d) => d,
        Err(_) => track.clone(),
    };
    let author = if !base_track.author.is_empty() {
        base_track.author.clone()
    } else {
        track.author.clone()
    };
    let uid = if base_track.up_mid > 0 { base_track.up_mid } else { track.up_mid };
    let pubdate = if base_track.pubdate > 0 { base_track.pubdate } else { track.pubdate };

    let base = out_dir
        .map(PathBuf::from)
        .unwrap_or_else(|| expand_home(&cfg.download_dir));
    let db = Db::open()?;
    let folder = db.folder_for(uid, &author)?;
    let dir = base.join(&folder);
    fs::create_dir_all(&dir)?;

    // 决定要下哪些分P
    let pages: Vec<usize> = if base_track.pages.is_empty() {
        vec![0]
    } else {
        match page {
            Some(i) if i < base_track.pages.len() => vec![i],
            Some(i) => bail!("没有第 {} 个分P（共 {}）", i + 1, base_track.pages.len()),
            None => (0..base_track.pages.len()).collect(),
        }
    };

    let mut saved = Vec::new();
    let mut skipped = 0usize;
    for idx in pages {
        let mut t = base_track.clone();
        let mut title = base_track.title.clone();
        let mut part_no = 0i64;
        if let Some(p) = t.pages.get(idx) {
            t.cid = Some(p.cid);
            part_no = p.page;
            if t.pages.len() > 1 {
                title = format!(
                    "{} [P{}{}]",
                    base_track.title,
                    p.page,
                    if p.part.is_empty() { String::new() } else { format!(" {}", p.part) }
                );
            }
        }
        let id_key = if base_track.pages.len() > 1 {
            format!("{}-p{}", bvid, part_no)
        } else {
            bvid.clone()
        };
        let url = if part_no > 0 {
            format!("https://www.bilibili.com/video/{}/?p={}", bvid, part_no)
        } else {
            format!("https://www.bilibili.com/video/{}", bvid)
        };

        if !force {
            if let Some(rec) = db.record(&id_key) {
                if rec.pubdate == pubdate && Path::new(&rec.path).exists() {
                    skipped += 1;
                    continue;
                }
            }
        }

        let stream = api.video_audio(&t)?;
        if stream.urls.is_empty() {
            bail!("无可用音轨");
        }
        let tmp = dir.join(format!(".bili_tmp_{}", std::process::id()));
        let mut last_err = anyhow!("下载失败");
        let mut ok = false;
        'outer: for u in &stream.urls {
            for attempt in 0..3 {
                match fetch(u, "https://www.bilibili.com/", &cfg.cookie(), &tmp) {
                    Ok(()) => {
                        ok = true;
                        break 'outer;
                    }
                    Err(e) => {
                        last_err = e;
                        std::thread::sleep(std::time::Duration::from_millis(
                            800 * (attempt + 1) as u64,
                        ));
                    }
                }
            }
        }
        if !ok {
            let _ = fs::remove_file(&tmp);
            return Err(last_err);
        }
        if fs::metadata(&tmp).map(|m| m.len()).unwrap_or(0) < 1024 {
            let _ = fs::remove_file(&tmp);
            bail!("下载内容为空（可能被风控或需要登录）");
        }

        let max = cfg.max_bitrate.max(32) as u64;
        let src_kbps = if stream.bitrate > 0 { stream.bitrate } else { 128 };
        let target = src_kbps.min(max);
        let is_aac = stream.codec.contains("mp4a")
            || stream.codec.contains("aac")
            || stream.codec.is_empty();
        let ff = &cfg.ffmpeg_path;
        let has_ff = have(ff);
        let date = upload_date(pubdate);
        let meta = meta_args(&title, &author, &url, &date, &bvid);
        // 只用了 150B 也不够：/ 等非法字符会当目录分隔符，整段超 255 字节也建不出文件
        let name = |ext: &str| {
            let raw = render(&cfg.filename_template, &date, &title, &bvid, ext);
            let mut s = sanitize(&raw);
            let suffix = format!(".{}", ext);
            if s.len() > 240 {
                let keep = 240usize.saturating_sub(suffix.len());
                s = format!("{}{}", truncate_bytes(&s, keep), suffix);
            } else if !s.ends_with(&suffix) {
                s.push_str(&suffix);
            }
            dir.join(s)
        };

        // 1) opus
        if has_ff {
            let out = name("opus");
            let mut args: Vec<String> = vec![
                "-y".into(),
                "-i".into(),
                tmp.to_string_lossy().into(),
                "-vn".into(),
                "-c:a".into(),
                "libopus".into(),
                "-b:a".into(),
                format!("{}k", target),
            ];
            args.extend(meta.clone());
            args.push(out.to_string_lossy().into());
            if run_ffmpeg(ff, &args) && fs::metadata(&out).map(|m| m.len()).unwrap_or(0) > 1024 {
                let _ = fs::remove_file(&tmp);
                db.upsert(&id_key, uid, &bvid, part_no, &title, &out, pubdate)?;
                saved.push(out);
                continue;
            }
        }

        // 2) m4a
        if has_ff {
            let out = name("m4a");
            let mut args: Vec<String> = vec![
                "-y".into(),
                "-i".into(),
                tmp.to_string_lossy().into(),
                "-vn".into(),
            ];
            if is_aac && src_kbps <= max {
                args.extend(["-c:a".into(), "copy".into()]);
            } else {
                args.extend(["-c:a".into(), "aac".into(), "-b:a".into(), format!("{}k", target)]);
            }
            args.push("-movflags".into());
            args.push("+faststart".into());
            args.extend(meta.clone());
            args.push(out.to_string_lossy().into());
            if run_ffmpeg(ff, &args) && fs::metadata(&out).map(|m| m.len()).unwrap_or(0) > 1024 {
                let _ = fs::remove_file(&tmp);
                db.upsert(&id_key, uid, &bvid, part_no, &title, &out, pubdate)?;
                saved.push(out);
                continue;
            }
        }

        // 3) mp3
        if has_ff {
            let out = name("mp3");
            let mut args: Vec<String> = vec![
                "-y".into(),
                "-i".into(),
                tmp.to_string_lossy().into(),
                "-vn".into(),
                "-c:a".into(),
                "libmp3lame".into(),
                "-b:a".into(),
                format!("{}k", target),
            ];
            args.extend(meta.clone());
            args.push(out.to_string_lossy().into());
            if run_ffmpeg(ff, &args) && fs::metadata(&out).map(|m| m.len()).unwrap_or(0) > 1024 {
                let _ = fs::remove_file(&tmp);
                db.upsert(&id_key, uid, &bvid, part_no, &title, &out, pubdate)?;
                saved.push(out);
                continue;
            }
        }

        // 无 ffmpeg：原样保存（无法写元数据）
        let ext = if is_aac { "m4a" } else { "opus" };
        let out = name(ext);
        fs::rename(&tmp, &out)?;
        db.upsert(&id_key, uid, &bvid, part_no, &title, &out, pubdate)?;
        saved.push(out);
    }

    Ok(DlResult { saved, skipped })
}
