//! mpv 作为音频后端：通过 IPC 控制播放/暂停/seek/音量，
//! 只加载音轨地址（`--no-video`），不请求视频流。
//! IPC 通道：Unix 走 unix socket，Windows 走 127.0.0.1 TCP（mpv 两者都支持）。

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};

#[cfg(unix)]
type IpcStream = std::os::unix::net::UnixStream;
#[cfg(windows)]
type IpcStream = std::net::TcpStream;

#[cfg(unix)]
fn connect_ipc(target: &std::path::Path) -> Result<IpcStream> {
    Ok(IpcStream::connect(target).context("连接 mpv IPC 失败")?)
}

#[cfg(windows)]
fn connect_ipc(target: &std::path::Path) -> Result<IpcStream> {
    Ok(IpcStream::connect(target.to_string_lossy().to_string())
        .context("连接 mpv IPC 失败")?)
}
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub pos: f64,
    pub dur: f64,
    pub paused: bool,
    pub eof: bool,
    pub idle: bool,
    pub loaded: bool,
    pub error: Option<String>,
}

#[derive(Clone, Copy)]
enum Prop {
    Pos,
    Dur,
    Paused,
    Load,
}

pub struct Mpv {
    tx: std::sync::mpsc::Sender<String>,
    snap: Arc<Mutex<Snapshot>>,
    pending: Arc<Mutex<std::collections::HashMap<u64, Prop>>>,
    next_id: AtomicU64,
    child: Arc<Mutex<Child>>,
    /// unix 下是 socket 路径；Windows 走 TCP，不用这个字段
    #[cfg_attr(windows, allow(dead_code))]
    ipc: PathBuf,
    cookie: Mutex<String>,
    log_path: PathBuf,
}

impl Drop for Mpv {
    fn drop(&mut self) {
        let _ = self.command(&["quit"]);
        thread::sleep(Duration::from_millis(120));
        if let Ok(mut c) = self.child.lock() {
            let _ = c.kill();
        }
        #[cfg(unix)]
        let _ = std::fs::remove_file(&self.ipc);
    }
}

impl Mpv {
    pub fn spawn(mpv_path: &str, cookie: &str, volume: u32) -> Result<Self> {
        #[cfg(unix)]
        cleanup_stale_sockets();
        #[cfg(unix)]
        let socket = std::env::temp_dir().join(format!("bmus-mpv-{}.sock", std::process::id()));
        #[cfg(windows)]
        let socket = {
            // 让系统分配一个空闲端口，交给 mpv 去绑定
            let port = std::net::TcpListener::bind("127.0.0.1:0")
                .and_then(|l| l.local_addr())
                .map(|a| a.port())
                .unwrap_or(0);
            if port == 0 {
                bail!("找不到空闲端口给 mpv IPC");
            }
            PathBuf::from(format!("127.0.0.1:{}", port))
        };
        #[cfg(unix)]
        let _ = std::fs::remove_file(&socket);

        let log_path = log_path();
        if let Some(dir) = log_path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let log_file = std::fs::File::create(&log_path).ok();

        let mut cmd = Command::new(mpv_path);
        cmd.arg("--no-video")
            .arg("--no-terminal")
            .arg("--idle=yes")
            .arg("--force-window=no")
            .arg("--keep-open=no")
            .arg("--cache=yes")
            .arg("--cache-secs=30")
            .arg("--demuxer-max-bytes=64MiB")
            .arg(format!("--volume={}", volume))
            .arg(format!("--input-ipc-server={}", socket.display()))
            .arg(format!("--user-agent={}", crate::api::UA))
            .arg("--referrer=https://www.bilibili.com/")
            .arg(format!("--http-header-fields=Cookie: {}", cookie))
            .stdin(Stdio::null())
            .stdout(Stdio::null());
        match log_file {
            Some(f) => {
                cmd.stderr(Stdio::from(f));
            }
            None => {
                cmd.stderr(Stdio::null());
            }
        }

        if let Ok(ao) = std::env::var("BMUS_MPV_AO") {
            if !ao.is_empty() {
                cmd.arg(format!("--ao={}", ao));
            }
        }
        let child = cmd
            .spawn()
            .with_context(|| format!("启动 mpv 失败（{}），请先安装 mpv", mpv_path))?;

        // 等 IPC 就绪（unix：等 socket 文件出现；windows：重试 TCP 连接）
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if Instant::now() > deadline {
                bail!("mpv IPC 未就绪: {}", socket.display());
            }
            if connect_ipc(&socket).is_ok() {
                break;
            }
            thread::sleep(Duration::from_millis(100));
        }
        thread::sleep(Duration::from_millis(100));

