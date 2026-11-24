//! 直接 dlopen libmpv（进程内播放），不需要额外 mpv 进程。

use anyhow::{anyhow, bail, Result};
use libloading::Library;
use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::os::raw::c_double;
use std::path::PathBuf;
use std::sync::Mutex;

use crate::mpv::Snapshot;

const MPV_FORMAT_FLAG: c_int = 3;
const MPV_FORMAT_DOUBLE: c_int = 5;

const EV_LOG_MESSAGE: c_int = 2;
const EV_START_FILE: c_int = 6;
const EV_END_FILE: c_int = 7;
const EV_FILE_LOADED: c_int = 8;

#[repr(C)]
struct MpvEvent {
    event_id: c_int,
    error: c_int,
    reply_userdata: u64,
    data: *mut c_void,
}

#[repr(C)]
struct MpvEventLogMessage {
    prefix: *const c_char,
    level: *const c_char,
    text: *const c_char,
    log_level: c_int,
}

#[repr(C)]
struct MpvEventEndFile {
    reason: c_int,
    error: c_int,
    playlist_entry_id: i64,
    playlist_insert_id: i64,
    playlist_insert_num_entries: c_int,
}

type FnCreate = unsafe extern "C" fn() -> *mut c_void;
type FnInit = unsafe extern "C" fn(*mut c_void) -> c_int;
type FnDestroy = unsafe extern "C" fn(*mut c_void);
type FnSetOptStr = unsafe extern "C" fn(*mut c_void, *const c_char, *const c_char) -> c_int;
type FnSetPropStr = unsafe extern "C" fn(*mut c_void, *const c_char, *const c_char) -> c_int;
type FnGetProp = unsafe extern "C" fn(*mut c_void, *const c_char, c_int, *mut c_void) -> c_int;
type FnCommand = unsafe extern "C" fn(*mut c_void, *const *const c_char) -> c_int;
type FnCommandStr = unsafe extern "C" fn(*mut c_void, *const c_char) -> c_int;
type FnWaitEvent = unsafe extern "C" fn(*mut c_void, c_double) -> *const MpvEvent;
type FnErrStr = unsafe extern "C" fn(c_int) -> *const c_char;
type FnReqLog = unsafe extern "C" fn(*mut c_void, *const c_char) -> c_int;

struct Fns {
    create: FnCreate,
    initialize: FnInit,
    terminate_destroy: FnDestroy,
    set_option_string: FnSetOptStr,
    set_property_string: FnSetPropStr,
    get_property: FnGetProp,
    command: FnCommand,
    command_string: FnCommandStr,
    wait_event: FnWaitEvent,
    error_string: FnErrStr,
    request_log_messages: FnReqLog,
}

pub struct MpvLib {
    _lib: Library,
    f: Fns,
    ctx: *mut c_void,
    snap: Mutex<Snapshot>,
    log_path: PathBuf,
    cookie: Mutex<String>,
}

unsafe impl Send for MpvLib {}

impl Drop for MpvLib {
    fn drop(&mut self) {
        unsafe { (self.f.terminate_destroy)(self.ctx) };
    }
}

fn candidates() -> Vec<String> {
    let mut v: Vec<String> = Vec::new();
    if let Ok(p) = std::env::var("BMUS_LIBMPV") {
        if !p.is_empty() {
            v.push(p);
        }
    }
    #[cfg(target_os = "macos")]
    {
        v.extend(
            ["libmpv.2.dylib", "libmpv.1.dylib", "libmpv.dylib", "/opt/homebrew/lib/libmpv.dylib"]
                .iter()
                .map(|s| s.to_string()),
        );
    }
    #[cfg(target_os = "windows")]
    {
        v.extend(
            ["libmpv-2.dll", "mpv-2.dll", "libmpv.dll"]
                .iter()
                .map(|s| s.to_string()),
        );
    }
    #[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
    {
        v.extend(
            ["libmpv.so.2", "libmpv.so.1", "libmpv.so", "/usr/lib/libmpv.so.2", "/usr/local/lib/libmpv.so"]
                .iter()
                .map(|s| s.to_string()),
        );
    }
    v
}

