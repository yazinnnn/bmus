//! 播放后端抽象：优先 libmpv（进程内），不可用时回退到 mpv 子进程 IPC。

use anyhow::{anyhow, Result};

use crate::config::Config;
use crate::mpv::{log_path, Mpv, Snapshot};
use crate::mpv_lib::MpvLib;

pub enum Inner {
    Lib(MpvLib),
    Proc(Mpv),
}

pub struct Player {
    inner: Inner,
}

impl Player {
    pub fn new(cfg: &Config) -> Result<Self> {
        let mut mode = cfg.backend.clone();
        if let Ok(v) = std::env::var("BMUS_BACKEND") {
            if !v.is_empty() {
                mode = v;
            }
        }
        match mode.as_str() {
            "lib" | "libmpv" => Ok(Self {
                inner: Inner::Lib(MpvLib::new(&cfg.cookie(), cfg.volume)?),
            }),
            "proc" | "process" => Ok(Self {
                inner: Inner::Proc(Mpv::spawn(&cfg.mpv_path, &cfg.cookie(), cfg.volume)?),
            }),
            _ => {
                // auto：优先 libmpv，失败退回子进程
                match MpvLib::new(&cfg.cookie(), cfg.volume) {
                    Ok(l) => Ok(Self { inner: Inner::Lib(l) }),
                    Err(e) => {
                        eprintln!("libmpv 不可用（{}），改用 mpv 子进程后端", e);
                        Ok(Self {
                            inner: Inner::Proc(Mpv::spawn(&cfg.mpv_path, &cfg.cookie(), cfg.volume)?),
                        })
                    }
                }
            }
        }
    }

    pub fn kind(&self) -> &'static str {
        match self.inner {
            Inner::Lib(_) => "libmpv",
            Inner::Proc(_) => "mpv 子进程",
        }
    }

    pub fn play_url(&self, url: &str, title: &str, live: bool) {
        match &self.inner {
            Inner::Lib(m) => m.play_url(url, title, live),
            Inner::Proc(m) => m.play_url(url, title, live),
        }
    }

    pub fn toggle_pause(&self) {
        match &self.inner {
            Inner::Lib(m) => m.toggle_pause(),
            Inner::Proc(m) => m.toggle_pause(),
        }
    }

    pub fn seek_relative(&self, delta: f64) {
        match &self.inner {
            Inner::Lib(m) => m.seek_relative(delta),
            Inner::Proc(m) => m.seek_relative(delta),
        }
    }

    pub fn set_loop(&self, on: bool) {
        match &self.inner {
            Inner::Lib(m) => m.set_loop(on),
            Inner::Proc(m) => m.set_loop(on),
        }
    }

    pub fn set_volume(&self, v: u32) {
        match &self.inner {
            Inner::Lib(m) => m.set_volume(v),
            Inner::Proc(m) => m.set_volume(v),
        }
    }

    pub fn stop(&self) {
        match &self.inner {
            Inner::Lib(m) => m.stop(),
            Inner::Proc(m) => m.stop(),
        }
    }

    pub fn set_cookie(&self, c: &str) {
        match &self.inner {
            Inner::Lib(m) => m.set_cookie(c),
            Inner::Proc(m) => m.set_cookie(c),
        }
    }

    pub fn tick(&self) {
        match &self.inner {
            Inner::Lib(m) => m.tick(),
            Inner::Proc(m) => m.tick(),
        }
    }

    pub fn snapshot(&self) -> Snapshot {
        match &self.inner {
            Inner::Lib(m) => m.snapshot(),
            Inner::Proc(m) => m.snapshot(),
        }
    }

    pub fn log_tail(&self, lines: usize) -> String {
        match &self.inner {
            Inner::Lib(m) => m.log_tail(lines),
            Inner::Proc(m) => m.log_tail(lines),
        }
    }

    pub fn log_path(&self) -> std::path::PathBuf {
        match &self.inner {
            Inner::Lib(m) => m.log_path().to_path_buf(),
            Inner::Proc(_) => log_path(),
        }
    }
}

/// 保留给脚本模式：解析后端可用性
#[allow(dead_code)]
pub fn lib_available() -> Result<()> {
    MpvLib::new("", 100).map(|_| ()).map_err(|e| anyhow!(e))
}
