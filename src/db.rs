//! 本地 SQLite：UP uid ↔ 首次创建的目录名；已下载记录（用于去重）。

use anyhow::{Context, Result};
use rusqlite::{params, Connection};
use std::path::PathBuf;

pub struct Record {
    pub pubdate: u64,
    pub path: String,
}

pub struct Db {
    conn: Connection,
}

fn db_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("bmus")
        .join("bmus.db")
}

fn legacy_db_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("bili-cli")
        .join("bili.db")
}

fn migrate_legacy_db() {
    let new = db_path();
    if new.exists() {
        return;
    }
    let old = legacy_db_path();
    if !old.exists() {
        return;
    }
    if let Some(dir) = new.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::copy(&old, &new);
}

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

impl Db {
    pub fn open() -> Result<Self> {
        migrate_legacy_db();
        let p = db_path();
        if let Some(d) = p.parent() {
            std::fs::create_dir_all(d).ok();
        }
        let conn = Connection::open(&p).with_context(|| format!("打开 {}", p.display()))?;
        conn.execute_batch(
            "create table if not exists ups(
                uid integer primary key,
                name text not null,
                folder text not null
             );
             create table if not exists downloads(
                id text primary key,
                uid integer,
                bvid text,
                page integer,
                title text,
                path text,
                pubdate integer,
                ts integer
             );",
        )?;
        Ok(Self { conn })
    }

    /// 取该 uid 首次创建时的目录名；没有则按当前昵称新建，重名加后缀
    pub fn folder_for(&self, uid: i64, name: &str) -> Result<String> {
        if uid > 0 {
            let mut st = self
                .conn
                .prepare("select folder from ups where uid = ?1")?;
            if let Ok(f) = st.query_row(params![uid], |r| r.get::<_, String>(0)) {
                return Ok(f);
            }
        }
        let base = {
            let s = sanitize(name);
            if s.is_empty() {
                format!("uid{}", uid)
            } else {
                s
            }
        };
        let mut folder = base.clone();
        let mut n = 2;
        loop {
            let used: Option<String> = self
                .conn
                .query_row(
                    "select folder from ups where folder = ?1 and uid <> ?2",
                    params![folder, uid],
                    |r| r.get(0),
                )
                .ok();
            if used.is_none() {
                break;
            }
            folder = format!("{} ({})", base, n);
            n += 1;
        }
        if uid > 0 {
            self.conn.execute(
                "insert into ups(uid, name, folder) values(?1, ?2, ?3)",
                params![uid, name, folder],
            )?;
        }
        Ok(folder)
    }

    /// 仅记录最新昵称，目录名保持不变
    #[allow(dead_code)]
    pub fn touch_name(&self, uid: i64, name: &str) -> Result<()> {
        if uid <= 0 {
            return Ok(());
        }
        if self
            .conn
            .query_row("select 1 from ups where uid = ?1", params![uid], |_| Ok(()))
            .is_ok()
        {
            self.conn
                .execute("update ups set name = ?2 where uid = ?1", params![uid, name])?;
        }
        Ok(())
    }

    pub fn record(&self, id: &str) -> Option<Record> {
        self.conn
            .query_row(
                "select pubdate, path from downloads where id = ?1",
                params![id],
                |r| {
                    Ok(Record {
                        pubdate: r.get::<_, i64>(0).unwrap_or(0) as u64,
                        path: r.get::<_, String>(1).unwrap_or_default(),
                    })
                },
            )
            .ok()
    }

    #[allow(clippy::too_many_arguments)]
    pub fn upsert(
        &self,
        id: &str,
        uid: i64,
        bvid: &str,
        page: i64,
        title: &str,
        path: &std::path::Path,
        pubdate: u64,
    ) -> Result<()> {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        self.conn.execute(
            "insert into downloads(id, uid, bvid, page, title, path, pubdate, ts)
             values(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             on conflict(id) do update set
               uid=excluded.uid, title=excluded.title, path=excluded.path,
               pubdate=excluded.pubdate, ts=excluded.ts",
            params![
                id,
                uid,
                bvid,
                page,
                title,
                path.to_string_lossy(),
                pubdate as i64,
                ts as i64
            ],
        )?;
        Ok(())
    }
}
