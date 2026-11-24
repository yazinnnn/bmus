# bmus

cmus 风格的 bilibili 终端播放器（bmus = bilibili + cmus）：视频、合集、播放列表、收藏夹、动态、直播，**只拉音频**，不下载视频流。

```
┌ bmus ──────────────────────────────────────────────────┐
│▶ [视频] 【直播回放】初次见面的盆栽生活（0 v 0）             │
└────────────────────────────────────────────────────────────┘
┌ 视图 ──────┐┌ UP/合集 [5] (103) ────────────────────────────┐
│ 1. 搜索    ││▶ ♪ 【直播回放】初次见面的盆栽生活…             │
│ 2. 热门    ││  ◉ 直播间·直播中（room 1727071052）            │
│ 3. 收藏    ││  ♪ 上山岗                                      │
│ 4. 直播    ││  ▣ 合集：罗翔说刑法                            │
│ 5. 队列    ││  ☰ 播放列表：直播回放（103）                   │
└────────────┘└───────────────────────────────────────────────┘
│ playing  12:03 / 34:06   vol 100                             │
│ Enter 播放  a 入队  space 暂停  n 下一首  / 搜索  : 命令      │
```

## 名字由来说明

`bmus` = bilibili + cmus，交互模型直接对齐 cmus（视图/队列/`:`命令行/单键控制），所以名字也照着 cmus 起。之前的目录名 `bili-cli` 已改为 `bmus`，配置与数据库目录做了一次性自动迁移。

## 借鉴与致谢

设计思路和若干实现细节参考了下列项目，本项目代码为原创 Rust 实现，未直接复用其代码：