        let stream = connect_ipc(&socket)?;
        let read_stream = stream.try_clone()?;
        let write_stream = stream;

        let snap = Arc::new(Mutex::new(Snapshot::default()));
        let pending: Arc<Mutex<std::collections::HashMap<u64, Prop>>> =
            Arc::new(Mutex::new(Default::default()));

        // 读线程：解析响应与事件
        {
            let snap = snap.clone();
            let pending = pending.clone();
            thread::spawn(move || {
                let reader = BufReader::new(read_stream);
                for line in reader.lines() {
                    let line = match line {
                        Ok(l) => l,
                        Err(_) => break,
                    };
                    let v: Value = match serde_json::from_str(&line) {
                        Ok(v) => v,
                        Err(_) => continue,
                    };
                    if let Some(ev) = v["event"].as_str() {
                        match ev {
                            "start-file" => {
                                let mut s = snap.lock().unwrap();
                                s.loaded = false;
                                s.error = None;
                                s.pos = 0.0;
                                s.dur = 0.0;
                            }
                            "file-loaded" => {
                                let mut s = snap.lock().unwrap();
                                s.loaded = true;
                                s.error = None;
                                s.eof = false;
                            }
                            "end-file" => {
                                let mut s = snap.lock().unwrap();
                                s.loaded = false;
                                let bad = v["reason"].as_str() == Some("error")
                                    || !v["file_error"].is_null();
                                if bad {
                                    let msg = v["file_error"]
                                        .as_str()
                                        .or_else(|| v["error"].as_str())
                                        .unwrap_or("播放失败")
                                        .to_string();
                                    s.error = Some(msg);
                                }
                            }
                            _ => {}
                        }
                    }
                    if v["event"].as_str() == Some("property-change") {
                        if v["name"].as_str() == Some("eof-reached") {
                            if let Some(b) = v["data"].as_bool() {
                                snap.lock().unwrap().eof = b;
                            }
                        }
                        continue;
                    }
                    let rid = match v["request_id"].as_u64() {
                        Some(r) => r,
                        None => continue,
                    };
                    let prop = pending.lock().unwrap().remove(&rid);
                    let mut s = snap.lock().unwrap();
                    let ipc_err = v["error"].as_str().unwrap_or("success");
                    if ipc_err != "success" {
                        if let Some(Prop::Load) = prop {
                            s.error = Some(ipc_err.to_string());
                        }
                        continue;
                    }
                    match prop {
                        Some(Prop::Pos) => {
                            if s.loaded {
                                if let Some(n) = v["data"].as_f64() {
                                    s.pos = n;
                                    s.eof = false;
                                }
                            }
                        }
                        Some(Prop::Dur) => {
                            if s.loaded {
                                if let Some(n) = v["data"].as_f64() {
                                    s.dur = n;
                                }
                            }
                        }
                        Some(Prop::Paused) => {
                            if let Some(b) = v["data"].as_bool() {
                                s.paused = b;
                            }
                        }
                        Some(Prop::Load) => {}
                        None => {}
                    }
                }
            });
        }

        // 写线程
        let (tx, rx) = std::sync::mpsc::channel::<String>();
        {
            let mut w = write_stream;
            thread::spawn(move || {
                while let Ok(line) = rx.recv() {
                    if writeln!(w, "{}", line).is_err() {
                        break;
                    }
                    let _ = w.flush();
                }
            });
        }