/// 只探测能否 dlopen 到 libmpv，真正的初始化交给 MpvLib::new
pub fn probe() -> Result<String> {
    let mut last_err = String::new();
    for name in candidates() {
        match unsafe { Library::new(&name) } {
            Ok(_) => return Ok(name),
            Err(e) => last_err = format!("{}: {}", name, e),
        }
    }
    Err(anyhow!("找不到 libmpv（{}）", last_err))
}

impl MpvLib {
    pub fn new(cookie: &str, volume: u32) -> Result<Self> {
        let mut last_err = String::new();
        let mut lib = None;
        for name in candidates() {
            match unsafe { Library::new(&name) } {
                Ok(l) => {
                    lib = Some((name, l));
                    break;
                }
                Err(e) => last_err = format!("{}: {}", name, e),
            }
        }
        let (lib_name, lib) = lib.ok_or_else(|| anyhow!("加载 libmpv 失败（{}）", last_err))?;
        let _ = lib_name;

        let f = unsafe {
            let create: FnCreate = *lib.get(b"mpv_create\0")?;
            let initialize: FnInit = *lib.get(b"mpv_initialize\0")?;
            let terminate_destroy: FnDestroy = *lib.get(b"mpv_terminate_destroy\0")?;
            let set_option_string: FnSetOptStr = *lib.get(b"mpv_set_option_string\0")?;
            let set_property_string: FnSetPropStr = *lib.get(b"mpv_set_property_string\0")?;
            let get_property: FnGetProp = *lib.get(b"mpv_get_property\0")?;
            let command: FnCommand = *lib.get(b"mpv_command\0")?;
            let command_string: FnCommandStr = *lib.get(b"mpv_command_string\0")?;
            let wait_event: FnWaitEvent = *lib.get(b"mpv_wait_event\0")?;
            let error_string: FnErrStr = *lib.get(b"mpv_error_string\0")?;
            let request_log_messages: FnReqLog = *lib.get(b"mpv_request_log_messages\0")?;
            Fns {
                create,
                initialize,
                terminate_destroy,
                set_option_string,
                set_property_string,
                get_property,
                command,
                command_string,
                wait_event,
                error_string,
                request_log_messages,
            }
        };

        let ctx = unsafe { (f.create)() };
        if ctx.is_null() {
            bail!("mpv_create 返回空");
        }
        let me = Self {
            _lib: lib,
            f,
            ctx,
            snap: Mutex::new(Snapshot::default()),
            log_path: crate::mpv::log_path(),
            cookie: Mutex::new(cookie.to_string()),
        };
        if let Some(dir) = me.log_path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::File::create(&me.log_path);

        let vol_s = volume.to_string();
        // 只出声，不要窗口 / 不要抢占终端
        for (k, v) in [
            ("vid", "no"),
            ("audio-display", "no"),
            ("terminal", "no"),
            ("input-terminal", "no"),
            ("input-default-bindings", "no"),
            ("idle", "yes"),
            ("cache", "yes"),
            ("cache-secs", "30"),
            ("demuxer-max-bytes", "64MiB"),
            ("user-agent", crate::api::UA),
            ("referrer", "https://www.bilibili.com/"),
            ("volume", vol_s.as_str()),
        ] {
            let r = me.set_option(k, v);
            if r < 0 && k != "volume" {
                return Err(anyhow!("libmpv 设置选项 {} 失败: {}", k, me.err_str(r)));
            }
        }
        if let Ok(ao) = std::env::var("BMUS_MPV_AO") {
            if !ao.is_empty() {
                let _ = me.set_option("ao", &ao);
            }
        }
        let headers = format!("Referer: https://www.bilibili.com/\r\nCookie: {}", cookie);
        let _ = me.set_option("http-header-fields", &headers);

        let rc = unsafe { (me.f.initialize)(me.ctx) };
        if rc < 0 {
            return Err(anyhow!("mpv_initialize 失败: {}", me.err_str(rc)));
        }
        unsafe {
            let lvl = CString::new("warn").unwrap();
            (me.f.request_log_messages)(me.ctx, lvl.as_ptr());
        }
        Ok(me)
    }