| 来源 | 借鉴了什么 |
|---|---|
| [cmus](https://github.com/cmus/cmus) | 整体交互模型：视图（view）+ 播放队列（queue）+ `:` 命令行 + 单键控制；`j/k`、`space`、`n/p`、`s`、`a`、`d/D` 一套键位映射到相同语义 |
| [mpv](https://mpv.io/) / [libmpv](https://github.com/mpv-player/mpv/blob/master/libmpv/client.h) | 播放内核。两条路径都调它的 C API：`dlopen` + `mpv_create/mpv_set_option_string/mpv_command` 做进程内播放；`--input-ipc-server` + JSON IPC 做子进程播放。`loop-file=inf` 实现单曲循环，`--no-video`/`--vid=no` 丢弃视频 |
| [yt-dlp](https://github.com/yt-dlp/yt-dlp) | 下载文件名模板语法（`%(title).150B`、`%(upload_date)s`、`%(ext)s`）与"音频抽取式下载"的组织方式 |
| [bilibili-API-collect](https://github.com/SocialSisterYi/bilibili-API-collect) | 接口字段与参数说明：`playurl` 的 DASH 音轨结构、`getRoomPlayInfo` 的 stream/format/codec 三级结构、收藏/合集/动态的响应形态 |
| B 站 web 端自身 | `wbi` 签名（`img_url`/`sub_url` 取 key → 固定位置置换表 → `w_rid`）、`buvid3/buvid4` 指纹换取流程，均按 web 端实际行为复刻 |

## 用到的接口

全部是 B 站 web 端公开 JSON 接口（`api.bilibili.com` / `api.live.bilibili.com`），未使用私有签名服务或第三方代理。

| 用途 | 接口 |
|---|---|
| 视频详情、分P、UP、合集归属 | `/x/web-interface/view?bvid=` |
| DASH 播放地址（音轨） | `/x/player/playurl?bvid=&cid=&fnval=16&fourk=1` |
| 搜索（视频 / 直播间 / UP 主） | `/x/web-interface/wbi/search/type?search_type=video\|live_room\|bili_user`（wbi 签名） |
| 热门 | `/x/web-interface/popular` |
| UP 投稿 | `/x/space/wbi/arc/search`（登录后可用，匿名 412） |
| 合集（season） | `/x/polymer/web-space/seasons_archives_list?season_id=&page_size=100` |
| UP 的合集/播放列表目录 | `/x/polymer/web-space/seasons_series_list` |
| 播放列表（series） | `/x/series/archives?mid=&series_id=&ps=100` |
| 收藏夹 | `/x/v3/fav/folder/created/list-all`、`/x/v3/fav/resource/list`、`/x/v3/fav/resource/deal` |
| 点赞 | `/x/web-interface/archive/like` |
| 动态 | `/x/polymer/web-dynamic/v1/feed/all?type=video\|all` |
| 关注 UP 的直播列表 | `/xlive/web-ucenter/v1/xfetter/FeedList`（需登录，每页 10 条，翻页取全） |
| 老版关注直播 feed | `/relation/v1/feed/feed_list`（兜底） |
| 按 UP mid 查直播间 | `/room/v1/Room/getRoomInfoOld?mid=` |
| 直播间状态/标题/流 | `/room/v1/Room/get_info`、`/xlive/web-room/v2/index/getRoomPlayInfo` |
| 设备指纹 | `/x/frontend/finger/spi` |
| 登录二维码 | `/x/passport-login/web/qrcode/generate`、`/qrcode/poll`（凭证在轮询响应的 `Set-Cookie`） |
| 当前用户 | `/x/web-interface/nav` |

## 实现思路

1. **只要音频，就不要视频流。** 普通视频取 `playurl` 的 `dash.audio[]` 里的**独立音轨 URL**（`30216/30232/30280`，约 43/100/203 kbps，fMP4 内 AAC），`ffprobe` 可验证流里只有 `codec_type=audio`，比 DASH 全量下载省 95% 以上带宽。
2. **直播没有纯音频轨。** 直播只有复用流（`flv` / `hls-ts` / `hls-fmp4`），因此走 `getRoomPlayInfo` 取 fmp4-hls 交给 mpv 并禁用视频输出。实测音轨固定 AAC-LC 48kHz ≈130kbps，与 `qn`（画质档）无关，所以默认 `qn=250` 省流不损音质——"高画质换高音质"在 B 站直播里不存在。
3. **播放器抽象。** `player.rs` 统一接口，后端二选一：`mpv_lib.rs`（`libloading` dlopen libmpv，进程内解码，无 IPC）与 `mpv.rs`（子进程 + JSON IPC）。`backend=auto` 先试 libmpv，失败自动降级，所以"只有 mpv、没有 libmpv"的机器也能用；两者都缺则无法播放。
4. **cmus 式状态机。** `tui.rs` 的 `App` 持有多个 `View`（各自 items/sel/offset/page/browse 栈），ratatui 渲染。网络操作全部投递到 worker 线程（`Job`/`JobOut` + `mpsc`），主线程每帧 `try_recv` 应用结果，慢网络与限流不会冻结界面。
5. **翻页与风控。** 搜索/热门/收藏/UP/合集/播放列表/动态都支持 `>` 追加下一页；合集与播放列表每页 100 条尽量一页装完；全局请求间隔 700ms，命中 `-799/-352/-509/-412` 或 HTTP 412 时按 1.5s→3.5s→7s 退避，worker 再等 25s 重试一次。
6. **登录态。** `:login` 拉二维码、用半块字符在终端渲染、轮询结果，凭证从 `Set-Cookie` 解析后写入 `config.toml`（600）。`buvid3/buvid4` 首次运行自动获取，缺失会被判风控。
7. **下载与归档。** 音轨先落临时文件，再按 opus → m4a(AAC) → mp3 顺序用 ffmpeg 转封装/转码（源码率高于 `max_bitrate` 才压到该值），写入 title/artist/date/comment（原始链接含 `?p=N`）/bvid。没有 ffmpeg 时降级为原样保存（无元数据）。多分P一次下全；重下按 `pubdate` 比对跳过。UP 目录名在 SQLite 里与 `uid` 绑定，只取第一次创建的名字，改名不会新建文件夹。
8. **纯 Rust 运行时。** 除 mpv/libmpv 与可选 ffmpeg，HTTP/TLS/JSON/SQLite 全部编进二进制：`reqwest`+`rustls`（不依赖系统 OpenSSL）、`serde_json`、`rusqlite`（bundled）。交叉编译只需处理 `ring` 那一点汇编。

## 依赖

| 依赖 | 必需性 | 说明 |
|---|---|---|
| `libmpv.so` 或 `mpv` 可执行文件 | **至少一个** | 播放内核 |
| `ffmpeg` | 可选 | 下载转封装/转码/写元数据；缺失则保存原始 `m4a`/`opus` |
| Rust ≥ 1.78 | 仅构建 | `cargo build --release` |

后端配置：`auto`（默认，先 libmpv 后 mpv）/ `lib` / `proc`。

安装：

```bash
sudo apt install mpv     # Debian/Ubuntu：同时提供 libmpv.so 与 mpv
sudo pacman -S mpv       # Arch 同上
brew install mpv         # macOS
```

库不在搜索路径时 `BMUS_LIBMPV=/path/libmpv.so`；二进制不在 PATH 时改 `mpv_path`。`bmus doctor` 打印两者可用性。

## 构建

```bash
cargo build --release
install -m755 target/release/bmus ~/.local/bin/bmus
```

产物约 11 MB（含 bundled SQLite）；`ldd` 只能看到 `libc/libm/libgcc_s`——libmpv 是运行时 dlopen，不进 `DT_NEEDED`。

### 交叉编译

平台相关代码只有两处：mpv 子进程 IPC（`cfg(unix)` 走 unix socket，`cfg(windows)` 走 `127.0.0.1` TCP）与 libmpv 的库名列表（`.so` / `.dylib` / `.dll`）。其余逻辑跨平台。

**Windows x86_64（在 Linux 上交叉编译，已验证可产出 exe）**

```bash
sudo apt install mingw-w64
rustup target add x86_64-pc-windows-gnu

# 关键：bundled SQLite 用的是 C 编译，必须显式指定 mingw 的 cc，否则会编出 ELF 目标文件
export CC_x86_64_pc_windows_gnu=x86_64-w64-mingw32-gcc
export AR_x86_64_pc_windows_gnu=x86_64-w64-mingw32-ar

cargo build --release --target x86_64-pc-windows-gnu
# 产物 target/x86_64-pc-windows-gnu/release/bmus.exe（约 16 MB，只依赖系统 DLL）
```

仓库里的 `build.rs` 只在 `target_os = windows` 时生效，负责把 rusqlite 的 `libsqlite3.a` 显式加到链接参数（cargo 在 gnu target 下不会自动带 `-lsqlite3`）。

Windows 运行需要 `mpv.exe` 或 `libmpv-2.dll`（放在 exe 同目录或 PATH）；`ffmpeg.exe` 可选。

**macOS arm64（aarch64-apple-darwin）**

交叉编译需要在 Linux 上装 osxcross（Apple SDK 不能随项目分发，需自行准备），或直接在 Apple Silicon 机器上构建：

```bash
rustup target add aarch64-apple-darwin
cargo build --release --target aarch64-apple-darwin   # 在 macOS 上
# 或 Linux + osxcross：
# export CC_aarch64_apple_darwin=aarch64-apple-darwin21-clang
# export CARGO_TARGET_AARCH64_APPLE_DARWIN_LINKER=aarch64-apple-darwin21-clang
brew install mpv                                       # 运行依赖（libmpv.dylib 或 mpv 二进制）
```

代码已按 macOS 适配：libmpv 候选名含 `libmpv.2.dylib` / `libmpv.1.dylib` / `/opt/homebrew/lib/libmpv.dylib`，IPC 走 unix socket。本仓库的 CI 环境没有 Apple SDK，因此不提供预编译的 mac 二进制。

目标机只需 `mpv` 或 `libmpv` 之一，TLS/JSON/SQLite 已在二进制内。

已知构建坑：Windows 交叉编译若出现 `sqlite3_*` undefined reference，说明 bundled SQLite 用宿主编译器编成了 ELF，检查 `CC_x86_64_pc_windows_gnu` 是否指向 mingw-gcc 并 `cargo clean -p libsqlite3-sys` 重来。

## 命令行用法

```bash
bmus                             # 打开 TUI（等同 bmus tui）
bmus play BV1GJ411x7h7           # 直接播放（前台 mpv，Ctrl-C 退出）
bmus play "周杰伦"                # 非 BV/av/链接时按关键词搜第一条
bmus play BV1xx --low            # 最低音轨（更省流）
bmus url BV1GJ411x7h7            # 只输出纯音频直链
mpv --no-video "$(bmus url BV1GJ411x7h7 2>/dev/null)"

bmus search "rust 教程" --kind video --page 1     # --json 输出
bmus search "游戏" --kind live_room
bmus popular --limit 20
bmus live 6 --url-only                            # 打印直播流地址
bmus fav --folders / --id 123456                  # 收藏夹
bmus info BV1d7wAzsE8V                            # 分P/发布时间
bmus dl BV1d7wAzsE8V                              # 下载到 ~/Music/bili_rec/<UP>/
bmus dl BV1d7wAzsE8V -p 3 --dir /tmp/music        # 只下第 3 个分P
bmus dl BV1d7wAzsE8V --force                       # 忽略去重记录

bmus login / logout / status                      # 登录 / 登出 / 查看登录态
bmus doctor                                       # 播放后端与依赖自检
bmus dynlives [--raw]                             # 关注 UP 的直播列表 / 原始 JSON
bmus livetest <room> --secs 15                    # 逐档实测直播音频码率
```

环境变量：`BMUS_BACKEND=lib|proc`、`BMUS_LIBMPV=/path/libmpv.so`、`BMUS_MPV_AO=null`（无声调试）、`BMUS_SESSDATA`。

## TUI 快捷键

| 键 | 作用 | cmus 对应 |
|---|---|---|
| `j/k` `↑/↓` | 上下移动 | 同 |
| `Enter` / `x` | 播放 / 进入（合集、收藏夹、UP、直播间） | `Enter` |
| `a` | 加入播放队列 | `a` |
| `d` / `D` | 删除选中 / 清空队列 | 同 |
| `space` | 暂停/继续 | 同 |
| `s` | 停止 | 同 |
| `n` / `p` | 下一首（下一分P→队列→当前视图下一项）/ 重播 | 同 |
| `h/l` `←/→` | ±10s | seek |
| `,` / `.` | ±60s | — |
| `[` / `]` | 上/下一个分P | — |
| `>` | 当前视图翻下一页 | — |
| `w` | 下载选中视频全部分P（后台转码） | — |
| `R` | 播放模式：关闭→单曲→列表循环→列表随机（作用于当前视图） | — |
| `+` / `-` | 音量 ±5 | 同 |
| `L` | 省流（最低音轨 / 直播 qn=250） | — |
| `c` / `C` | 收藏 / 取消收藏（默认收藏夹） | — |
| `v` | 点赞 | — |
| `Esc` | 返回上一层浏览 | — |
| `1`–`7` / `Tab` | 搜索/热门/收藏/直播/队列/UP·合集/动态 | 视图切换 |
| `/` `:` `r` `?` `q` | 搜索 / 命令行 / 刷新 / 帮助 / 退出 | `/` `:` |

命令行示例：

```
:play BV1GJ411x7h7        立即播放
:add 周杰伦                搜索并加入队列
:search 关键词
:live 12345               播放直播间
:fav / :fav 123456        收藏夹列表 / 载入指定收藏夹
:up 1891335475            看 UP 投稿 + 直播间入口 + 合集 + 播放列表
:seasons 546195           看该 UP 的合集 + 播放列表
:season BV18fhb65EGs      看某视频所属合集的全部剧集
:dynamic                  重新拉动态（直播列表置顶）
:dl / :star / :unstar / :like
:login / :logout
:vol 80   :clear   :q
```

列表前缀：`♪` 视频、`◉` 直播、`▤` 收藏夹、`▣` 合集、`☰` 播放列表、`◍` UP 主。

## 配置

路径 `~/.config/bmus/config.toml`（600）。从旧名 `bili-cli` 升级时会自动迁移：新版配置/数据库不存在而旧目录存在时，直接复制 `~/.config/bili-cli/config.toml` 与 `bili.db`，登录态和下载去重记录不丢。

```toml
sessdata = ""          # 登录凭证
bili_jct = ""
dede_user_id = ""
buvid3 = ""            # 设备指纹，首次运行自动获取
buvid4 = ""
audio_quality = "best" # best | lowest | 30216/30232/30280
live_low = true        # 直播取 qn=250（音质与高档一致，仅省带宽）
mpv_path = "mpv"
volume = 100
backend = "auto"       # auto | lib | proc
download_dir = "~/Music/bili_rec"      # 实际落在 <up主>/ 下
filename_template = "[%(upload_date)s]%(title).150B [%(id)s].%(ext)s"
ffmpeg_path = "ffmpeg"
max_bitrate = 192
```

模板字段：`%(upload_date)s`、`%(title)s`、`%(title).150B`（按**字节**截断，中文 3 字节/字）、`%(id)s`、`%(ext)s`。渲染后做文件名净化（`/ \ : * ? " < > |` 与控制字符 → `_`），整段名字超 240 字节在扩展名前截断，避免撞 `NAME_MAX=255`。

数据库 `~/.config/bmus/bmus.db`：

```sql
ups(uid, name, folder)   -- uid → 首次创建的目录名（UP 改名不换目录）
downloads(id, uid, bvid, page, title, path, pubdate, ts)  -- 去重依据 pubdate
```

## 已知限制

- 直播没有纯音频轨：音轨固定 ~130kbps AAC，选更高画质档不会提升音质。
- 视频音源本身有损（AAC ≤203kbps / Opus ~100kbps），没有 FLAC 源，转 FLAC 只是体积膨胀。
- 未登录：搜索可用，但 `space/wbi/arc/search` 会 412；动态与直播关注列表必须登录。
- 同 IP 高频请求会触发 `-799/-412`，程序会退避重试，仍失败需等待或换网络。
- 点赞只支持"赞"，无取消赞；收藏固定写入默认收藏夹。
- 不支持弹幕、评论、投币、下载视频画面。
- 只发布 Linux 与 Windows 预编译产物；macOS 需自行构建（缺 Apple SDK）。
#（注：内容由AI生成）
