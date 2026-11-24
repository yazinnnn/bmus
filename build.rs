//! Windows-gnu 交叉编译时，rusqlite(bundled) 的 libsqlite3.a 没有被自动加进链接参数，
//! 这里向上找 build 目录里的 libsqlite3-sys/*/out/libsqlite3.a 并显式链接。
fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let out = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap());
    for anc in out.ancestors().take(6) {
        if let Ok(rd) = std::fs::read_dir(anc) {
            let mut dirs: Vec<_> = rd
                .flatten()
                .filter(|e| e.file_name().to_string_lossy().starts_with("libsqlite3-sys"))
                .map(|e| e.path())
                .collect();
            dirs.sort();
            for d in dirs.iter().rev() {
                for sub in std::fs::read_dir(d).into_iter().flatten().flatten() {
                    let lib = sub.path().join("out").join("libsqlite3.a");
                    if lib.exists() {
                        println!("cargo:rustc-link-search=native={}", sub.path().join("out").display());
                        println!("cargo:rustc-link-lib=static=sqlite3");
                        return;
                    }
                }
            }
        }
    }
}