    fn err_str(&self, code: c_int) -> String {
        unsafe {
            let p = (self.f.error_string)(code);
            if p.is_null() {
                format!("error {}", code)
            } else {
                CStr::from_ptr(p).to_string_lossy().to_string()
            }
        }
    }

    fn set_option(&self, name: &str, value: &str) -> c_int {
        let n = CString::new(name).unwrap();
        let v = CString::new(value).unwrap();
        unsafe { (self.f.set_option_string)(self.ctx, n.as_ptr(), v.as_ptr()) }
    }

    fn set_prop_str(&self, name: &str, value: &str) -> c_int {
        let n = CString::new(name).unwrap();
        let v = CString::new(value).unwrap();
        unsafe { (self.f.set_property_string)(self.ctx, n.as_ptr(), v.as_ptr()) }
    }

    fn get_double(&self, name: &str) -> Option<f64> {
        let n = CString::new(name).unwrap();
        let mut d: f64 = 0.0;
        let rc = unsafe {
            (self.f.get_property)(
                self.ctx,
                n.as_ptr(),
                MPV_FORMAT_DOUBLE,
                &mut d as *mut f64 as *mut c_void,
            )
        };
        if rc >= 0 {
            Some(d)
        } else {
            None
        }
    }

    fn get_flag(&self, name: &str) -> Option<bool> {
        let n = CString::new(name).unwrap();
        let mut b: c_int = 0;
        let rc = unsafe {
            (self.f.get_property)(
                self.ctx,
                n.as_ptr(),
                MPV_FORMAT_FLAG,
                &mut b as *mut c_int as *mut c_void,
            )
        };
        if rc >= 0 {
            Some(b != 0)
        } else {
            None
        }
    }

    fn cmd(&self, args: &[&str]) -> c_int {
        let cs: Vec<CString> = args.iter().map(|a| CString::new(*a).unwrap()).collect();
        let mut ptrs: Vec<*const c_char> = cs.iter().map(|c| c.as_ptr()).collect();
        ptrs.push(std::ptr::null());
        unsafe { (self.f.command)(self.ctx, ptrs.as_ptr()) }
    }

