use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// 登录凭证（SESSDATA），也可用环境变量 BMUS_SESSDATA 覆盖
    pub sessdata: String,
    pub bili_jct: String,
    pub dede_user_id: String,
    /// 设备指纹，自动获取并缓存，避免被风控
    pub buvid3: String,
    pub buvid4: String,
    /// best | lowest | 具体音轨 id（30216/30232/30280）
    pub audio_quality: String,
    /// 直播默认拉最低画质（省带宽）
    pub live_low: bool,
    pub mpv_path: String,
    pub volume: u32,
    /// auto | lib | proc
    pub backend: String,
    /// 下载目录（支持 ~ 展开）
    pub download_dir: String,
    /// yt-dlp 风格命名模板
    pub filename_template: String,
    pub ffmpeg_path: String,
    pub max_bitrate: u32,
    /// 是否允许在 TUI 里给当前视频/直播发弹幕（:say）。默认关闭，涉及账号风控
    pub allow_send: bool,
    /// 图文动态是否在终端用半块字符渲染缩略图（chafa 风格）
    pub image_preview: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            sessdata: String::new(),
            bili_jct: String::new(),
            dede_user_id: String::new(),
            buvid3: String::new(),
            buvid4: String::new(),
            audio_quality: "best".into(),
            live_low: true,
            mpv_path: "mpv".into(),
            volume: 100,
            backend: "auto".into(),
            download_dir: "~/Music/bili_rec".into(),
            filename_template: "[%(upload_date)s]%(title).150B [%(id)s].%(ext)s".into(),
            ffmpeg_path: "ffmpeg".into(),
            max_bitrate: 192,
            allow_send: true,
            image_preview: true,
        }
    }
}

pub fn config_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("bmus")
        .join("config.toml")
}

/// 旧版本（bili-cli）的配置路径，仅用于首次升级时迁移
fn legacy_config_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("bili-cli")
        .join("config.toml")
}

/// 新版路径不存在而旧版存在时，把旧配置整份复制过来（登录态/下载记录不丢）
fn migrate_legacy() {
    let new = config_path();
    if new.exists() {
        return;
    }
    let old = legacy_config_path();
    if !old.exists() {
        return;
    }
    if let Some(dir) = new.parent() {
        let _ = fs::create_dir_all(dir);
    }
    let _ = fs::copy(&old, &new);
}

/// 数据库（下载去重 + uid→目录 映射）迁移，独立于配置迁移，配置已存在时也要检查
fn migrate_legacy_db() {
    let new = config_path().with_file_name("bmus.db");
    if new.exists() {
        return;
    }
    let old = legacy_config_path().with_file_name("bili.db");
    if !old.exists() {
        return;
    }
    if let Some(dir) = new.parent() {
        let _ = fs::create_dir_all(dir);
    }
    let _ = fs::copy(&old, &new);
}

/// 只追加旧文件里没有的键，已有键（含用户改过的值）一律不动
fn merge_missing(path: PathBuf, cfg: &Config) {
    let raw = fs::read_to_string(&path).unwrap_or_default();
    let full = match toml::Value::try_from(cfg) {
        Ok(v) => v,
        Err(_) => return,
    };
    let defaults = match full.as_table() {
        Some(t) => t,
        None => return,
    };
    let existing = raw
        .parse::<toml::Value>()
        .ok()
        .and_then(|v| v.as_table().cloned())
        .unwrap_or_default();
    let mut appended = String::new();
    for (k, v) in defaults {
        if existing.contains_key(k) {
            continue;
        }
        let lit = match v {
            toml::Value::String(x) => format!("{:?}", x),
            toml::Value::Boolean(b) => b.to_string(),
            toml::Value::Integer(i) => i.to_string(),
            toml::Value::Float(f) => f.to_string(),
            _ => continue,
        };
        appended.push_str(&format!("{} = {}\n", k, lit));
    }
    if appended.is_empty() {
        return;
    }
    let mut out = raw;
    if !out.ends_with('\n') && !out.is_empty() {
        out.push('\n');
    }
    out.push_str(&appended);
    if let Some(dir) = path.parent() {
        let _ = fs::create_dir_all(dir);
    }
    if fs::write(&path, out).is_ok() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&path, fs::Permissions::from_mode(0o600));
        }
    }
}

impl Config {
    pub fn load() -> Self {
        migrate_legacy();
        migrate_legacy_db();
        let mut cfg: Config = fs::read_to_string(config_path())
            .ok()
            .and_then(|s| toml::from_str(&s).ok())
            .unwrap_or_default();
        if let Ok(v) = std::env::var("BMUS_SESSDATA") {
            if !v.is_empty() {
                cfg.sessdata = v;
            }
        }
        if let Ok(v) = std::env::var("BMUS_MPV") {
            if !v.is_empty() {
                cfg.mpv_path = v;
            }
        }
        // 旧默认目录迁移到 ~/Music
        if cfg.download_dir == "~/Musics/bili_rec" {
            cfg.download_dir = "~/Music/bili_rec".into();
            let _ = cfg.save();
        }
        // 版本升级新增字段：把默认值补进旧配置文件，注释与已有值保持原样
        merge_missing(config_path(), &cfg);
        cfg
    }

    pub fn save(&self) -> Result<()> {
        let p = config_path();
        if let Some(dir) = p.parent() {
            fs::create_dir_all(dir).ok();
        }
        let s = toml::to_string_pretty(self)?;
        fs::write(&p, s).with_context(|| format!("写入 {}", p.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&p, fs::Permissions::from_mode(0o600));
        }
        Ok(())
    }

    pub fn cookie(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        if !self.buvid3.is_empty() {
            parts.push(format!("buvid3={}", self.buvid3));
        }
        if !self.buvid4.is_empty() {
            parts.push(format!("buvid4={}", self.buvid4));
        }
        if !self.sessdata.is_empty() {
            parts.push(format!("SESSDATA={}", self.sessdata));
            parts.push(format!("bili_jct={}", self.bili_jct));
            parts.push(format!("DedeUserID={}", self.dede_user_id));
        }
        parts.join("; ")
    }

    pub fn logged_in(&self) -> bool {
        !self.sessdata.is_empty()
    }
}