        let m = Self {
            tx,
            snap,
            pending,
            next_id: AtomicU64::new(1),
            child: Arc::new(Mutex::new(child)),
            ipc: socket,
            cookie: Mutex::new(cookie.to_string()),
            log_path,
        };
        m.command(&["observe_property", "1", "eof-reached"]);
        m.command(&["observe_property", "2", "pause"]);
        Ok(m)
    }

    fn command(&self, args: &[&str]) -> u64 {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let mut cmd: Vec<Value> = Vec::new();
        for a in args {
            cmd.push(json!(a));
        }
        let payload = json!({ "command": cmd, "request_id": id });
        let _ = self.tx.send(payload.to_string());
        id
    }

    fn set_property(&self, name: &str, value: Value) {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let payload = json!({
            "command": [ "set_property", name, value ],
            "request_id": id
        });
        let _ = self.tx.send(payload.to_string());
    }

    fn get_property(&self, name: &str, prop: Prop) {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.pending.lock().unwrap().insert(id, prop);
        let payload = json!({ "command": [ "get_property", name ], "request_id": id });
        let _ = self.tx.send(payload.to_string());
    }

    fn load(&self, url: &str, title: &str, referer: &str) {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.pending.lock().unwrap().insert(id, Prop::Load);
        let payload = json!({
            "command": ["loadfile", url, "replace", {
                "force-media-title": title,
                "http-header-fields": format!("Referer: {}\r\nCookie: {}", referer, self.cookie_hint())
            }],
            "request_id": id
        });
        let _ = self.tx.send(payload.to_string());
        let mut s = self.snap.lock().unwrap();
        s.eof = false;
        s.pos = 0.0;
        s.dur = 0.0;
        s.idle = false;
    }

    fn cookie_hint(&self) -> String {
        self.cookie.lock().unwrap().clone()
    }

    pub fn set_cookie(&self, c: &str) {
        *self.cookie.lock().unwrap() = c.to_string();
    }

    pub fn play_url(&self, url: &str, title: &str, live: bool) {
        self.load(url, title, if live { "https://live.bilibili.com/" } else { "https://www.bilibili.com/" });
        self.set_property("pause", json!(false));
    }

    pub fn toggle_pause(&self) {
        let paused = self.snap.lock().unwrap().paused;
        self.set_property("pause", json!(!paused));
        self.snap.lock().unwrap().paused = !paused;
    }

    pub fn seek_relative(&self, delta: f64) {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let payload = json!({
            "command": ["seek", delta, "relative"],
            "request_id": id
        });
        let _ = self.tx.send(payload.to_string());
    }

    pub fn set_volume(&self, v: u32) {
        self.set_property("volume", json!(v));
    }

    pub fn set_loop(&self, on: bool) {
        self.set_property("loop-file", json!(if on { "inf" } else { "no" }));
    }

    pub fn stop(&self) {
        self.command(&["stop"]);
        let mut s = self.snap.lock().unwrap();
        s.pos = 0.0;
        s.dur = 0.0;
        s.eof = false;
    }

    pub fn log_tail(&self, lines: usize) -> String {
        let content = std::fs::read_to_string(&self.log_path).unwrap_or_default();
        let all: Vec<&str> = content.lines().collect();
        let start = all.len().saturating_sub(lines);
        all[start..].join("\n")
    }

    #[allow(dead_code)]
    pub fn log_path(&self) -> &std::path::Path {
        &self.log_path
    }

    pub fn snapshot(&self) -> Snapshot {
        self.snap.lock().unwrap().clone()
    }

    pub fn tick(&self) {
        self.get_property("time-pos", Prop::Pos);
        self.get_property("duration", Prop::Dur);
        self.get_property("pause", Prop::Paused);
    }

    #[allow(dead_code)]
    pub fn alive(&self) -> bool {
        match self.child.lock() {
            Ok(mut c) => matches!(c.try_wait(), Ok(None)),
            Err(_) => false,
        }
    }
}


pub fn log_path() -> PathBuf {
    dirs::cache_dir()
        .unwrap_or_else(|| std::env::temp_dir())
        .join("bmus")
        .join("mpv.log")
}

/// 清理之前被 kill -9 的会话遗留的 socket
#[cfg(unix)]
fn cleanup_stale_sockets() {
    let dir = std::env::temp_dir();
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        if !name.starts_with("bmus-mpv-") || !name.ends_with(".sock") {
            continue;
        }
        let pid = name
            .trim_start_matches("bmus-mpv-")
            .trim_end_matches(".sock")
            .to_string();
        let alive = std::path::Path::new(&format!("/proc/{}", pid)).exists();
        if !alive {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

/// 一次性播放（不开 TUI），直接交给 mpv 前台
pub fn play_blocking(mpv_path: &str, url: &str, cookie: &str, live: bool) -> Result<()> {
    let mut c = Command::new(mpv_path);
    c.arg("--no-video")
        .arg("--force-window=no")
        .arg(format!("--user-agent={}", crate::api::UA))
        .arg(format!("--referrer={}", if live { "https://live.bilibili.com/" } else { "https://www.bilibili.com/" }))
        .arg(format!("--http-header-fields=Cookie: {}", cookie))
        .arg(url);
    if let Ok(ao) = std::env::var("BMUS_MPV_AO") {
        if !ao.is_empty() {
            c.arg(format!("--ao={}", ao));
        }
    }
    let status = c.status().map_err(|e| anyhow!("启动 mpv 失败: {}", e))?;
    if !status.success() {
        bail!("mpv 退出码 {:?}", status.code());
    }
    Ok(())
}