    fn push_log(&self, line: &str) {
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.log_path)
        {
            let _ = writeln!(f, "{}", line);
        }
    }

    pub fn set_cookie(&self, c: &str) {
        *self.cookie.lock().unwrap() = c.to_string();
    }

    pub fn log_tail(&self, lines: usize) -> String {
        let content = std::fs::read_to_string(&self.log_path).unwrap_or_default();
        let all: Vec<&str> = content.lines().collect();
        let start = all.len().saturating_sub(lines);
        all[start..].join("\n")
    }

    pub fn log_path(&self) -> &std::path::Path {
        &self.log_path
    }

    pub fn snapshot(&self) -> Snapshot {
        self.snap.lock().unwrap().clone()
    }

    pub fn play_url(&self, url: &str, title: &str, live: bool) {
        let referer = if live {
            "https://live.bilibili.com/"
        } else {
            "https://www.bilibili.com/"
        };
        let headers = format!(
            "Referer: {}\r\nCookie: {}",
            referer,
            self.cookie.lock().unwrap().clone()
        );
        let _ = self.set_prop_str("http-header-fields", &headers);
        let _ = self.set_prop_str("force-media-title", title);
        let rc = self.cmd(&["loadfile", url, "replace"]);
        if rc < 0 {
            let mut s = self.snap.lock().unwrap();
            s.error = Some(self.err_str(rc));
            return;
        }
        {
            let mut s = self.snap.lock().unwrap();
            s.eof = false;
            s.pos = 0.0;
            s.dur = 0.0;
            s.loaded = false;
            s.error = None;
        }
        let _ = self.set_prop_str("pause", "no");
    }

    pub fn toggle_pause(&self) {
        let paused = self.snap.lock().unwrap().paused;
        let _ = self.set_prop_str("pause", if paused { "no" } else { "yes" });
        self.snap.lock().unwrap().paused = !paused;
    }

    pub fn seek_relative(&self, delta: f64) {
        let cmd = CString::new(format!("seek {} relative", delta)).unwrap();
        unsafe {
            let _ = (self.f.command_string)(self.ctx, cmd.as_ptr());
        }
    }

    pub fn set_volume(&self, v: u32) {
        let _ = self.set_prop_str("volume", &v.to_string());
    }

    /// loop-file=inf 实现单曲循环（不依赖 EOF 事件）
    pub fn set_loop(&self, on: bool) {
        let _ = self.set_prop_str("loop-file", if on { "inf" } else { "no" });
    }

    pub fn stop(&self) {
        let _ = self.cmd(&["stop"]);
        let mut s = self.snap.lock().unwrap();
        s.pos = 0.0;
        s.dur = 0.0;
        s.eof = false;
        s.loaded = false;
    }

    pub fn tick(&self) {
        // 1. 排空事件
        loop {
            let ev = unsafe { (self.f.wait_event)(self.ctx, 0.0) };
            if ev.is_null() {
                break;
            }
            let e = unsafe { &*ev };
            if e.event_id == 0 {
                break;
            }
            match e.event_id {
                EV_LOG_MESSAGE => {
                    if !e.data.is_null() {
                        let m = unsafe { &*(e.data as *const MpvEventLogMessage) };
                        let text = unsafe {
                            if m.text.is_null() {
                                String::new()
                            } else {
                                CStr::from_ptr(m.text).to_string_lossy().to_string()
                            }
                        };
                        let level = unsafe {
                            if m.level.is_null() {
                                String::new()
                            } else {
                                CStr::from_ptr(m.level).to_string_lossy().to_string()
                            }
                        };
                        self.push_log(&format!("[{}] {}", level, text.trim_end()));
                    }
                }
                EV_START_FILE => {
                    let mut s = self.snap.lock().unwrap();
                    s.loaded = false;
                    s.error = None;
                    s.pos = 0.0;
                    s.dur = 0.0;
                }
                EV_FILE_LOADED => {
                    let mut s = self.snap.lock().unwrap();
                    s.loaded = true;
                    s.error = None;
                    s.eof = false;
                }
                EV_END_FILE => {
                    let mut s = self.snap.lock().unwrap();
                    s.loaded = false;
                    if !e.data.is_null() {
                        let ef = unsafe { &*(e.data as *const MpvEventEndFile) };
                        // reason: 0 EOF, 1 STOP, 2 QUIT, 3 ERROR, 4 REDIRECT
                        if ef.reason == 3 || ef.error < 0 {
                            s.error = Some(if ef.error < 0 {
                                self.err_str(ef.error)
                            } else {
                                "播放失败".to_string()
                            });
                        }
                    }
                }
                _ => {}
            }
        }

        // 2. 刷新属性
        let pos = self.get_double("time-pos");
        let dur = self.get_double("duration");
        let paused = self.get_flag("pause");
        let eof = self.get_flag("eof-reached");
        let mut s = self.snap.lock().unwrap();
        // 换文件的瞬间属性还是上一条的，未 loaded 时不采信
        if s.loaded {
            if let Some(p) = pos {
                s.pos = p;
                s.eof = false;
            }
            if let Some(d) = dur {
                s.dur = d;
            }
        }
        if let Some(p) = paused {
            s.paused = p;
        }
        if let Some(e) = eof {
            s.eof = e;
        }
    }
}
