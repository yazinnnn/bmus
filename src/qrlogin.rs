//! 扫码登录：`bmus login`

use anyhow::{anyhow, bail, Result};
use crossterm::event::{self, Event, KeyCode, KeyModifiers};
use serde_json::Value;
use std::thread;
use std::time::{Duration, Instant};

use crate::config::Config;

fn build_client() -> Result<reqwest::blocking::Client> {
    Ok(reqwest::blocking::Client::builder()
        .user_agent(crate::api::UA)
        .timeout(Duration::from_secs(10))
        .build()?)
}

fn query_param(url: &str, key: &str) -> String {
    let q = url.split('?').nth(1).unwrap_or("");
    for pair in q.split('&') {
        let mut it = pair.splitn(2, '=');
        let k = it.next().unwrap_or("");
        let v = it.next().unwrap_or("");
        if k == key {
            return urlencoding::decode(v)
                .map(|s| s.into_owned())
                .unwrap_or_else(|_| v.to_string());
        }
    }
    String::new()
}

fn cookie_from(headers: &reqwest::header::HeaderMap, key: &str) -> String {
    for hv in headers.get_all("set-cookie").iter() {
        if let Ok(s) = hv.to_str() {
            for seg in s.split(';') {
                let seg = seg.trim();
                if let Some(rest) = seg.strip_prefix(&format!("{}=", key)) {
                    if !rest.is_empty() {
                        return rest.to_string();
                    }
                }
            }
        }
    }
    String::new()
}

pub fn login(cfg: &mut Config) -> Result<()> {
    let http = build_client()?;
    let gen: Value = http
        .get("https://passport.bilibili.com/x/passport-login/web/qrcode/generate")
        .header("Referer", "https://www.bilibili.com/")
        .send()?
        .json()?;
    if gen["code"].as_i64() != Some(0) {
        bail!("获取登录二维码失败: {}", gen["message"]);
    }
    let url = gen["data"]["url"].as_str().unwrap_or("").to_string();
    let key = gen["data"]["qrcode_key"].as_str().unwrap_or("").to_string();

    let code = qrcode::QrCode::new(url.as_bytes())?;
    let art = code
        .render::<qrcode::render::unicode::Dense1x2>()
        .quiet_zone(true)
        .build();
    println!("\n用 B 站手机 App 扫描二维码登录：\n");
    println!("{}", art);
    println!("（{} 内有效，Esc/Ctrl-C 取消）\n", "180 秒");

    let start = Instant::now();
    let mut last_code = -1;
    loop {
        // 终端里按 Esc / Ctrl-C 取消（TUI 会恢复到原界面）
        if let Ok(true) = event::poll(Duration::from_millis(0)) {
            if let Ok(Event::Key(k)) = event::read() {
                if matches!(k.code, KeyCode::Esc)
                    || (k.code == KeyCode::Char('c') && k.modifiers.contains(KeyModifiers::CONTROL))
                {
                    bail!("已取消登录");
                }
            }
        }
        if start.elapsed() > Duration::from_secs(180) {
            bail!("二维码已过期，请重新执行 `bmus login`");
        }
        let resp = http
            .get(format!(
                "https://passport.bilibili.com/x/passport-login/web/qrcode/poll?qrcode_key={}",
                key
            ))
            .send()?;
        // 真正的登录 cookie 在 Set-Cookie 头里，data.url 常常是空的
        let sess_hdr = cookie_from(resp.headers(), "SESSDATA");
        let jct_hdr = cookie_from(resp.headers(), "bili_jct");
        let uid_hdr = cookie_from(resp.headers(), "DedeUserID");
        let v: Value = resp.json()?;
        let d = &v["data"];
        let c = d["code"].as_i64().unwrap_or(-1);
        if c != last_code {
            match c {
                86101 => println!("等待扫码…"),
                86090 => println!("已扫码，请在手机上确认登录"),
                86038 => bail!("二维码已失效"),
                _ => {}
            }
            last_code = c;
        }
        if c == 0 {
            let redirect = d["url"].as_str().unwrap_or("");
            let sess = if !sess_hdr.is_empty() {
                sess_hdr.clone()
            } else {
                query_param(redirect, "SESSDATA")
            };
            if sess.is_empty() {
                return Err(anyhow!("登录成功但没解析到 SESSDATA，请手动填写配置"));
            }
            cfg.sessdata = sess;
            cfg.bili_jct = if !jct_hdr.is_empty() { jct_hdr } else { query_param(redirect, "bili_jct") };
            cfg.dede_user_id = if !uid_hdr.is_empty() { uid_hdr } else { query_param(redirect, "DedeUserID") };
            cfg.save()?;

            // 校验身份
            let me: Value = http
                .get("https://api.bilibili.com/x/web-interface/nav")
                .header("Referer", "https://www.bilibili.com/")
                .header("Cookie", cfg.cookie())
                .send()?
                .json()?;
            println!(
                "登录成功：{}（配置已保存到 {}）",
                me["data"]["uname"].as_str().unwrap_or("已登录"),
                crate::config::config_path().display()
            );
            return Ok(());
        }
        thread::sleep(Duration::from_secs(2));
    }
}
