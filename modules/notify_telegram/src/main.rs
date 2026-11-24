//! Telegram 通知后处理模块 / Telegram Notification Post-processing Module
//!
//! 通过 MTProto 协议将录制信息、封面图和视频文件发送到 Telegram 频道或群组。
//! 支持超过 2GB 的大文件（自动分割）、HTTP/SOCKS5 代理，以及多次重连重试。
//!
//! 支持三种登录方式（参数 `account`，默认 `auto`）：
//! - `auto`：**已登录的用户会话优先**，没有用户会话时才用机器人账号。
//! - `bot`：机器人账号（Bot Token）。机器人**无法**调用 `messages.getForumTopics`
//!   （会返回 `BOT_METHOD_INVALID`），因此话题只能依赖本地缓存。
//! - `user`：用户账号（通过 `--login` 交互式登录生成会话文件）。可以列出群组话题，
//!   因此能可靠复用同名话题；高级会员还可上传 4GB 单文件。
//!
//! 话题（Topic）只存在于**已开启话题的超级群组**，机器人还需要「管理话题」管理员权限：
//! - 普通群没有话题：在群设置里开启「话题」后 Telegram 会把群升级为超级群组，
//!   群 ID 也从 `-1234567890` 变成 `-100…`，需要把新 ID 填到 `chat_id`。
//! - 私有群组没有 username：`username` 留空，只填 `chat_id`。
//! - 关闭 `topic_per_model` 即可直接发到群的主聊天（普通群同样可用）。
//!
//! Sends recording info, cover image, and video files to a Telegram channel or group
//! via the MTProto protocol. Supports files over 2GB (auto-split), HTTP/SOCKS5 proxy,
//! and multiple reconnect retries.
//!
//! Three login modes (the `account` parameter, default `auto`):
//! - `auto`: **a signed-in user session wins**; the bot account is used only when no user
//!   session exists.
//! - `bot`: bot account (Bot Token). Bots **cannot** call `messages.getForumTopics`
//!   (it fails with `BOT_METHOD_INVALID`), so topics rely on the local cache.
//! - `user`: user account (session file created by the interactive `--login` mode).
//!   Can list group topics, so existing topics are reliably reused; premium accounts
//!   can also upload single files up to 4GB.
//!
//! Forum topics only exist in **supergroups with Topics enabled**, and the bot needs the
//! "Manage Topics" admin right:
//! - A basic group has no topics. Enabling Topics upgrades the group to a supergroup and its
//!   id changes from `-1234567890` to a `-100…` form — use that new id as `chat_id`.
//! - Private groups have no username: leave `username` empty and use `chat_id`.
//! - Turning `topic_per_model` off posts straight into the group's main chat (works for basic
//!   groups too).
//!
//! # 协议 / Protocol
//! - `--describe`: 输出 JSON 格式的模块元数据 / Output module metadata as JSON
//! - `--login`: 交互式登录（stdin/stdout 行协议），生成用户会话文件
//!   Interactive login (line protocol over stdin/stdout) that writes the user session file
//! - `--refresh`: 刷新已授权会话（认证密钥/peer 缓存），不需要输入视频
//!   Refresh an authorized session (auth key / peer cache); no input video needed
//! - 环境变量 `PP_INPUT`: 输入视频文件路径 / Input video file path via env var
//! - 环境变量 `PP_TEST=1`: 自检模式（无需 `PP_INPUT`）/ Self-test mode (no `PP_INPUT` required)
//! - 标准输出 `OUTPUT:{path}`: 成功后输出视频路径 / Output video path on success
//! - 标准输出 `PROGRESS:{done}/{total}`: 进度上报 / Progress reporting
//! - 标准输出 `STATUS:{speed}`: 上传速度上报 / Upload speed reporting
//!
//! # 登录行协议 / Login line protocol
//! stdin 每行 `命令:值`（`phone:` / `code:` / `password:` / `quit`），
//! stdout 以 `LOGIN_STATE:` 开头的行表示状态：
//! `ready`、`code_sent`、`password_required:{hint}`、`invalid_code`、
//! `invalid_password`、`done:{name}:{premium}`、`already:{name}:{premium}`、`error:{msg}`。
//!
//! Each stdin line is `command:value` (`phone:` / `code:` / `password:` / `quit`);
//! stdout lines starting with `LOGIN_STATE:` report the state.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::{AtomicI32, AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use futures::stream::{FuturesUnordered, StreamExt};
use grammers_client::Client;
use grammers_client::InvocationError;
use grammers_client::client::ClientConfiguration;
use grammers_client::media::{InputMedia, Uploaded};
use grammers_client::message::InputMessage;
use grammers_client::peer::Peer;
use grammers_client::sender::{ConnectionParams, SenderPool};
use grammers_client::tl;
use grammers_session::storages::SqliteSession;
use grammers_session::types::{PeerAuth, PeerId, PeerKind, PeerRef};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncSeekExt, BufReader, SeekFrom};
use pp_utils::{param, param_bool, format_duration, format_bytes, format_speed, parse_stem, tmp_dir, image_dimensions, video_meta, extract_video_frame};

/// 进度上报的缩放基数 / Progress reporting scale base
const PROGRESS_SCALE: usize = 10_000;

/// 模块元数据 JSON，通过 `--describe` 参数输出。
/// Module metadata JSON, output via `--describe` argument.
const DESCRIBE: &str = r#"{
    "id": "notify_telegram",
    "name": "Telegram Notification 0.6.0",
    "description": "Send finished videos to Telegram via MTProto (files over 50 MB and HTTP/SOCKS5 proxies supported). Posts the video only by default — no text at all; the text caption and the cover image each have their own switch. Can create/reuse one group forum topic per model and post the video into it. Uses your signed-in account by default: it can list existing topics (no duplicates) and files over 2 GB are sent as numbered video parts. Supports multiple user sessions: when the current account is upload-throttled, the next signed-in account is used and the file is re-uploaded",
    "testable": true,
    "loginable": true,
    "params": [
        {
        "key": "account",
        "label": "Login mode (auto = your signed-in account wins / user = your account only / bot = bot account only)",
        "type": "select",
        "default": "auto",
        "options": ["auto", "user", "bot"]
        },
        {
        "key": "session_label",
        "label": "Current session label (blank = primary {api_id}_user.db; enter a label such as backup1 when signing in another account)",
        "type": "string",
        "default": ""
        },
        {
        "key": "fallback_sessions",
        "label": "Backup session labels (comma separated, e.g. backup1,backup2. On FLOOD_WAIT / FLOOD_PREMIUM_WAIT the next signed-in account is tried and the whole file is re-uploaded)",
        "type": "string",
        "default": ""
        },
        {
        "key": "api_id",
        "label": "API ID (from my.telegram.org)",
        "type": "string",
        "default": ""
        },
        {
        "key": "api_hash",
        "label": "API Hash (bot accounts only — leave empty for a user account)",
        "type": "string",
        "default": ""
        },
        {
        "key": "bot_token",
        "label": "Bot Token (from @BotFather, bot accounts only)",
        "type": "string",
        "default": ""
        },
        {
        "key": "chat_id",
        "label": "Chat ID (groups and supergroups: -100xxxxxxxxxx; a private group has no username, so the id is the only way to target it)",
        "type": "string",
        "default": ""
        },
        {
        "key": "username",
        "label": "Group username (optional — public groups only, e.g. mygroupname, without @; leave empty for private groups and use Chat ID)",
        "type": "string",
        "default": ""
        },
        {
        "key": "proxy",
        "label": "Proxy (http:// or socks5://)",
        "type": "string",
        "default": ""
        },
        {
        "key": "send_video",
        "label": "Also send video file",
        "type": "boolean",
        "default": true
        },
        {
        "key": "send_caption",
        "label": "Attach a text caption (model name, timestamp, duration, file name, size). With this off the message carries no text at all — just the video",
        "type": "boolean",
        "default": false
        },
        {
        "key": "send_cover",
        "label": "Also send the cover image (a frame extracted from the video). With this off the album holds the video only",
        "type": "boolean",
        "default": true
        },
        {
        "key": "cover_first",
        "label": "Upload the cover first; once the video upload finishes, post the cover as its own message before the video. With this off the cover and the video go out together as one album message",
        "type": "boolean",
        "default": true
        },
        {
        "key": "topic_per_model",
        "label": "Create/reuse a group topic per model and upload into it",
        "type": "boolean",
        "default": true
        },
        {
        "key": "delete_after_send",
        "label": "Delete local data after a successful send (video, segments, metadata and module side files) to free server space",
        "type": "boolean",
        "default": true
        },
        {
        "key": "batch_size",
        "label": "Maximum simultaneous video uploads in a batch (default 3, range 1–10). Remaining videos wait in the queue.",
        "type": "number",
        "default": 3
        },
        {
        "key": "upload_workers",
        "label": "Concurrent upload parts (default 16, max 16; 0 = default). Serial uploads wait for the server to acknowledge every 512KB part; running several at once overlaps those round trips — a 12MB video measured ~1.8x faster (the uplink itself is the ceiling)",
        "type": "number",
        "default": 16
        },
        {
        "key": "upload_engine",
        "label": "Upload engine (wzgram = fast WZGram uploader / grammers = built-in Rust uploader). WZGram reuses the signed-in session and falls back to Rust on failure",
        "type": "select",
        "default": "wzgram",
        "options": ["wzgram", "grammers"]
        }
    ]
}"#;

/// 若封面图不满足 Telegram 限制（宽+高 < 10000 且宽高比 < 20:1），则等比缩放。
/// Resize cover image if it violates Telegram limits (w+h < 10000 and aspect ratio < 20:1).
/// Returns Some(new_path) if resized, None if no resize needed.
fn resize_cover_for_telegram(img: &Path) -> Result<Option<PathBuf>, String> {
    const MAX_PHOTO_BYTES: u64 = 10 * 1024 * 1024; // Telegram photo limit: 10 MB

    let (w, h) = match image_dimensions(img) {
        Some(d) => d,
        None => return Ok(None), // 无法获取尺寸，跳过
    };

    let file_size = fs::metadata(img).map(|m| m.len()).unwrap_or(0);

    // 检查是否需要缩放
    let sum_ok = (w + h) < 10000;
    let ratio_ok = w.max(h) < h.min(w).saturating_mul(20);
    let size_ok = file_size < MAX_PHOTO_BYTES;
    if sum_ok && ratio_ok && size_ok {
        return Ok(None);
    }

    // 计算目标尺寸：同时满足两个约束
    // 约束1: w' + h' < 10000  => scale <= 9999 / (w+h)
    // 约束2: max(w',h') / min(w',h') < 20  => 宽高比不变，只要原始比例 < 20:1 就满足
    //        若原始比例 >= 20:1，则将长边限制为短边的 19 倍
    let (tw, th) = if !ratio_ok {
        // 先修正宽高比：将长边缩到短边的 19 倍
        if w >= h {
            (h * 19, h)
        } else {
            (w, w * 19)
        }
    } else {
        (w, h)
    };

    // 再检查 sum 约束
    let (tw, th) = if tw + th >= 10000 {
        // 等比缩放使 w'+h' = 9999
        let scale = 9999.0 / (tw + th) as f64;
        let nw = ((tw as f64 * scale).floor() as u32).max(1);
        let nh = ((th as f64 * scale).floor() as u32).max(1);
        (nw, nh)
    } else {
        (tw, th)
    };

    let stem = img.file_stem().and_then(|s| s.to_str()).unwrap_or("cover");
    let out_path = tmp_dir().join(format!("{}_tg_resized.jpg", stem));

    // 若文件超过 10MB，逐步降低质量直到满足大小限制
    // If file exceeds 10MB, progressively lower quality until size limit is met
    let q_values: &[&str] = if !size_ok { &["5", "10", "15", "20", "25", "31"] } else { &["2"] };
    let mut success = false;
    for &q in q_values {
        let status = Command::new("ffmpeg")
            .args(["-y", "-i"]).arg(img)
            .args(["-vf", &format!("scale={}:{}", tw, th), "-q:v", q])
            .arg(&out_path)
            .stdout(Stdio::null()).stderr(Stdio::null())
            .status().map_err(|e| format!("ffmpeg not found: {}", e))?;

        if !status.success() {
            return Err("ffmpeg failed to resize cover image for Telegram".to_string());
        }

        let out_size = fs::metadata(&out_path).map(|m| m.len()).unwrap_or(u64::MAX);
        if out_size < MAX_PHOTO_BYTES {
            success = true;
            break;
        }
    }

    if !success {
        return Err("cover image exceeds Telegram 10MB photo limit even after compression".to_string());
    }

    Ok(Some(out_path))
}

/// 使用 ffmpeg 从视频中提取第一帧作为缩略图（用于 Telegram 视频消息的预览图）。
/// Extract the first frame from a video as a thumbnail using ffmpeg
/// (used as preview image for Telegram video messages).
///
/// # 返回值 / Returns
/// 缩略图文件路径，失败时返回错误。
/// Thumbnail file path, or error on failure.
/// 提取视频缩略图（用于上传视频消息内嵌的 thumb）。
/// Telegram MTProto 要求缩略图为 JPEG 格式且建议 320x320 尺寸。
///
/// Extract video thumbnail (used as embedded thumb for InputMediaUploadedDocument).
/// Telegram MTProto requires the thumbnail to be JPEG and ideally <= 320x320.
fn extract_video_thumbnail(input: &Path, directory: &Path) -> Result<PathBuf, String> {
    let stem = input.file_stem().and_then(|s| s.to_str()).unwrap_or("recording");
    let thumb_path = directory.join(format!("{}.tg_thumb.jpg", stem));
    let dur = pp_utils::video_duration(input).unwrap_or(0.0);
    let ss = if dur > 10.0 { "5" } else if dur > 2.0 { "1" } else { "0" };
    let status = Command::new("ffmpeg")
        .args(["-y", "-ss", ss, "-i"])
        .arg(input)
        .args([
            "-frames:v", "1",
            "-vf", "scale='if(gt(iw,ih),320,-1)':'if(gt(iw,ih),-1,320)'",
            "-q:v", "2",
        ])
        .arg(&thumb_path)
        .stdout(Stdio::null()).stderr(Stdio::null())
        .status()
        .map_err(|e| format!("ffmpeg not found: {}", e))?;
    if !status.success() || !thumb_path.exists() {
        return Err("ffmpeg failed to extract video thumbnail".to_string());
    }
    Ok(thumb_path)
}

/// Maximum size of each video part, in bytes.
const VIDEO_PART_LIMIT: u64 = 2_000_000_000;
static UPLOAD_WORKSPACE_ID: AtomicUsize = AtomicUsize::new(0);

struct UploadWorkspace(PathBuf);

impl UploadWorkspace {
    fn new() -> Result<Self, String> {
        loop {
            let id = UPLOAD_WORKSPACE_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let path = tmp_dir().join(format!("telegram-parts-{}-{id}", std::process::id()));
            match fs::create_dir(&path) {
                Ok(()) => return Ok(Self(path)),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(format!("Could not create upload workspace: {error}")),
            }
        }
    }
}

impl Drop for UploadWorkspace {
    fn drop(&mut self) {
        if let Err(error) = fs::remove_dir_all(&self.0) {
            eprintln!("Could not clean up Telegram upload workspace: {error}");
        }
    }
}

fn split_video(input: &Path, max_bytes: u64, workspace: &Path) -> Result<Vec<PathBuf>, String> {
    let size = fs::metadata(input).map_err(|e| format!("Could not read video size: {e}"))?.len();
    if size <= max_bytes {
        return Ok(vec![input.to_path_buf()]);
    }
    let duration = pp_utils::video_duration(input)
        .filter(|duration| duration.is_finite() && *duration > 0.0)
        .ok_or("Cannot split video because its duration is unavailable")?;
    let stem = input.file_stem().and_then(|s| s.to_str()).unwrap_or("video");
    let ext = input.extension().and_then(|s| s.to_str()).unwrap_or("mp4");
    let mut seconds = (duration * max_bytes as f64 / size as f64 * 0.85).max(1.0);
    for attempt in 0..4 {
        let dir = workspace.join(format!("split-{attempt}"));
        fs::create_dir(&dir).map_err(|e| format!("Could not create split directory: {e}"))?;
        let pattern = dir.join(format!("{}_part%06d.{ext}", stem.replace('%', "%%")));
        let mut cmd = Command::new("ffmpeg");
        cmd.args(["-nostdin", "-v", "error", "-i"]).arg(input);
        if attempt < 2 {
            cmd.args(["-c", "copy"]);
        } else {
            // Sparse keyframes can make lossless segments exceed the cap.
            cmd.args(["-c:v", "libx264", "-preset", "veryfast", "-crf", "23", "-c:a", "aac",
                "-force_key_frames", &format!("expr:gte(t,n_forced*{seconds})")]);
        }
        let output = cmd.args(["-f", "segment", "-segment_time", &seconds.to_string(),
                "-segment_start_number", "1", "-reset_timestamps", "1"])
            .arg(pattern).output().map_err(|e| format!("Could not start FFmpeg: {e}"))?;
        if !output.status.success() {
            return Err(format!("FFmpeg could not split the video: {}", String::from_utf8_lossy(&output.stderr).trim()));
        }

        let mut parts = Vec::new();
        let mut largest = 0;
        for entry in fs::read_dir(&dir).map_err(|e| format!("Could not list video parts: {e}"))? {
            let entry = entry.map_err(|e| format!("Could not read video part: {e}"))?;
            let bytes = entry.metadata().map_err(|e| format!("Could not read part size: {e}"))?.len();
            if bytes == 0 {
                return Err("FFmpeg produced an empty video part".into());
            }
            largest = largest.max(bytes);
            parts.push(entry.path());
        }
        if parts.is_empty() {
            return Err("FFmpeg produced no video parts".into());
        }
        if largest <= max_bytes {
            parts.sort();
            println!("Prepared {} video part(s), each at most {} bytes", parts.len(), max_bytes);
            return Ok(parts);
        }
        fs::remove_dir_all(&dir).map_err(|e| format!("Could not remove oversized parts: {e}"))?;
        seconds = (seconds * (max_bytes as f64 / largest as f64 * 0.8).min(0.65)).max(0.5);
    }
    Err("Could not split the video into parts of 2 GB or less".into())
}

/// Normalize the video format before checking the actual upload size.
fn prepare_video_parts(input: &Path, workspace: &Path) -> Result<Vec<PathBuf>, String> {
    let ext = input.extension().and_then(|value| value.to_str()).unwrap_or("");
    if ext.eq_ignore_ascii_case("mp4") || ext.eq_ignore_ascii_case("mkv") {
        return split_video(input, VIDEO_PART_LIMIT, workspace);
    }

    let stem = input.file_stem().and_then(|value| value.to_str()).unwrap_or("video");
    let converted = workspace.join(format!("{stem}.mkv"));
    let remuxed = Command::new("ffmpeg").args(["-nostdin", "-y", "-v", "error", "-i"])
        .arg(input).args(["-c", "copy"]).arg(&converted)
        .status().map_err(|e| format!("Could not start FFmpeg: {e}"))?.success();
    if !remuxed {
        let output = Command::new("ffmpeg").args(["-nostdin", "-y", "-v", "error", "-i"])
            .arg(input).args(["-c:v", "libx264", "-preset", "veryfast", "-crf", "23", "-c:a", "aac"])
            .arg(&converted).output().map_err(|e| format!("Could not start FFmpeg: {e}"))?;
        if !output.status.success() {
            return Err(format!("FFmpeg could not convert the video: {}", String::from_utf8_lossy(&output.stderr).trim()));
        }
    }
    split_video(&converted, VIDEO_PART_LIMIT, workspace)
}

/// 话题标题的最大长度（Telegram 限制 128 字符）。
/// Maximum forum topic title length (Telegram limit is 128 characters).
const MAX_TOPIC_TITLE_LEN: usize = 128;

/// 返回模块配置目录（系统配置目录下的 notify_telegram/）。
/// Return the module config directory (notify_telegram/ under the system config dir).
fn config_dir_path() -> PathBuf {
    let base = dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("notify_telegram");
    fs::create_dir_all(&base).ok();
    base
}

/// 话题缓存文件路径：记录「群组 + 模特 → 话题 ID」，避免重复创建同名话题。
/// Forum topic cache path: maps "chat + model → topic id" to avoid creating duplicate topics.
fn topic_cache_path() -> PathBuf {
    config_dir_path().join("topics.tsv")
}

/// 生成用于话题缓存的主播名键（统一小写，忽略首尾空格）。
/// Build the streamer key used by the topic cache (lowercased, trimmed).
fn topic_model_key(model_name: &str) -> String {
    model_name.trim().to_lowercase()
}

/// 读取话题缓存（键为 `(chat_id, 小写模特名)`）。
/// Load the topic cache (keyed by `(chat_id, lowercased model name)`).
fn load_topic_cache() -> std::collections::HashMap<(i64, String), i32> {
    let mut map = std::collections::HashMap::new();
    let Ok(content) = fs::read_to_string(topic_cache_path()) else {
        return map;
    };
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut parts = line.split('\t');
        let (Some(chat), Some(model), Some(topic)) = (parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        let (Ok(chat), Ok(topic)) = (
            chat.trim().parse::<i64>(),
            topic.trim().parse::<i32>(),
        ) else {
            continue;
        };
        map.insert((chat, topic_model_key(model)), topic);
    }
    map
}

/// 写入或删除一条话题缓存记录（`topic_id = None` 表示删除）。
/// Insert or remove one topic cache entry (`topic_id = None` removes it).
fn update_topic_cache(chat_id: i64, model_name: &str, topic_id: Option<i32>) {
    let mut map = load_topic_cache();
    let key = (chat_id, topic_model_key(model_name));
    match topic_id {
        Some(id) => {
            map.insert(key, id);
        }
        None => {
            map.remove(&key);
        }
    }
    save_topic_cache(&map);
}

/// 用「服务端实际存在的话题」替换某个群组的全部缓存记录（保留其他群组的记录）。
/// Replace all cached entries of one chat with the topics that actually exist on the server
/// (entries of other chats are preserved).
fn replace_chat_topics(chat_id: i64, topics: &[(String, i32)]) {
    let mut map = load_topic_cache();
    map.retain(|(chat, _), _| *chat != chat_id);
    for (title, id) in topics {
        map.insert((chat_id, topic_model_key(title)), *id);
    }
    save_topic_cache(&map);
}

/// 把话题缓存整体写回磁盘。
/// Persist the whole topic cache to disk.
fn save_topic_cache(map: &std::collections::HashMap<(i64, String), i32>) {
    let mut entries: Vec<((i64, String), i32)> = map.clone().into_iter().collect();
    entries.sort();
    let mut out = String::from("# chat_id\tmodel\ttopic_id\n");
    for ((chat, model), id) in entries {
        out.push_str(&format!("{}\t{}\t{}\n", chat, model, id));
    }
    if let Err(e) = fs::write(topic_cache_path(), out) {
        eprintln!("failed to write topic cache: {}", e);
    }
}

/// 规范化话题标题（模特名，超长截断）。
/// Normalize the forum topic title (model name, truncated when too long).
fn topic_title(model_name: &str) -> String {
    let trimmed = model_name.trim();
    let title = if trimmed.is_empty() { "unknown" } else { trimmed };
    title.chars().take(MAX_TOPIC_TITLE_LEN).collect()
}

/// 生成一个随机的 TL `random_id`（用于创建话题、发送相册）。
/// Generate a random TL `random_id` (used for creating topics and sending albums).
fn random_i64() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let t = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let nanos = t.as_nanos() as i64;
    // 混合进程内计数器，避免同一纳秒内重复
    // Mix in an in-process counter to avoid collisions within the same nanosecond
    use std::sync::atomic::{AtomicI64, Ordering};
    static COUNTER: AtomicI64 = AtomicI64::new(0x5deece66d);
    let c = COUNTER.fetch_add(0x9e3779b97f4a7c15_u64 as i64, Ordering::Relaxed);
    nanos.wrapping_mul(0x2545F4914F6CDD1D_u64 as i64) ^ c
}

/// 从 `Updates` 中解析新建话题的 ID（即创建话题的服务消息 ID）。
/// Extract the new topic id from `Updates` (the id of the topic-creation service message).
fn topic_id_from_updates(updates: &tl::enums::Updates) -> Option<i32> {
    let tl::enums::Updates::Updates(raw) = updates else {
        return None;
    };
    let mut first_id: Option<i32> = None;
    for update in &raw.updates {
        let message = match update {
            tl::enums::Update::NewChannelMessage(u) => Some(&u.message),
            tl::enums::Update::NewMessage(u) => Some(&u.message),
            _ => None,
        };
        match message {
            // 优先匹配「创建话题」服务消息，其 ID 即话题 ID
            // Prefer the topic-creation service message: its id is the topic id
            Some(tl::enums::Message::Service(m)) => {
                if matches!(m.action, tl::enums::MessageAction::TopicCreate(_)) {
                    return Some(m.id);
                }
                first_id = first_id.or(Some(m.id));
            }
            Some(tl::enums::Message::Message(m)) => first_id = first_id.or(Some(m.id)),
            _ => continue,
        }
    }
    first_id
}

/// 列出群组中的全部论坛话题（分页遍历），返回 `(标题, 话题 ID)`。
///
/// **仅用户账号可用**：机器人调用 `messages.getForumTopics` 会返回
/// `BOT_METHOD_INVALID`（Bot API 也没有对应的方法），所以机器人模式下必须跳过。
///
/// List every forum topic of the group (paginated) as `(title, topic id)`.
///
/// **User accounts only**: calling `messages.getForumTopics` from a bot session fails with
/// `BOT_METHOD_INVALID` (the Bot API has no equivalent), so bot mode must skip this.
async fn list_all_topics(client: &Client, peer: &PeerRef) -> Result<Vec<(String, i32)>, String> {
    const PAGE: i32 = 100;
    let mut offset_date = 0i32;
    let mut offset_id = 0i32;
    let mut offset_topic = 0i32;
    let mut found: Vec<(String, i32)> = Vec::new();

    // 最多翻 20 页（2000 个话题），避免异常情况下无限循环
    // Up to 20 pages (2000 topics) to avoid an unbounded loop in abnormal cases
    for _ in 0..20 {
        let result = client
            .invoke(&tl::functions::messages::GetForumTopics {
                peer: (*peer).into(),
                q: None,
                offset_date,
                offset_id,
                offset_topic,
                limit: PAGE,
            })
            .await
            .map_err(|e| format!("getForumTopics failed: {}", e))?;

        let tl::enums::messages::ForumTopics::Topics(topics) = result;

        let mut last_id = 0i32;
        let mut last_date = 0i32;
        for topic in &topics.topics {
            let tl::enums::ForumTopic::Topic(t) = topic else {
                continue;
            };
            found.push((t.title.trim().to_string(), t.id));
            last_id = t.id;
            last_date = t.date;
        }

        if topics.topics.len() < PAGE as usize || last_id == 0 {
            break;
        }
        offset_topic = last_id;
        offset_date = last_date;
        offset_id = 0;
    }
    Ok(found)
}

/// 用话题标题过滤服务端话题，缓存未命中时避免为了找一个话题翻完 20 页。
///
/// Search server-side topics by title so a cache miss does not need to page through all
/// 20 pages just to find one topic. Returns the exact (case-insensitive) match only.
async fn find_topic_by_query(
    client: &Client,
    peer: &PeerRef,
    title: &str,
) -> Result<Option<i32>, String> {
    let result = client
        .invoke(&tl::functions::messages::GetForumTopics {
            peer: (*peer).into(),
            q: Some(title.to_string()),
            offset_date: 0,
            offset_id: 0,
            offset_topic: 0,
            limit: 100,
        })
        .await
        .map_err(|e| format!("getForumTopics(query) failed: {}", e))?;

    let tl::enums::messages::ForumTopics::Topics(topics) = result;
    Ok(topics
        .topics
        .iter()
        .filter_map(|topic| match topic {
            tl::enums::ForumTopic::Topic(t) => Some((t.title.trim(), t.id)),
            _ => None,
        })
        .find(|(t, _)| t.eq_ignore_ascii_case(title.trim()))
        .map(|(_, id)| id))
}

/// 在群组中创建论坛话题，返回新话题 ID。
/// Create a forum topic in the group and return its id.
async fn create_topic(
    client: &Client,
    peer: &PeerRef,
    title: &str,
    can_list: bool,
    chat_id: i64,
) -> Result<i32, String> {
    let updates = client
        .invoke(&tl::functions::messages::CreateForumTopic {
            title_missing: false,
            peer: (*peer).into(),
            title: title.to_string(),
            icon_color: None,
            icon_emoji_id: None,
            random_id: random_i64(),
            send_as: None,
        })
        .await
        .map_err(|e| topic_error_hint(&e.to_string(), chat_id))?;

    if let Some(id) = topic_id_from_updates(&updates) {
        return Ok(id);
    }

    // 回退：创建成功但无法从 updates 解析 ID 时，按标题重新查找
    // （机器人无法列出话题，此时只能报错）
    // Fallback: when the id cannot be parsed from updates, look the topic up by title again
    // (bots cannot list topics, so they can only report the failure)
    if !can_list {
        return Err(
            "created forum topic but could not determine its id (bot sessions cannot list topics)"
                .to_string(),
        );
    }
    list_all_topics(client, peer)
        .await?
        .into_iter()
        .find(|(t, _)| t.eq_ignore_ascii_case(title))
        .map(|(_, id)| id)
        .ok_or_else(|| "created forum topic but could not determine its id".to_string())
}

/// 解析模特对应的群组话题：优先使用缓存，其次（用户账号）从服务端刷新，最后新建。
///
/// Resolve the forum topic for a model: cache first, then (user accounts only) refresh
/// from the server, and finally create it.
///
/// # 参数 / Parameters
/// - `can_list`: 当前会话能否列出话题（用户账号为 `true`，机器人为 `false`）
///   Whether the current session may list topics (`true` for user accounts, `false` for bots)
async fn ensure_topic(
    client: &Client,
    target: &Target,
    chat_id: i64,
    model_name: &str,
    can_list: bool,
) -> Result<i32, String> {
    // 不是论坛超级群组时直接给出可操作的错误，不必等 Telegram 回 CHANNEL_INVALID
    // Fail fast with an actionable message instead of waiting for CHANNEL_INVALID.
    if let Some(problem) = target.topic_problem(chat_id) {
        return Err(problem);
    }

    let key = (chat_id, topic_model_key(model_name));
    if let Some(id) = load_topic_cache().get(&key).copied() {
        return Ok(id);
    }

    let title = topic_title(model_name);

    if can_list {
        // 先按标题做一次服务端过滤查询，绝大多数缓存未命中只需 1 个 RPC。
        // Only one filtered RPC for the common cache-miss case; fall back to the full list.
        match find_topic_by_query(client, &target.peer, &title).await {
            Ok(Some(id)) => {
                update_topic_cache(chat_id, model_name, Some(id));
                println!("topic: reusing \"{}\" (id {})", title, id);
                return Ok(id);
            }
            Ok(None) => {}
            Err(e) => eprintln!("topic: filtered topic lookup failed ({}), listing all topics", e),
        }

        // 过滤查询没命中时再列出全部话题：刷新缓存并避免重复创建。
        // Fall back to listing every topic: refresh the cache so no duplicate is created.
        match list_all_topics(client, &target.peer).await {
            Ok(all) => {
                replace_chat_topics(chat_id, &all);
                if let Some(id) = load_topic_cache().get(&key).copied() {
                    println!("topic: reusing \"{}\" (id {})", title, id);
                    return Ok(id);
                }
            }
            Err(e) => {
                eprintln!(
                    "topic: could not list existing topics ({}), creating without lookup",
                    e
                );
            }
        }
    } else {
        eprintln!(
            "topic: bot sessions cannot list forum topics (BOT_METHOD_INVALID); \
             using the local cache and creating \"{}\" when unknown",
            title
        );
    }

    let id = create_topic(client, &target.peer, &title, can_list, chat_id).await?;
    update_topic_cache(chat_id, model_name, Some(id));
    println!("topic: created \"{}\" (id {})", title, id);
    Ok(id)
}

/// 若错误与话题失效相关（话题被删除/关闭），清除缓存以便下次重建。
/// Clear the cached topic when the error suggests the topic is gone, so it gets recreated.
fn invalidate_topic_on_error(chat_id: i64, model_name: &str, err: &str) {
    let upper = err.to_uppercase();
    if upper.contains("TOPIC") {
        eprintln!("topic error detected, clearing cached topic: {}", err);
        update_topic_cache(chat_id, model_name, None);
    }
}

/// Telegram 登录方式 / Telegram login mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionMode {
    /// 机器人账号（Bot Token）/ Bot account (Bot Token)
    Bot,
    /// 用户账号（`--login` 生成的会话文件）/ User account (session file created by `--login`)
    User,
    /// 自动：已登录的用户会话优先，没有时回退到机器人
    /// Automatic: prefer the signed-in user session, fall back to the bot account
    Auto,
}

impl SessionMode {
    /// 解析 `account` 参数（默认 `auto`：优先使用已登录的真实账号）。
    /// Parse the `account` parameter (defaults to `auto`: the signed-in real account wins).
    fn from_param(raw: &str) -> Self {
        match raw.trim().to_ascii_lowercase().as_str() {
            "user" => SessionMode::User,
            "bot" => SessionMode::Bot,
            _ => SessionMode::Auto,
        }
    }

    /// 把 `auto` 解析成具体方式：只要用户会话文件存在，就使用真实账号。
    ///
    /// 用户会话比机器人会话能力强：能列出群组话题（避免重复创建）、高级会员还能上传
    /// 4GB 单文件，所以「已经登录过」时没有任何理由再用机器人。
    ///
    /// Resolve `auto` into a concrete mode: as soon as a user session file exists, the real
    /// account is used.
    ///
    /// A user session is strictly more capable than a bot session — it can list forum topics
    /// (so topics are reused instead of duplicated) and premium accounts may upload 4GB in a
    /// single file — so once a login exists there is no reason to keep using the bot.
    fn resolve(self, api_id: i32) -> Self {
        self.resolve_with_label(api_id, "")
    }

    /// `auto` 解析时也要认得具名主会话，否则用 session_label 登录的账号会被误判成「未登录」。
    /// Resolve `auto` while honouring a labeled primary session, so signing in with
    /// `session_label` is not mistaken for "not signed in".
    fn resolve_with_label(self, api_id: i32, label: &str) -> Self {
        match self {
            SessionMode::Auto => {
                if user_session_path(api_id, label).exists() {
                    SessionMode::User
                } else {
                    SessionMode::Bot
                }
            }
            concrete => concrete,
        }
    }

    /// 当前登录方式能否列出群组话题 / Whether this login mode may list group topics.
    ///
    /// 机器人调用 `messages.getForumTopics` 会返回 `BOT_METHOD_INVALID`。
    /// Bots get `BOT_METHOD_INVALID` from `messages.getForumTopics`.
    fn can_list_topics(self) -> bool {
        matches!(self, SessionMode::User | SessionMode::Auto)
    }

    /// 用于日志的简短名称 / Short name used in logs.
    fn label(self) -> &'static str {
        match self {
            SessionMode::Bot => "bot",
            SessionMode::User => "user",
            SessionMode::Auto => "auto",
        }
    }
}

/// 获取 Telegram 会话文件路径（存储在系统配置目录下）。
///
/// 机器人会话与用户会话分开保存，避免同一 api_id 的两种授权互相覆盖。
///
/// Get the Telegram session file path (stored in the system config directory).
///
/// Bot and user sessions live in separate files so that two authorizations of the same
/// api_id never overwrite each other.
fn session_path(api_id: i32, mode: SessionMode) -> PathBuf {
    match mode {
        SessionMode::User => config_dir_path().join(format!("{}_user.db", api_id)),
        // `auto` 在调用前已解析；兜底按机器人会话处理
        // `auto` is resolved before it gets here; fall back to the bot session file
        SessionMode::Bot | SessionMode::Auto => config_dir_path().join(format!("{}.db", api_id)),
    }
}

/// 把用户填写的会话标签规范化成安全的文件名片段。
/// Sanitize a user-supplied session label into a safe filename fragment.
///
/// 多个 Telegram 账号可以共用同一个 API ID，所以会话文件按标签区分：
/// 空标签沿用旧文件 `{api_id}_user.db`，具名标签使用 `{api_id}_user_{label}.db`。
/// Several Telegram accounts can share one API ID, so sessions are separated by label:
/// an empty label keeps the legacy `{api_id}_user.db`, named labels use
/// `{api_id}_user_{label}.db`.
fn sanitize_session_label(raw: &str) -> String {
    raw.trim()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect::<String>()
        .trim_matches('_')
        .to_string()
}

/// 返回带标签的用户会话文件路径。空标签保持原有主会话文件名，保证旧配置不需要重新登录。
/// Return the labeled user session path. An empty label keeps the legacy primary session file,
/// so existing configurations never need to sign in again.
fn user_session_path(api_id: i32, label: &str) -> PathBuf {
    let clean = sanitize_session_label(label);
    if clean.is_empty() {
        session_path(api_id, SessionMode::User)
    } else {
        config_dir_path().join(format!("{}_user_{}.db", api_id, clean))
    }
}

/// 解析备用会话标签列表（逗号、分号或换行分隔），去重并过滤无效项。
/// Parse the fallback session label list (comma/semicolon/newline separated), deduplicated.
fn parse_fallback_labels(raw: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for part in raw.split(|c: char| c == ',' || c == ';' || c == '\n' || c == '\r') {
        let clean = sanitize_session_label(part);
        if !clean.is_empty() && !out.contains(&clean) {
            out.push(clean);
        }
    }
    out
}

/// 一次发送尝试使用的会话身份：登录方式、标签和实际 session 文件路径。
/// The session identity used by one send attempt: mode, label and concrete session path.
#[derive(Debug, Clone)]
struct SessionIdentity {
    api_id: i32,
    mode: SessionMode,
    label: String,
    path: PathBuf,
}

impl SessionIdentity {
    /// 日志里使用的简短名称 / Short name used in logs.
    fn display_name(&self) -> String {
        if self.label.is_empty() {
            match self.mode {
                SessionMode::User | SessionMode::Auto => "primary user".to_string(),
                SessionMode::Bot => "bot".to_string(),
            }
        } else {
            format!("user({})", self.label)
        }
    }
}

/// 根据（已解析的）登录方式和标签构造主会话身份。
/// Build the primary session identity from a resolved mode and label.
fn primary_session(api_id: i32, mode: SessionMode, label: &str) -> SessionIdentity {
    let clean = sanitize_session_label(label);
    let (mode, label, path) = if mode == SessionMode::Bot {
        (SessionMode::Bot, String::new(), session_path(api_id, SessionMode::Bot))
    } else {
        (SessionMode::User, clean, user_session_path(api_id, label))
    };
    SessionIdentity { api_id, mode, label, path }
}

/// 为当前进程复制一份独立的会话文件副本。
///
/// 多个并发上传进程共用同一个会话文件时，grammers-session 的 SQLite 写锁
/// 会互相冲突并在锁上 panic（`storages/sqlite.rs` 中的 `unwrap`）。这里把
/// 会话复制到进程专属的临时路径：Telegram 允许同一个 auth key 建立多条
/// 连接，所以副本不需要重新登录。
///
/// 原文件不存在（例如首次机器人登录）或复制失败时，退回原路径 —— 退回后
/// 仍然可能发生锁冲突，但那只是降级为旧行为，而不是新问题。
///
/// Give this process its own copy of the session file.
///
/// When concurrent upload processes share one session file, grammers-session's
/// SQLite write locks collide and panic on the `unwrap` in `storages/sqlite.rs`.
/// The session is copied to a process-private temporary path instead: Telegram
/// allows several connections with the same auth key, so the copy needs no
/// re-login. When the original does not exist (first bot login) or the copy
/// fails, the original path is returned unchanged — degrading to the old
/// behaviour rather than introducing a new failure.
fn private_session_copy(path: &Path) -> PathBuf {
    // 首次登录（文件不存在）或复制失败时用原路径
    // First login (file missing) or copy failure falls back to the original path
    if !path.is_file() {
        return path.to_path_buf();
    }
    let file_name = path.file_name().and_then(|s| s.to_str()).unwrap_or("session");
    let copy = tmp_dir().join(format!("{}.{}.session", file_name, std::process::id()));
    match std::fs::copy(path, &copy) {
        Ok(_) => {
            let wal = format!("{}-wal", path.display());
            let shm = format!("{}-shm", path.display());
            if Path::new(&wal).is_file() {
                let _ = std::fs::copy(&wal, format!("{}-wal", copy.display()));
            }
            if Path::new(&shm).is_file() {
                let _ = std::fs::copy(&shm, format!("{}-shm", copy.display()));
            }
            copy
        }
        Err(_) => path.to_path_buf(),
    }
}

/// 清理本进程创建的临时会话副本 / Clean up the temp session copy for this process
fn cleanup_session_copy(path: &Path) {
    let file_name = path.file_name().and_then(|s| s.to_str()).unwrap_or("session");
    let copy = tmp_dir().join(format!("{}.{}.session", file_name, std::process::id()));
    let _ = std::fs::remove_file(&copy);
    let _ = std::fs::remove_file(format!("{}-wal", copy.display()));
    let _ = std::fs::remove_file(format!("{}-shm", copy.display()));
}

/// 刷新会话：重新打开一个已授权的会话并强制同步，刷新认证密钥与状态。
///
/// 由后端 `/api/telegram/relogin` 驱动（模块的 `--refresh` 参数）。需要 `--login`
/// 登录过的已授权会话文件；会话未授权时会给出明确的登录提示，而不是让用户猜。
/// 刷新成功后 stdout 会打印 `LOGIN_STATE:refreshed:{user}`，失败则打印错误。
///
/// Refresh a session: re-open an already-authorized session and sync to
/// refresh the auth key and status.
///
/// Driven by the backend `/api/telegram/relogin` endpoint (module `--refresh` param).
/// Requires an authorized session file created by `--login`; if the session is not
/// authorized, prints a clear login hint instead of leaving the user guessing. On
/// success stdout prints `LOGIN_STATE:refreshed:...`, on failure prints an error.
async fn refresh_session(api_id: i32, proxy: &str, label: &str) -> Result<(), String> {
    let path = user_session_path(api_id, label);
    if !path.is_file() {
        return Err(format!(
            "session file not found at {} — click \"Login\" in the pipeline UI to sign in first",
            path.display()
        ));
    }
    // 刷新时必须操作真实会话文件（session_private: false），避免在临时副本上操作
    // Refresh must operate directly on the real session file (session_private: false)
    let client = open_client(api_id, proxy, SessionMode::User, Some(&path), false).await?;

    if !client
        .is_authorized()
        .await
        .map_err(|e| format!("is_authorized failed: {}", e))?
    {
        return Err("session file exists but is not authorized — the real account is not signed in. Run with --login once to sign in".to_string());
    }

    let me = client
        .get_me()
        .await
        .map_err(|e| format!("get_me failed: {}", e))?;
    let user = login_user_summary(&me);

    login_state(&format!("refreshed:{}", user));
    eprintln!("session refreshed as {}", user);
    Ok(())
}

/// 刷新模式入口（`--refresh`）：验证并刷新已授权用户会话（无需手机号或验证码）。
/// Refresh entry point (`--refresh`): verify and refresh an authorized user session.
fn run_refresh(api_id: i32, proxy: &str, label: &str) -> Result<(), String> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("tokio runtime error: {}", e))?
        .block_on(refresh_session(api_id, proxy, label))
}

/// 打开（或创建）会话文件并建立 MTProto 连接，但不做任何鉴权。
/// Open (or create) the session file and connect over MTProto without authenticating.
/// 并发上传时多个模块进程同时打开同一个会话文件，SQLite 写锁会互相冲突
/// （grammers-session 在锁上 `unwrap()`，直接 panic）。因此每个进程复制一份
/// 独立的会话副本：Telegram 允许同一个 auth key 建立多条连接，副本不需要
/// 重新登录，只在临时目录留下一个文件。
///
/// Concurrent processes would open the same session file and collide on the
/// SQLite write lock (grammers-session `unwrap()s` and panics). Each process
/// gets its own copy instead: Telegram allows multiple connections with the
/// same auth key, so no re-login is needed.
async fn open_client(
    api_id: i32,
    proxy: &str,
    mode: SessionMode,
    session_override: Option<&Path>,
    session_private: bool,
) -> Result<Client, String> {
    let path = session_override
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| session_path(api_id, mode));

    // 复制出每个进程独立的会话副本，避免 SQLite 锁冲突；登录流程必须写回
    // 原会话文件，因此不走副本
    // Copy a per-process session file to avoid SQLite lock collisions; the login
    // flow must write back to the real session file, so it never uses a copy
    let path = if session_private {
        private_session_copy(&path)
    } else {
        path
    };

    // 打开或创建 SQLite 会话文件 / Open or create SQLite session file
    let session = Arc::new(
        SqliteSession::open(&path)
            .await
            .map_err(|e| format!("open session {} failed: {}", path.display(), e))?,
    );

    let conn_params = ConnectionParams {
        proxy_url: if proxy.is_empty() { None } else { Some(proxy.to_string()) },
        ..Default::default()
    };

    // 创建连接池并在后台运行 / Create connection pool and run in background
    let pool = SenderPool::with_configuration(Arc::clone(&session), api_id, conn_params);
    let runner = pool.runner;
    tokio::spawn(async move { runner.run().await });
    // grammers 0.12 移除了自定义 retry_policy；flood wait 由 sender 自己处理。
    // grammers 0.12 removed the custom retry_policy hook; flood waits are handled by the sender.
    Ok(Client::with_configuration(pool.handle, ClientConfiguration {
        auto_cache_peers: true,
    }))
}

/// 建立 MTProto 连接，并按登录方式完成鉴权。
///
/// - `bot`：会话未授权时使用 Bot Token 登录（首次运行会自动创建会话文件）。
/// - `user`：会话必须已经通过 `--login` 授权，否则返回错误提示。
///
/// Open an MTProto connection and authenticate according to the login mode.
///
/// - `bot`: signs in with the Bot Token when the session is not authorized yet
///   (the session file is created on first run).
/// - `user`: the session must already be authorized via `--login`, otherwise an error is returned.
async fn connect_client(
    api_id: i32,
    api_hash: &str,
    bot_token: &str,
    proxy: &str,
    mode: SessionMode,
    session_override: Option<&Path>,
) -> Result<Client, String> {
    // `auto` 已经在 read_config 中解析过；这里再兜底一次，保证连接用的总是具体方式
    // `auto` is already resolved in read_config; resolve again here so the connection always
    // uses a concrete mode
    let mode = mode.resolve(api_id);
    let path = session_override
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| session_path(api_id, mode));

    // 用户模式：会话文件不存在时直接给出可操作的提示
    // User mode: fail fast with an actionable message when the session file is missing
    if mode == SessionMode::User && !path.exists() {
        return Err(format!(
            "user session not found at {} — click \"Login\" in the pipeline UI (or run the module with --login) first",
            path.display()
        ));
    }

    let client = open_client(api_id, proxy, mode, Some(&path), true).await?;

    let authorized = client
        .is_authorized()
        .await
        .map_err(|e| format!("is_authorized failed: {}", e))?;

    match mode {
        // Bot 登录（会话已存在时跳过）/ Bot sign-in (skipped if the session exists)
        SessionMode::Bot => {
            if !authorized {
                client
                    .bot_sign_in(bot_token, api_hash)
                    .await
                    .map_err(|e| format!("bot_sign_in failed: {}", e))?;
            }
        }
        // 用户登录必须事先完成 / The user login must have been completed beforehand
        SessionMode::User => {
            if !authorized {
                return Err(format!(
                    "user session {} is not authorized — click \"Login\" in the pipeline UI to sign in again",
                    path.display()
                ));
            }
        }
        // resolve() 只返回具体方式 / resolve() only ever returns a concrete mode
        SessionMode::Auto => unreachable!("auto is resolved before connecting"),
    }

    Ok(client)
}

/// 根据数字 chat_id 构建 Telegram PeerRef（不需要网络请求）。
/// Build a Telegram PeerRef from a numeric chat_id (no network request needed).
///
/// Telegram 的 Bot API 会话 ID 只有三种形状：
/// - 普通群：`-<chat_id>`（负数，普通群 ID 已经能到 10~12 位）
/// - 超级群组/频道：`-100<channel_id>`
/// - 用户：正数
///
/// 另外兼容省略负号的超级群组 ID（把 `-1002795956983` 写成 `1002795956983`）。
///
/// Telegram's Bot API dialog ids only come in three shapes:
/// - basic group: `-<chat_id>` (negative; basic-group ids already reach 10–12 digits)
/// - supergroup/channel: `-100<channel_id>`
/// - user: positive
///
/// A supergroup id written without the minus sign (`1002795956983`) is also accepted.
fn build_peer_ref(chat_id: i64) -> PeerRef {
    let id = if chat_id <= -1_000_000_000_000 {
        // -100xxxxxxxxxx → 超级群组/频道 / supergroup or channel
        PeerId::channel_unchecked((-chat_id) - 1_000_000_000_000)
    } else if chat_id < 0 {
        // -xxxxxxxxx → 普通群 / basic group
        PeerId::chat_unchecked(-chat_id)
    } else if chat_id >= 1_000_000_000_000 {
        // 省略负号的超级群组/频道 ID：100xxxxxxxxxx
        // Supergroup/channel id written without the minus sign: 100xxxxxxxxxx
        PeerId::channel_unchecked(chat_id - 1_000_000_000_000)
    } else {
        PeerId::user_unchecked(chat_id)
    };
    PeerRef { id, auth: PeerAuth::default() }
}

/// 数字 chat_id 可能的 Peer 解释，主解释排在最前面。
///
/// 负数只可能是普通群或 `-100…` 频道，但很多人会把频道 ID 写成 `-<channel_id>`（少写
/// 100 前缀），正数也可能是省略负号的频道 ID。所以解析失败时再按频道解释试一次。
///
/// Every plausible peer interpretation of a numeric chat_id, primary first.
///
/// A negative id is either a basic group or a `-100…` channel, but people often write a
/// channel id as `-<channel_id>` (dropping the 100 prefix), and a positive id may be a
/// channel id without the minus sign. The channel interpretation is therefore tried as a
/// fallback when the primary one does not resolve.
fn peer_candidates(chat_id: i64) -> Vec<PeerId> {
    let primary = build_peer_ref(chat_id).id;
    let fallback = if chat_id <= -1_000_000_000_000 {
        None
    } else if chat_id < 0 {
        // -<channel_id>：少写了 100 前缀 / the 100 prefix is missing
        Some(PeerId::channel_unchecked(-chat_id))
    } else if chat_id >= 1_000_000_000_000 {
        None
    } else {
        // <channel_id>：省略负号 / the minus sign is missing
        Some(PeerId::channel_unchecked(chat_id))
    };
    match fallback {
        Some(id) if id != primary => vec![primary, id],
        _ => vec![primary],
    }
}

/// 目标会话的类别 / Kind of the resolved target chat.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChatKind {
    /// 私聊用户 / A user (private chat)
    User,
    /// 普通群（从未升级为超级群组）/ A basic group that was never upgraded to a supergroup
    BasicGroup,
    /// 超级群组（`forum` 表示是否已开启话题）/ A supergroup; `forum` reports whether Topics are on
    Supergroup { forum: bool },
    /// 广播频道 / A broadcast channel
    Broadcast,
    /// 未能向 Telegram 确认 / Telegram could not confirm the kind
    Unknown,
}

impl ChatKind {
    /// 供日志与错误提示使用的人类可读名称 / Human-readable name used in logs and errors.
    fn label(self) -> &'static str {
        match self {
            ChatKind::User => "user",
            ChatKind::BasicGroup => "basic group",
            ChatKind::Supergroup { forum: true } => "supergroup, topics on",
            ChatKind::Supergroup { forum: false } => "supergroup, topics off",
            ChatKind::Broadcast => "broadcast channel",
            ChatKind::Unknown => "kind unknown",
        }
    }
}

/// 解析后的目标会话：带真实 `access_hash` 的引用 + Telegram 返回的元数据。
///
/// The resolved target: a `PeerRef` carrying the real `access_hash`, plus the metadata
/// Telegram reported for it (when the lookup succeeded).
#[derive(Debug, Clone)]
struct Target {
    peer: PeerRef,
    kind: ChatKind,
    title: Option<String>,
}

impl Target {
    /// 一行描述，用于日志与自检输出 / One-line description used in logs and the self-test.
    fn describe(&self) -> String {
        match &self.title {
            Some(title) if !title.is_empty() => {
                format!("{} \"{}\" ({})", self.peer.id, title, self.kind.label())
            }
            _ => format!("{} ({})", self.peer.id, self.kind.label()),
        }
    }

    /// 该会话为什么无法创建论坛话题；可以创建时返回 `None`。
    ///
    /// 提前给出结论，避免把「普通群 / 未开启话题」拖到 Telegram 的 `CHANNEL_INVALID`。
    ///
    /// Why this chat cannot host forum topics (`None` when it can).
    ///
    /// Deciding this up-front turns a bare `CHANNEL_INVALID` into an actionable message.
    fn topic_problem(&self, chat_id: i64) -> Option<String> {
        match self.kind {
            ChatKind::BasicGroup => Some(format!(
                "chat {} is a basic group — forum topics only exist in supergroups with Topics \
                 enabled. Open the group in Telegram → Edit → Topics → turn it on (Telegram then \
                 upgrades the group to a supergroup and its id becomes -100…; use that new id as \
                 the Chat ID). To post into the group without topics, turn off \"topic per model\".",
                chat_id
            )),
            ChatKind::Supergroup { forum: false } => Some(format!(
                "topics are disabled in chat {} — enable Topics in the group settings, or turn off \
                 \"topic per model\" to post into the main chat.",
                chat_id
            )),
            ChatKind::Broadcast => Some(format!(
                "chat {} is a broadcast channel — forum topics need a supergroup, not a channel",
                chat_id
            )),
            ChatKind::User => Some(format!(
                "chat {} is a user, not a group — forum topics need a supergroup",
                chat_id
            )),
            ChatKind::Supergroup { forum: true } | ChatKind::Unknown => None,
        }
    }
}

/// 从 `messages.Chats` 结果中取出会话对象列表 / Pull the chat list out of a `messages.Chats`.
fn chats_of(result: tl::enums::messages::Chats) -> Vec<tl::enums::Chat> {
    match result {
        tl::enums::messages::Chats::Chats(c) => c.chats,
        tl::enums::messages::Chats::Slice(c) => c.chats,
    }
}

/// 从 TL `Chat` 对象读取类别与标题 / Read the kind and title out of a raw `Chat`.
fn chat_kind_and_title(chat: &tl::enums::Chat) -> (ChatKind, Option<String>) {
    match chat {
        tl::enums::Chat::Channel(c) => (
            if c.broadcast {
                ChatKind::Broadcast
            } else {
                ChatKind::Supergroup { forum: c.forum }
            },
            Some(c.title.clone()),
        ),
        // 无法访问的会话看不到 forum 标记，按「未开启话题」处理
        // An inaccessible channel does not expose the forum flag; treat it as "topics off"
        tl::enums::Chat::ChannelForbidden(c) => (
            if c.broadcast {
                ChatKind::Broadcast
            } else {
                ChatKind::Supergroup { forum: false }
            },
            Some(c.title.clone()),
        ),
        tl::enums::Chat::Chat(c) => (ChatKind::BasicGroup, Some(c.title.clone())),
        tl::enums::Chat::Forbidden(c) => (ChatKind::BasicGroup, Some(c.title.clone())),
        tl::enums::Chat::CommunityForbidden(c) => {
            (ChatKind::Supergroup { forum: true }, Some(c.title.clone()))
        }
        tl::enums::Chat::Community(c) => {
            (ChatKind::Supergroup { forum: true }, Some(c.title.clone()))
        }
        tl::enums::Chat::Empty(_) => (ChatKind::Unknown, None),
    }
}

/// 从 grammers 的 `Peer` 读取类别与标题 / Read the kind and title out of a grammers `Peer`.
fn peer_kind_and_title(peer: &Peer) -> (ChatKind, Option<String>) {
    match peer {
        Peer::User(_) => (ChatKind::User, peer.name().map(str::to_string)),
        Peer::Channel(c) => (ChatKind::Broadcast, Some(c.raw.title.clone())),
        Peer::Group(g) => chat_kind_and_title(&g.raw),
        Peer::Community(c) => (ChatKind::Supergroup { forum: true }, Some(c.raw.title.clone())),
    }
}

/// 用 Telegram 返回的会话对象补齐 `access_hash`（只有频道/超级群组带这个值）。
///
/// Fill in the real `access_hash` from the chat object Telegram returned (channels only).
fn with_real_access_hash(peer: &PeerRef, chat: &tl::enums::Chat) -> PeerRef {
    let hash = match chat {
        tl::enums::Chat::Channel(c) => c.access_hash,
        tl::enums::Chat::ChannelForbidden(c) => Some(c.access_hash),
        _ => None,
    };
    match hash {
        Some(hash) => PeerRef { id: peer.id, auth: PeerAuth::from_hash(hash) },
        None => *peer,
    }
}

/// 把 `createForumTopic` 的原始错误翻译成可操作的提示。
/// Turn a raw `createForumTopic` error into an actionable hint.
fn topic_error_hint(raw: &str, chat_id: i64) -> String {
    let upper = raw.to_uppercase();
    let mut msg = format!("createForumTopic failed: {}", raw);
    if upper.contains("CHANNEL_INVALID") {
        msg.push_str(&format!(
            "\n  hint: Telegram could not resolve chat {} as a supergroup with topics. Most often \
             the id belongs to a basic group (topics need a supergroup), the account is no longer \
             a member, or the account has never seen this chat. Enable Topics in the group — it \
             then becomes a supergroup with a new -100… id — and use that id.",
            chat_id
        ));
    } else if upper.contains("CHAT_ADMIN_REQUIRED") || upper.contains("RIGHT_FORBIDDEN") {
        msg.push_str(
            "\n  hint: the account must be a group admin with the \"Manage Topics\" permission.",
        );
    } else if upper.contains("CHAT_WRITE_FORBIDDEN") {
        msg.push_str("\n  hint: the account may not post in this group (check the group permissions).");
    }
    msg
}

/// 在会话的对话框列表中查找指定 Peer。
///
/// 对话框里的会话对象带真实的 `access_hash`，用户账号与机器人账号都能用这一招；
/// 机器人凭 `access_hash = 0` 只能解析「自己已经见过」的会话，所以私有群组常常解析失败。
/// 最多扫描 `MAX_PAGES` 页，避免账号加入大量群组时耗时过长。
///
/// Look the given peer up in the session's dialog list.
///
/// Dialogs carry the real `access_hash`, and this works for user and bot accounts alike.
/// A bot can only resolve chats it has already seen with `access_hash = 0`, which is why
/// private groups often fail to resolve. At most `MAX_PAGES` pages are scanned.
async fn find_peer_in_dialogs(client: &Client, targets: &[PeerId]) -> Result<Option<Peer>, String> {
    const MAX_PAGES: usize = 5;
    const PAGE: usize = 100;

    let primary = targets[0];
    let fallback = targets.get(1).copied();
    let mut fallback_hit: Option<Peer> = None;

    let mut dialogs = client.iter_dialogs();
    let mut seen = 0usize;
    let mut pages = 0usize;

    while seen < MAX_PAGES * PAGE {
        match dialogs.next().await {
            Ok(Some(dialog)) => {
                seen += 1;
                // 主解释命中即返回；备选解释先记下，继续寻找主解释
                // A primary match wins outright; a fallback match is remembered while the
                // search continues for the primary one.
                if dialog.peer_id() == primary {
                    return Ok(Some(dialog.peer().clone()));
                }
                if fallback == Some(dialog.peer_id()) && fallback_hit.is_none() {
                    fallback_hit = Some(dialog.peer().clone());
                }
                if seen % PAGE == 0 {
                    pages += 1;
                    if pages >= MAX_PAGES {
                        break;
                    }
                }
            }
            Ok(None) => break,
            Err(e) => return Err(format!("iter_dialogs failed: {}", e)),
        }
    }
    Ok(fallback_hit)
}

/// 直接向 Telegram 查询会话对象（`channels.getChannels` / `messages.getChats`）。
///
/// 机器人账号可以凭 `access_hash = 0` 解析自己已经见过的会话，因此这一步经常能拿到
/// 真实的 `access_hash` 以及 `forum`（是否开启话题）标记。
///
/// Ask Telegram for the chat object directly (`channels.getChannels` / `messages.getChats`).
///
/// A bot can resolve chats it has already seen with `access_hash = 0`, so this often yields the
/// real `access_hash` along with the `forum` flag.
async fn fetch_chat(client: &Client, peer: &PeerRef) -> Result<Option<tl::enums::Chat>, String> {
    let chats = match peer.id.kind() {
        PeerKind::Channel => chats_of(
            client
                .invoke(&tl::functions::channels::GetChannels { id: vec![(*peer).into()] })
                .await
                .map_err(|e| format!("channels.getChannels failed: {}", e))?,
        ),
        PeerKind::Chat => chats_of(
            client
                .invoke(&tl::functions::messages::GetChats {
                    id: vec![peer.id.bare_id().ok_or_else(|| "chat id unavailable".to_string())?],
                })
                .await
                .map_err(|e| format!("messages.getChats failed: {}", e))?,
        ),
        _ => return Ok(None),
    };
    Ok(chats.into_iter().next())
}

/// 解析目标会话：优先 username，其次数字 chat_id；尽量拿到真实 `access_hash` 与元数据。
///
/// 解析顺序 / Resolution order:
/// 1. `username`（公开群组）/ `username` (public groups only)
/// 2. 对话框列表（用户与机器人都适用）/ the dialog list (user and bot accounts alike)
/// 3. 直接查询（机器人凭 `access_hash = 0`）/ a direct lookup (bots with `access_hash = 0`)
///
/// 三步都失败时返回未确认的引用，由调用方在真正出错时给出提示。
/// When all three fail the unresolved reference is returned as-is, and the caller reports the
/// real error when the actual request fails.
async fn resolve_target(
    client: &Client,
    chat_id: i64,
    username: &str,
) -> Result<Target, String> {
    if !username.is_empty() {
        let peer = client
            .resolve_username(username)
            .await
            .map_err(|e| format!("resolve_username failed: {}", e))?
            .ok_or_else(|| format!("username @{} not found", username))?;
        let peer_ref = peer
            .to_ref()
            .await
            .map_err(|e| format!("@{} peer_ref lookup failed: {}", username, e))?
            .ok_or_else(|| format!("@{} peer_ref unavailable", username))?;
        let (kind, title) = peer_kind_and_title(&peer);
        return Ok(Target { peer: peer_ref, kind, title });
    }

    let candidates = peer_candidates(chat_id);

    match find_peer_in_dialogs(client, &candidates).await {
        Ok(Some(found)) => {
            if let Ok(Some(peer_ref)) = found.to_ref().await {
                let (kind, title) = peer_kind_and_title(&found);
                if found.id() != candidates[0] {
                    println!(
                        "peer: {} resolved as {} (supergroup/channel interpretation)",
                        chat_id, peer_ref.id
                    );
                }
                return Ok(Target { peer: peer_ref, kind, title });
            }
        }
        Ok(None) => eprintln!(
            "peer: {} is not in this account's dialog list (the account must be a member of the chat)",
            chat_id
        ),
        Err(e) => eprintln!("peer: dialog lookup failed ({}), asking Telegram directly", e),
    }

    for (idx, id) in candidates.iter().enumerate() {
        let candidate = PeerRef { id: *id, auth: PeerAuth::default() };
        match fetch_chat(client, &candidate).await {
            Ok(Some(chat)) => {
                let (kind, title) = chat_kind_and_title(&chat);
                let resolved = with_real_access_hash(&candidate, &chat);
                if idx > 0 {
                    println!(
                        "peer: {} resolved as {} (supergroup/channel interpretation)",
                        chat_id, resolved.id
                    );
                }
                return Ok(Target { peer: resolved, kind, title });
            }
            Ok(None) => {}
            Err(e) if idx == 0 => {
                eprintln!("peer: direct lookup failed ({}), using the id as-is", e)
            }
            Err(_) => {}
        }
    }

    Ok(Target { peer: build_peer_ref(chat_id), kind: ChatKind::Unknown, title: None })
}

/// 计算字符串的 UTF-16 编码长度（Telegram 消息实体使用 UTF-16 偏移量）。
/// Calculate the UTF-16 encoded length of a string (Telegram message entities use UTF-16 offsets).
fn utf16_len(s: &str) -> usize { s.encode_utf16().count() }

/// 构建 Telegram 消息的文字内容和格式化实体（粗体标签、Hashtag、代码块、引用块）。
/// Build the text content and formatting entities for a Telegram message
/// (bold labels, hashtag, code blocks, blockquote).
///
/// # 参数 / Parameters
/// - `model_name`: 主播名（用作 Hashtag）/ Model name (used as hashtag)
/// - `timestamp`: 录制时间戳字符串 / Recording timestamp string
/// - `duration_str`: 格式化后的时长字符串 / Formatted duration string
/// - `file_name`: 文件名 / Filename
/// - `file_size`: 格式化后的文件大小字符串 / Formatted file size string
/// - `part_label`: 分片标签（当前/总数），None 表示不分片 / Part label (current/total), None if not split
fn build_caption(
    model_name: &str, timestamp: &str, duration_str: &str,
    file_name: &str, file_size: &str,
    part_label: Option<(usize, usize)>,
) -> (String, Vec<tl::enums::MessageEntity>) {
    let mut text = String::new();
    let mut entities: Vec<tl::enums::MessageEntity> = Vec::new();

    // 宏：添加一行"粗体标签: 值"并附加对应的格式化实体
    // Macro: add a "bold label: value" line with corresponding formatting entities
    macro_rules! push_line {
        ($label:expr, $value:expr, $entity:expr) => {{
            let bold_start = utf16_len(&text) as i32;
            text.push_str(&format!("{}: ", $label));
            let bold_end = utf16_len(&text) as i32;
            entities.push(tl::types::MessageEntityBold { offset: bold_start, length: bold_end - bold_start }.into());
            let val_start = utf16_len(&text) as i32;
            text.push_str($value);
            let val_end = utf16_len(&text) as i32;
            entities.push($entity(val_start, val_end - val_start));
        }};
    }

    // 第一行：主播名作为 Hashtag / First line: model name as hashtag
    let hashtag_value = format!("#{}", model_name);
    push_line!("ModelName", &hashtag_value, |offset, length| {
        tl::types::MessageEntityHashtag { offset, length }.into()
    });

    // 后续行：时间戳、时长、文件名、文件大小（代码格式）
    // Subsequent lines: timestamp, duration, filename, file size (code format)
    for (label, value) in &[("Timestamp", timestamp), ("Duration", duration_str), ("FileName", file_name), ("FileSize", file_size)] {
        text.push('\n');
        push_line!(label, value, |offset, length| {
            tl::types::MessageEntityCode { offset, length }.into()
        });
    }

    // 可选的分片标签 / Optional part label
    if let Some((cur, total)) = part_label {
        let part_value = format!("{}/{}", cur, total);
        text.push('\n');
        push_line!("Part", &part_value, |offset, length| {
            tl::types::MessageEntityCode { offset, length }.into()
        });
    }

    // 将整个消息包裹在引用块中 / Wrap the entire message in a blockquote
    let total_len = utf16_len(&text) as i32;
    entities.push(tl::types::MessageEntityBlockquote { collapsed: false, offset: 0, length: total_len }.into());
    (text, entities)
}

/// 生成一次性的上传 `file_id`（正整数，整个上传过程保持不变）。
///
/// 混合纳秒时间戳与文件路径，保证不同文件、不同次上传不会撞号；全程使用
/// wrapping 运算，避免 debug 构建下算术溢出 panic。
///
/// Generate a one-off upload `file_id` (a positive integer, kept for the whole upload).
///
/// Mixes the nanosecond timestamp with the file path so different files (and repeated
/// uploads of the same file) never collide. Every step wraps, so a debug build cannot
/// panic on arithmetic overflow.
fn upload_file_id(path: &Path) -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64;
    let path_hash = {
        // FNV-1a：稳定的路径哈希，与标准库的随机种子无关
        // FNV-1a: a stable path hash, independent of the std hasher's random seed
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for byte in path.as_os_str().as_encoded_bytes() {
            hash ^= *byte as u64;
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        hash
    };
    let mixed = nanos ^ path_hash.wrapping_mul(0x9e37_79b9_7f4a_7c15);
    // 取正数并排除 0（0 是 Telegram 的「无效 file_id」）
    // Keep it positive and never zero (0 is Telegram's "invalid file_id")
    let id = (mixed >> 1) as i64;
    if id == 0 { 1 } else { id }
}

/// 每个上传分块的大小（Telegram 允许的最大值）/ Size of one upload part (Telegram's maximum)
const UPLOAD_CHUNK_SIZE: usize = 512 * 1024;

/// 超过这个大小才走大文件接口（不需要 md5）/ Above this size the big-file API is used (no md5)
const BIG_FILE_THRESHOLD: usize = 10 * 1024 * 1024;

/// 默认同时上传的分块数（16 线程满速并发，与 Telegram-Fast-Uploader 保持一致）
/// Default number of parts uploaded concurrently (16 parallel workers matching Telegram-Fast-Uploader)
const DEFAULT_UPLOAD_WORKERS: usize = 16;

/// 并发分块数的上限（再高只会更快触发限流）/ Upper bound (beyond this only flood-waits grow)
const MAX_UPLOAD_WORKERS: usize = 16;

/// 进度上报的最小间隔：避免每个 512KB 分块都刷新一次前端事件。
/// Minimum interval between progress lines: avoid one frontend event per 512KB part.
const PROGRESS_INTERVAL: Duration = Duration::from_millis(250);

/// RPC 要求等待时的上限（秒），防止异常错误让进程挂一天。
/// Cap for server-requested waits, so an abnormal error cannot stall the process for a day.
const MAX_RPC_WAIT_SECS: u64 = 3600;

/// 归一化并发分块数：0 表示自动（默认值），超过上限则截断。
///
/// Normalize the concurrent part count: 0 means automatic (the default), anything above the
/// cap is clamped.
fn normalize_upload_workers(workers: usize) -> usize {
    match workers {
        0 => DEFAULT_UPLOAD_WORKERS,
        n if n > MAX_UPLOAD_WORKERS => MAX_UPLOAD_WORKERS,
        n => n,
    }
}

/// 实际执行文件分块上传的引擎 / Engine that performs file-part uploads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UploadEngine {
    /// WZGram (Pyrogram fork) sidecar, reusing the grammers auth key.
    Wzgram,
    /// Built-in Rust/grammers uploader (the previous behaviour).
    Grammers,
}

/// 解析引擎参数；未知值回退到 WZGram（默认高速路径）。
/// Parse the engine parameter; unknown values fall back to WZGram (the fast default).
fn parse_upload_engine(value: &str) -> UploadEngine {
    match value.trim().to_ascii_lowercase().as_str() {
        "grammers" | "rust" | "builtin" => UploadEngine::Grammers,
        _ => UploadEngine::Wzgram,
    }
}

/// 小文件跳过 WZGram：启动 Python 旁进程的成本比上传几十 KB 封面/缩略图更高。
/// Small files stay on Rust: starting the Python sidecar costs more than uploading
/// a tiny cover or thumbnail.
fn should_use_wzgram(engine: UploadEngine, bytes: u64) -> bool {
    engine == UploadEngine::Wzgram && bytes >= BIG_FILE_THRESHOLD as u64
}

/// 多线程共享的上传进度：汇总字节数，并按秒上报速度。
/// Upload progress shared by every worker: aggregates the byte count and reports speed once a
/// second.
struct UploadProgress {
    /// 本次发送已上传的总字节（含封面图、缩略图和所有视频分片）
    /// Bytes uploaded for this send so far (cover, thumbnails and every video part)
    done: Arc<AtomicUsize>,
    /// 本次发送要上传的总字节 / Total bytes this send will upload
    total: usize,
    /// 速度统计窗口（本文件）/ Speed measurement window (this file)
    window: Mutex<SpeedWindow>,
}

/// 速度统计窗口 / Speed measurement window.
struct SpeedWindow {
    bytes: usize,
    start: Instant,
    /// 上一次真正输出进度的时间与百分比 / Last emitted progress time and percentage
    last_progress: Instant,
    last_scaled: usize,
}

/// 是否需要输出一条进度：百分比前进了，且距上次至少 `PROGRESS_INTERVAL`，或已完成。
/// Whether a progress line is due: the percentage moved and enough time passed, or it is final.
fn should_report_progress(
    scaled: usize,
    last_scaled: usize,
    elapsed: Duration,
    is_final: bool,
) -> bool {
    scaled > last_scaled && (is_final || elapsed >= PROGRESS_INTERVAL)
}

impl UploadProgress {
    fn new(done: Arc<AtomicUsize>, total: usize) -> Self {
        Self {
            done,
            total,
            window: Mutex::new(SpeedWindow {
                bytes: 0,
                start: Instant::now(),
                last_progress: Instant::now(),
                last_scaled: 0,
            }),
        }
    }

    /// 记录一个分块上传完成 / Record that one part finished uploading.
    fn add(&self, bytes: usize) {
        if self.total == 0 || bytes == 0 {
            return;
        }
        let new_done = self.done.fetch_add(bytes, Ordering::Relaxed) + bytes;
        let scaled = ((new_done as u128) * (PROGRESS_SCALE as u128) / (self.total as u128))
            .min(PROGRESS_SCALE as u128) as usize;
        let is_final = scaled >= PROGRESS_SCALE;

        // 锁内只做算术；进度行最多每 250ms 一条，速度仍每秒一条。
        // Only arithmetic under the lock; progress at most every 250ms, speed still once a second.
        let (report_progress, bps) = {
            let mut w = self.window.lock().unwrap_or_else(|e| e.into_inner());
            w.bytes += bytes;
            let speed_elapsed = w.start.elapsed();
            let bps = if speed_elapsed >= Duration::from_secs(1) {
                let bps = w.bytes as f64 / speed_elapsed.as_secs_f64();
                w.bytes = 0;
                w.start = Instant::now();
                Some(bps)
            } else {
                None
            };

            let report = should_report_progress(
                scaled,
                w.last_scaled,
                w.last_progress.elapsed(),
                is_final,
            );
            if report {
                w.last_scaled = scaled;
                w.last_progress = Instant::now();
            }
            (report, bps)
        };

        if !report_progress && bps.is_none() {
            return;
        }
        // 一次 flush 写出所有到期行，减少热路径上的系统调用。
        // Flush all due lines in one syscall instead of one per line.
        let mut lines = String::new();
        if report_progress {
            lines.push_str(&format!("PROGRESS:{}/{}\n", scaled, PROGRESS_SCALE));
        }
        if let Some(bps) = bps {
            lines.push_str(&format!("STATUS:{}\n", format_speed(bps)));
        }
        print!("{}", lines);
        let _ = std::io::stdout().flush();
    }
}

/// 计算整个文件的 md5（只有小文件接口需要）。
/// Compute the md5 of the whole file (only the small-file API needs it).
async fn md5_of_file(path: &Path) -> Result<String, String> {
    let mut file = tokio::fs::File::open(path)
        .await
        .map_err(|e| format!("open {} failed: {}", path.display(), e))?;
    let mut ctx = md5::Context::new();
    let mut buf = vec![0u8; UPLOAD_CHUNK_SIZE];
    loop {
        let n = file
            .read(&mut buf)
            .await
            .map_err(|e| format!("read {} failed: {}", path.display(), e))?;
        if n == 0 {
            break;
        }
        ctx.consume(&buf[..n]);
    }
    Ok(format!("{:x}", ctx.finalize()))
}

/// 从 RPC 错误名/值中解析 Telegram 要求的等待秒数。
///
/// Telegram 可能返回 `FLOOD_WAIT_42`、`FLOOD_WAIT_X`（值在 value 字段）或
/// 非会员上传限速的 `FLOOD_PREMIUM_WAIT_3600`。旧代码只认识精确的 `FLOOD_WAIT`，
/// 因此常见的带秒数错误会退化成固定退避，导致长时间空等或过早重试。
///
/// Parse the server-requested delay from an RPC error name/value. Telegram may return
/// `FLOOD_WAIT_42`, `FLOOD_WAIT_X` (seconds in the value field), or the non-Premium upload
/// limit `FLOOD_PREMIUM_WAIT_3600`. The old code only matched the exact `FLOOD_WAIT`.
fn wait_secs_from_rpc_name(name: &str, value: Option<u32>) -> Option<u64> {
    for prefix in ["FLOOD_PREMIUM_WAIT", "FLOOD_WAIT"] {
        let Some(rest) = name.strip_prefix(prefix) else {
            continue;
        };
        let digits = rest.trim_start_matches('_');
        if !digits.is_empty()
            && digits.bytes().all(|b| b.is_ascii_digit())
            && let Ok(secs) = digits.parse::<u64>()
        {
            return Some(secs.clamp(1, MAX_RPC_WAIT_SECS));
        }
        if rest.is_empty() || rest.eq_ignore_ascii_case("_X") {
            return value.map(|secs| (secs as u64).clamp(1, MAX_RPC_WAIT_SECS));
        }
    }
    None
}

/// 从 `InvocationError` 中取出服务器要求的等待秒数。
/// Extract the server-requested wait from an `InvocationError`.
fn rpc_wait_secs(err: &InvocationError) -> Option<u64> {
    match err {
        InvocationError::Rpc(rpc) => wait_secs_from_rpc_name(&rpc.name, rpc.value),
        _ => None,
    }
}

/// 通用重试退避：第 N 次失败取 `delays[N]`，超出序列就重复最后一个值。
/// Generic retry backoff: the Nth failure uses `delays[N]`, then repeats the last value.
fn backoff_secs(attempt: u32, delays: &[u64]) -> u64 {
    delays[(attempt as usize).min(delays.len() - 1)]
}

/// 这些错误重试多少次都不会成功，必须立即失败，避免同一个文件被反复上传。
/// These errors are deterministic; retrying only re-uploads the same file again.
fn is_permanent_telegram_error(message: &str) -> bool {
    message.contains("unexpected constructor")
        || message.contains("CHANNEL_INVALID")
        || message.contains("CHAT_INVALID")
        || message.contains("USER_INVALID")
        || message.contains("BOT_METHOD_INVALID")
        || message.contains("PEER_ID_INVALID")
}

/// 上传阶段遇到限流错误时打上标记，让调用方切换到下一个账号。
/// 只标记上传/固化阶段：消息发送阶段的限流留在原账号等待，避免重复发帖。
/// Tag an upload-phase rate limit so the caller can switch accounts. Only upload/commit
/// errors are tagged: send-phase limits stay on the same account to avoid duplicate posts.
const FAILOVER_TAG: &str = "FAILOVER:";

/// 错误文本是否包含 Telegram 限流（FLOOD_WAIT / FLOOD_PREMIUM_WAIT…）。
/// Whether an error message contains a Telegram flood-wait error.
fn is_flood_error(message: &str) -> bool {
    let upper = message.to_ascii_uppercase();
    upper.contains("FLOOD_WAIT") || upper.contains("FLOOD_PREMIUM_WAIT")
}

/// 给上传阶段的限流错误加标记；非限流错误原样返回。
/// Tag upload-phase flood errors; leave other errors unchanged.
fn tag_flood_error(message: String) -> String {
    if message.starts_with(FAILOVER_TAG) || !is_flood_error(&message) {
        message
    } else {
        format!("{}{}", FAILOVER_TAG, message)
    }
}

/// 去掉内部切换标记，保留用户可见的错误文本。
/// Strip the internal failover marker, keeping the user-visible error text.
fn untag_failover(message: &str) -> &str {
    message.strip_prefix(FAILOVER_TAG).unwrap_or(message)
}

/// 一个已构造好的分块 RPC。请求只构造一次，重试时重复借用同一个对象，
/// 不再为每次尝试克隆 512KB 的 `Vec<u8>`。
///
/// A pre-built part RPC. The request is built once and every retry borrows the same object,
/// avoiding a 512KB `Vec<u8>` clone per attempt.
enum UploadPartRequest {
    Big(tl::functions::upload::SaveBigFilePart),
    Small(tl::functions::upload::SaveFilePart),
}

impl UploadPartRequest {
    async fn invoke(&self, client: &Client) -> Result<bool, InvocationError> {
        match self {
            Self::Big(req) => client.invoke(req).await,
            Self::Small(req) => client.invoke(req).await,
        }
    }
}

/// 上传单个分块，失败时按退避序列重试（`file_id` 与分块号都不变，因此可以安全重发）。
///
/// Upload one part, retrying with backoff. Both the `file_id` and the part index stay the same,
/// so resending is safe.
async fn upload_one_part(
    client: &Client,
    file_id: i64,
    part: i32,
    total_parts: i32,
    is_big: bool,
    chunk: Vec<u8>,
) -> Result<(), String> {
    // 单个分块的最大尝试次数 / Max attempts for a single part
    const MAX_ATTEMPTS: u32 = 20;
    // 通用错误的退避序列（秒）/ Backoff sequence for generic errors (seconds)
    const BACKOFF: [u64; 6] = [2, 4, 8, 15, 30, 60];

    let request = if is_big {
        UploadPartRequest::Big(tl::functions::upload::SaveBigFilePart {
            file_id,
            file_part: part,
            file_total_parts: total_parts,
            bytes: chunk,
        })
    } else {
        UploadPartRequest::Small(tl::functions::upload::SaveFilePart {
            file_id,
            file_part: part,
            bytes: chunk,
        })
    };

    let mut attempt = 0u32;
    loop {
        let (err, server_wait) = match request.invoke(client).await {
            Ok(true) => return Ok(()),
            Ok(false) => ("server rejected part".to_string(), None),
            Err(e) => {
                let wait = rpc_wait_secs(&e);
                let message = e.to_string();
                // 单个分块遇到账号级上传限流时不要再等：整次上传换账号重来。
                // An account-level upload flood on one part should not be waited out here;
                // the whole upload will be retried with another session.
                if is_flood_error(&message) {
                    return Err(message);
                }
                (message, wait)
            }
        };

        attempt += 1;
        if attempt >= MAX_ATTEMPTS {
            return Err(format!("part {} upload error: {}", part, err));
        }
        // 服务器明确要求等待时优先按它给出的秒数，避免过早重试。
        // Prefer the server's explicit delay when present so we never retry too early.
        let delay = server_wait.unwrap_or_else(|| backoff_secs(attempt - 1, &BACKOFF));
        eprintln!(
            "chunk {}/{} attempt {}/{}: {}. retrying in {}s…",
            part + 1,
            total_parts,
            attempt,
            MAX_ATTEMPTS,
            err,
            delay
        );
        tokio::time::sleep(Duration::from_secs(delay)).await;
    }
}

/// 一个上传 worker：不断领取下一个分块并上传，直到所有分块都被领完。
///
/// 每个 worker 持有自己的文件句柄，因此并行读取时不会互相干扰 seek 位置。
///
/// One upload worker: claim the next part and upload it until every part has been claimed.
///
/// Each worker owns its file handle, so parallel reads never fight over a shared seek offset.
async fn upload_parts(
    client: &Client,
    path: &Path,
    size: usize,
    file_id: i64,
    total_parts: i32,
    is_big: bool,
    next_part: &AtomicI32,
    progress: &UploadProgress,
) -> Result<(), String> {
    let mut file = tokio::fs::File::open(path)
        .await
        .map_err(|e| format!("open {} failed: {}", path.display(), e))?;

    loop {
        let part = next_part.fetch_add(1, Ordering::Relaxed);
        if part < 0 || part >= total_parts {
            break;
        }

        let offset = part as usize * UPLOAD_CHUNK_SIZE;
        let want = UPLOAD_CHUNK_SIZE.min(size - offset);
        file.seek(SeekFrom::Start(offset as u64))
            .await
            .map_err(|e| format!("seek {} failed: {}", path.display(), e))?;

        let mut chunk = vec![0u8; want];
        let mut read = 0usize;
        while read < want {
            match file.read(&mut chunk[read..]).await {
                Ok(0) => break,
                Ok(n) => read += n,
                Err(e) => return Err(format!("read chunk {} failed: {}", part, e)),
            }
        }
        if read == 0 {
            break;
        }
        chunk.truncate(read);

        upload_one_part(client, file_id, part, total_parts, is_big, chunk).await?;
        progress.add(read);
    }

    Ok(())
}

/// 上传文件到 Telegram：并发分块、断点续传、进度上报。
///
/// 旧实现一次只发一个分块，吞吐量被往返时延锁死：每个 512KB 分块都要等服务器回包，
/// 一个 100MB 的文件就是约 200 次串行往返。现在默认并发上传 16 个分块，把这些往返重叠起来；
/// 每个分块仍然独立重试，因此断点续传的语义完全不变。
///
/// Upload a file to Telegram with concurrent parts, resumable retries and progress reporting.
///
/// The old implementation sent one part at a time, so throughput was pinned to the round-trip
/// time: every 512KB part waited for the server's acknowledgement, which meant roughly 200
/// serial round trips for a 100MB file. Sixteen parts are uploaded concurrently by default so
/// those round trips overlap. Each part still retries on its own, so resumability is unchanged.
async fn upload_with_progress(
    client: &Client,
    session: &SessionIdentity,
    path: &Path,
    done: Arc<AtomicUsize>,
    total: usize,
) -> Result<Uploaded, String> {
    let workers = normalize_upload_workers(pp_utils::param_u32(
        "upload_workers",
        DEFAULT_UPLOAD_WORKERS as u32,
    ) as usize);

    let file_size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    let engine = parse_upload_engine(&param("upload_engine", "wzgram"));
    let use_wzgram = should_use_wzgram(engine, file_size);

    // Make the chosen engine obvious in VPS logs. Previously a broken WZGram
    // sidecar silently fell back to Rust, which made every upload look "slow"
    // even though the fast path was never used.
    if file_size >= BIG_FILE_THRESHOLD as u64 {
        if use_wzgram {
            eprintln!(
                "UPLOAD_ENGINE: wzgram (file={} bytes, workers={})",
                file_size, workers
            );
        } else {
            eprintln!(
                "UPLOAD_ENGINE: rust (file={} bytes, workers={}, requested={:?})",
                file_size, workers, engine
            );
        }
    }

    if use_wzgram {
        let before = done.load(Ordering::Relaxed);
        match upload_via_wzgram(session, path, Arc::clone(&done), total, workers).await {
            Ok(uploaded) => return Ok(uploaded),
            Err(err) => {
                // 限流错误不能靠换上传引擎解决：同一个账号仍然会被限速。
                // 直接标记，让外层切换到下一个账号并从原文件重新上传。
                // A flood limit cannot be fixed by switching engines: the same account is still
                // throttled. Tag it so the caller switches accounts and re-uploads from scratch.
                if is_flood_error(&err) {
                    return Err(tag_flood_error(err));
                }
                // A failed sidecar may have reported partial progress. Rewind it so
                // the Rust fallback does not double-count those bytes.
                eprintln!("UPLOAD_ENGINE: rust (fallback from wzgram)");
                eprintln!("WZGram upload failed: {}. falling back to Rust uploader", err);
                done.store(before, Ordering::Relaxed);
            }
        }
    }

    upload_with_workers(client, path, done, total, workers)
        .await
        .map_err(tag_flood_error)
}

/// 通过 WZGram 旁进程上传文件分块。
///
/// The Python sidecar imports the existing grammers auth key, uploads the file
/// through WZGram and prints `WZ_BYTES:` / `WZ_RESULT:` lines. All Telegram
/// sending logic stays in this Rust module; only the part upload is delegated.
async fn upload_via_wzgram(
    session: &SessionIdentity,
    path: &Path,
    done: Arc<AtomicUsize>,
    total: usize,
    workers: usize,
) -> Result<Uploaded, String> {
    let api_id = session.api_id;
    let session_file = &session.path;
    if !session_file.exists() {
        return Err(format!(
            "WZGram needs an existing grammers session at {} — sign in first",
            session_file.display()
        ));
    }

    let base_dir = env::var("PP_EXE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            env::current_exe()
                .ok()
                .and_then(|p| p.parent().map(|d| d.to_path_buf()))
                .unwrap_or_else(|| PathBuf::from("."))
        });
    let script = base_dir.join("wzgram_upload.py");
    if !script.is_file() {
        return Err(format!("WZGram sidecar not found at {}", script.display()));
    }

    let configured_python = env::var("WZGRAM_PYTHON").unwrap_or_else(|_| {
        if Path::new("/opt/wzgram-venv/bin/python").exists() {
            "/opt/wzgram-venv/bin/python".to_string()
        } else {
            "python3".to_string()
        }
    });
    // WZGram opens its own media-session pool; one session carries two part
    // workers. More sessions only create more requests that get the same
    // FLOOD_PREMIUM_WAIT_X on a throttled non-Premium account, so cap the pool
    // at 8 while keeping enough parallelism to hide WARP round-trip latency.
    let pool = workers.clamp(4, 8);

    let mut child = tokio::process::Command::new(&configured_python)
        .arg(&script)
        .env("PYTHONUNBUFFERED", "1")
        .env("TG_UPLOAD_PATH", path)
        .env("TG_API_ID", api_id.to_string())
        .env("TG_SESSION_PATH", session_file.as_path())
        .env("TG_PROXY", param("proxy", ""))
        .env("TG_WZGRAM_POOL", pool.to_string())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("spawn WZGram sidecar: {}", e))?;

    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| "WZGram sidecar has no stderr pipe".to_string())?;
    let stderr_task = tokio::spawn(async move {
        let mut reader = BufReader::new(stderr);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line).await {
                Ok(0) | Err(_) => break,
                Ok(_) => eprintln!("[wzgram] {}", line.trim_end()),
            }
        }
    });

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "WZGram sidecar has no stdout pipe".to_string())?;
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    let progress = UploadProgress::new(Arc::clone(&done), total);
    let mut reported_bytes = 0usize;
    let mut result: Option<(i64, i32, bool, String)> = None;
    let mut sidecar_error = String::new();

    loop {
        line.clear();
        match reader.read_line(&mut line).await {
            Ok(0) => break,
            Ok(_) => {}
            Err(e) => return Err(format!("read WZGram sidecar stdout: {}", e)),
        }
        let trimmed = line.trim_end();
        if let Some(value) = trimmed.strip_prefix("WZ_BYTES:") {
            let bytes: usize = value
                .parse()
                .map_err(|_| format!("invalid WZ_BYTES value: {}", value))?;
            if bytes > reported_bytes {
                progress.add(bytes - reported_bytes);
                reported_bytes = bytes;
            }
        } else if let Some(value) = trimmed.strip_prefix("WZ_RESULT:") {
            let mut parts = value.split(':');
            let id = parts
                .next()
                .and_then(|v| v.parse::<i64>().ok())
                .ok_or_else(|| format!("invalid WZ_RESULT id: {}", value))?;
            let total_parts = parts
                .next()
                .and_then(|v| v.parse::<i32>().ok())
                .ok_or_else(|| format!("invalid WZ_RESULT parts: {}", value))?;
            let is_big = parts.next() == Some("1");
            let md5_checksum = parts.next().unwrap_or("").to_string();
            result = Some((id, total_parts, is_big, md5_checksum));
        } else if let Some(value) = trimmed.strip_prefix("WZ_ERROR:") {
            sidecar_error = value.to_string();
        } else if !trimmed.is_empty() {
            eprintln!("[wzgram] {}", trimmed);
        }
    }

    let status = child
        .wait()
        .await
        .map_err(|e| format!("wait for WZGram sidecar: {}", e))?;
    let _ = stderr_task.await;

    if !status.success() {
        return Err(if sidecar_error.is_empty() {
            format!("WZGram sidecar exited with {}", status)
        } else {
            sidecar_error
        });
    }
    if !sidecar_error.is_empty() {
        return Err(sidecar_error);
    }
    let (file_id, total_parts, is_big, md5_checksum) = result
        .ok_or_else(|| "WZGram sidecar produced no WZ_RESULT line".to_string())?;

    let size = tokio::fs::metadata(path)
        .await
        .map_err(|e| format!("metadata {} failed: {}", path.display(), e))?
        .len() as usize;
    if reported_bytes < size {
        progress.add(size - reported_bytes);
    }

    // Telegram only knows the file through the parts we uploaded. If WZGram
    // reports a different part count than the file actually needs, the commit
    // would create a 0-byte/corrupt document. Fail loudly so the Rust uploader
    // retries from scratch.
    let expected_parts = size.div_ceil(UPLOAD_CHUNK_SIZE) as i32;
    if total_parts != expected_parts {
        return Err(format!(
            "WZGram part count mismatch: got {}, expected {} ({} bytes)",
            total_parts, expected_parts, size
        ));
    }

    let name = path.file_name().unwrap().to_string_lossy().to_string();
    Ok(Uploaded::from_raw(if is_big {
        tl::types::InputFileBig {
            id: file_id,
            parts: total_parts,
            name,
        }
        .into()
    } else {
        tl::types::InputFile {
            id: file_id,
            parts: total_parts,
            name,
            md5_checksum,
        }
        .into()
    }))
}

/// 与 [`upload_with_progress`] 相同，但可以指定并发分块数（1 = 旧的串行行为）。
///
/// Same as [`upload_with_progress`], but the number of concurrent parts can be chosen
/// (1 reproduces the old serial behaviour).
async fn upload_with_workers(
    client: &Client,
    path: &Path,
    done: Arc<AtomicUsize>,
    total: usize,
    workers: usize,
) -> Result<Uploaded, String> {
    let name = path.file_name().unwrap().to_string_lossy().to_string();

    let size = tokio::fs::metadata(path)
        .await
        .map_err(|e| format!("metadata {} failed: {}", path.display(), e))?
        .len() as usize;
    if size == 0 {
        return Err(format!("{} is empty", path.display()));
    }

    // 与 grammers 保持一致：> 10MB 用大文件接口，否则用小文件接口
    // Match grammers behavior: use big-file API for >10MB, small-file API otherwise.
    // InputMediaUploadedPhoto requires InputFile (small), not InputFileBig — using the wrong
    // one causes PHOTO_SAVE_FILE_INVALID.
    let is_big = size > BIG_FILE_THRESHOLD;
    let total_parts = size.div_ceil(UPLOAD_CHUNK_SIZE) as i32;
    // 生成一次性 file_id，整个上传过程保持不变 / Generate file_id once; keep it for the entire upload
    let file_id = upload_file_id(path);

    // 小文件接口要求整份文件的 md5，先本地顺序读一遍（≤10MB，可以忽略不计）
    // The small-file API needs the whole file's md5, hashed in one local pass (≤10MB, negligible)
    let md5_checksum = if is_big {
        None
    } else {
        Some(md5_of_file(path).await?)
    };

    let progress = UploadProgress::new(done, total);
    let next_part = AtomicI32::new(0);
    // 分块数少于 worker 数时没有意义，多余的 worker 直接不启动
    // More workers than parts is pointless, so only start as many as there is work for
    let workers = normalize_upload_workers(workers)
        .min(total_parts.max(1) as usize)
        .max(1);

    let mut tasks = FuturesUnordered::new();
    for _ in 0..workers {
        tasks.push(upload_parts(
            client,
            path,
            size,
            file_id,
            total_parts,
            is_big,
            &next_part,
            &progress,
        ));
    }
    while let Some(result) = tasks.next().await {
        result?;
    }

    Ok(Uploaded::from_raw(if is_big {
        tl::types::InputFileBig {
            id: file_id,
            parts: total_parts,
            name,
        }
        .into()
    } else {
        tl::types::InputFile {
            id: file_id,
            parts: total_parts,
            name,
            md5_checksum: md5_checksum.unwrap_or_default(),
        }
        .into()
    }))
}

/// 解析本次发送使用的群组话题 ID；未启用话题模式或解析失败时返回 `None`。
/// Resolve the group topic id used for this send; returns `None` when disabled or on failure.
///
/// 话题解析失败（例如机器人没有管理话题权限）不会中断上传，只是退化为普通发送。
/// A topic failure (e.g. the bot lacks the "Manage Topics" right) does not abort the upload;
/// it simply falls back to a normal send.
async fn resolve_topic_id(
    client: &Client,
    target: &Target,
    chat_id: i64,
    model_name: &str,
    enabled: bool,
    mode: SessionMode,
) -> Option<i32> {
    // 本次运行中话题解析已失败过（例如群组不是论坛），不再重复尝试
    // Topic setup already failed during this run (e.g. the group is not a forum); don't retry
    static TOPIC_DISABLED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

    if !enabled || TOPIC_DISABLED.load(std::sync::atomic::Ordering::Relaxed) {
        return None;
    }
    if model_name.trim().is_empty() {
        eprintln!("topic: empty model name, sending without topic");
        return None;
    }
    match ensure_topic(client, target, chat_id, model_name, mode.can_list_topics()).await {
        Ok(id) => Some(id),
        Err(e) => {
            eprintln!("topic setup failed, sending without topic: {}", e);
            TOPIC_DISABLED.store(true, std::sync::atomic::Ordering::Relaxed);
            None
        }
    }
}

/// 把已固化的封面图作为一条独立的图片消息发送（不放进相册），带重试。
///
/// Send an already-committed cover image as a standalone photo message (not part of an
/// album), with retry.
///
/// # 参数 / Parameters
/// - `media`: 已通过 `messages.UploadMedia` 固化的封面图 / Cover committed via `UploadMedia`
/// - `topic_id`: 目标话题（`None` = 群组主聊天）/ Target topic (`None` = the group's main chat)
async fn send_standalone_photo(
    client: &Client,
    peer: PeerRef,
    media: &grammers_client::media::Media,
    topic_id: Option<i32>,
) -> Result<(), String> {
    const MAX_SEND: u32 = 5;
    const SEND_RETRY_DELAY: Duration = Duration::from_secs(30);
    let mut send_attempt = 0u32;
    loop {
        let msg = InputMessage::new()
            .copy_media(media)
            .reply_to(topic_id);
        match client.send_message(peer, msg).await {
            Ok(_) => return Ok(()),
            Err(e) => {
                send_attempt += 1;
                let err = format!("send_message (cover photo) failed: {}", e);
                if is_permanent_telegram_error(&err) {
                    return Err(err);
                }
                if send_attempt >= MAX_SEND {
                    return Err(err);
                }
                eprintln!("cover photo send failed (attempt {}/{}): {}. retrying in {:?}…",
                    send_attempt, MAX_SEND, err, SEND_RETRY_DELAY);
                tokio::time::sleep(SEND_RETRY_DELAY).await;
            }
        }
    }
}

/// 封面图是否单独发一条消息（而不是放进视频相册）。
///
/// 只有「同时发视频」「发封面图」「先发封面图」三项都开启时才单独发送：
/// 只发图片时本来就只有一条图片消息，不需要再拆。
///
/// Whether the cover is posted as its own message instead of inside the video album.
///
/// It only splits when the video, the cover and the "cover first" switch are all on; when
/// the video is off the cover already is a message of its own.
fn cover_sent_separately(send_video: bool, has_cover: bool, cover_first: bool) -> bool {
    send_video && has_cover && cover_first
}

/// 上传单个文件后立即调用 `messages.UploadMedia` 固化到服务端，带重试。
/// 固化后的 Media 不受上传 session TTL 限制（约 10 分钟），可安全地在之后发送。
///
/// Upload a file then immediately commit it via `messages.UploadMedia`, with retry.
/// Committed Media is not subject to the ~10-minute upload session TTL.
async fn commit_media(
    client: &Client,
    peer: &PeerRef,
    raw_media: tl::enums::InputMedia,
) -> Result<grammers_client::media::Media, String> {
    const MAX_RETRIES: u32 = 10;
    // 固定 5/10/20s 起步会让一次瞬时错误凭空多等 5 秒；改成指数退避。
    // A fixed 5/10/20s start added needless latency to transient errors; use exponential backoff.
    const DELAYS: [u64; 6] = [1, 2, 4, 8, 15, 30];
    let mut attempt = 0u32;
    loop {
        let result = client.invoke(&tl::functions::messages::UploadMedia {
            business_connection_id: None,
            peer: (*peer).into(),
            media: raw_media.clone(),
        }).await;
        match result {
            Ok(committed) => {
                return grammers_client::media::Media::from_raw(committed)
                    .ok_or_else(|| "UploadMedia returned unrecognized media type".to_string());
            }
            Err(e) => {
                let message = tag_flood_error(e.to_string());
                if is_permanent_telegram_error(&message) {
                    return Err(format!("UploadMedia failed permanently: {}", message));
                }
                // 固化阶段的限流立刻交给外层切换账号，不在同一个账号上空等一小时。
                // A flood here is the account-level upload limit: switch accounts instead of
                // sleeping through a one-hour wait on the same session.
                if message.starts_with(FAILOVER_TAG) {
                    return Err(message);
                }
                let server_wait = rpc_wait_secs(&e);
                attempt += 1;
                if attempt >= MAX_RETRIES {
                    return Err(format!("UploadMedia failed after {} attempts: {}", attempt, message));
                }
                let delay = server_wait.unwrap_or_else(|| backoff_secs(attempt - 1, &DELAYS));
                eprintln!("UploadMedia attempt {}/{} failed: {}. retrying in {}s…",
                    attempt, MAX_RETRIES, e, delay);
                tokio::time::sleep(Duration::from_secs(delay)).await;
            }
        }
    }
}

/// 核心异步函数：在已建立的连接上上传文件并发送消息。
/// Core async function: upload files and send messages on an established connection.
///
/// 流程：
/// 1. 解析目标会话
/// 2. 转换封面图格式（若需要）
/// 3. 转换/重封装视频格式（若需要）
/// 4. 上传封面图并立即固化（顺序仍早于视频）
/// 5. 解析/创建话题
/// 6. 并行上传视频缩略图和视频文件并固化
/// 7. `cover_first` 开启时，先把封面图单独作为一条图片消息发出，再发送视频相册（每批最多 10 条）
///
/// 封面图和文字说明都可单独关闭：两者都关时相册里只有视频，消息里没有任何文字。
/// `cover_first` 关闭时封面图和视频回到同一条相册消息里（旧行为）。
///
/// Flow:
/// 1. Resolve the target peer
/// 2. Convert cover image format if needed
/// 3. Convert/remux video format if needed
/// 4. Upload the cover image and commit it immediately (still before the video)
/// 5. Resolve/create the topic
/// 6. Upload video thumbnails and files and commit them
/// 7. With `cover_first` on, post the cover as its own photo message, then send the videos as an
///    album (max 10 per batch)
///
/// The cover image and the text caption can each be switched off; with both off the album holds
/// the videos only and the message carries no text at all. With `cover_first` off the cover
/// goes back inside the video album (the previous behaviour).
#[allow(clippy::too_many_arguments)]
async fn upload_and_send(
    client: &Client, session: &SessionIdentity,
    chat_id: i64, username: &str,
    model_name: &str, timestamp: &str, duration_str: &str,
    file_name: &str, file_size_str: &str,
    input: &Path, cover: Option<&Path>, send_video: bool,
    topic_per_model: bool, effective_parts: &[PathBuf], workspace: Option<&Path>,
    send_caption: bool, send_cover: bool, cover_first: bool,
) -> Result<(), String> {
    // 文字说明是可选的：关闭后消息里不含任何文字，只有视频（或图片）
    // The caption is optional: when off the message carries no text at all, only media
    let caption: Option<(String, Vec<tl::enums::MessageEntity>)> = if send_caption {
        Some(build_caption(model_name, timestamp, duration_str, file_name, file_size_str, None))
    } else {
        None
    };

    // 关闭封面图时直接当作「没有封面」，上传、相册和「只发图」分支都不再涉及它
    // With the cover switched off, treat it as absent everywhere: upload, album and photo branch
    let cover = if send_cover { cover } else { None };

    let target = resolve_target(client, chat_id, username).await?;
    let peer = target.peer;

    // 处理封面图：非 jpg/png 格式需先用 ffmpeg 转换，然后检查尺寸限制
    // Handle cover image: non-jpg/png formats need ffmpeg conversion first, then check dimension limits
    let converted_cover: Option<PathBuf>;
    let resized_cover: Option<PathBuf>;
    let effective_cover: Option<&Path>;
    if let Some(img) = cover {
        let ext = img.extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase();
        if matches!(ext.as_str(), "jpg" | "jpeg" | "png") {
            converted_cover = None;
        } else {
            let tmp = tmp_dir().join(format!(
                "{}_tg_tmp.png",
                img.file_stem().and_then(|s| s.to_str()).unwrap_or("cover")
            ));
            let status = Command::new("ffmpeg")
                .args(["-y", "-i"]).arg(img).arg(&tmp)
                .stdout(Stdio::null()).stderr(Stdio::null())
                .status().map_err(|e| format!("ffmpeg not found: {}", e))?;
            if !status.success() { return Err("ffmpeg failed to convert cover image".to_string()); }
            converted_cover = Some(tmp);
        }
        let after_format: &Path = converted_cover.as_deref().unwrap_or(img);
        // 检查并等比缩放封面图以满足 Telegram 尺寸限制
        // Check and proportionally resize cover image to meet Telegram dimension limits
        resized_cover = resize_cover_for_telegram(after_format)?;
        effective_cover = resized_cover.as_deref().or(Some(after_format));
    } else {
        converted_cover = None;
        resized_cover = None;
        effective_cover = None;
    }

    // 提取每个视频片段的缩略图和元数据 / Extract thumbnail and metadata for each video part
    struct PartMeta { thumb_path: Option<PathBuf>, duration: f64, w: i32, h: i32 }
    let part_metas: Vec<PartMeta> = if send_video {
        effective_parts.iter().map(|p| {
            let thumb_path = workspace.and_then(|dir| extract_video_thumbnail(p, dir).ok());
            let (dur, w, h) = video_meta(p).unwrap_or((0.0, 1280, 720));
            PartMeta { thumb_path, duration: dur, w, h }
        }).collect()
    } else { vec![] };

    // 计算所有需要上传的文件总大小（用于进度计算）
    // Calculate total size of all files to upload (for progress calculation)
    let cover_size = effective_cover.and_then(|p| fs::metadata(p).ok()).map(|m| m.len() as usize).unwrap_or(0);
    let video_size: usize = if send_video {
        effective_parts.iter().map(|p| fs::metadata(p).ok().map(|m| m.len() as usize).unwrap_or(0)).sum()
    } else { 0 };
    let thumb_size: usize = part_metas.iter()
        .filter_map(|m| m.thumb_path.as_ref())
        .filter_map(|p| fs::metadata(p).ok())
        .map(|m| m.len() as usize).sum();
    let upload_total = cover_size + video_size + thumb_size;

    // 共享的已上传字节计数器 / Shared uploaded bytes counter
    let done = Arc::new(AtomicUsize::new(0));

    if send_video {
        // 固化后的视频分片，包含服务端永久 Media 引用和元数据
        // Committed video parts: server-side permanent Media reference + metadata
        struct CommittedPart {
            media: grammers_client::media::Media,  // 已通过 UploadMedia 固化 / committed via UploadMedia
        }

        // 上传单个文件后立即调用 messages.UploadMedia 固化到服务端（顶层函数 `commit_media`）
        // Committed via the top-level `commit_media` helper below

        // 上传封面图并立即固化 / Upload cover and commit immediately
        let committed_cover: Option<grammers_client::media::Media> = if let Some(img) = effective_cover {
            let uploaded = upload_with_progress(client, session, img, Arc::clone(&done), upload_total).await?;
            let raw: tl::enums::InputMedia = tl::types::InputMediaUploadedPhoto {
                spoiler: false,
                file: uploaded.raw,
                stickers: None,
                ttl_seconds: None,
                live_photo: false,
                video: None,
            }.into();
            Some(commit_media(client, &peer, raw).await?)
        } else { None };
        if let Some(ref tmp) = converted_cover { let _ = fs::remove_file(tmp); }
        if let Some(ref tmp) = resized_cover { let _ = fs::remove_file(tmp); }

        // 解析/创建该模特对应的群组话题；失败时退化为普通发送，不影响视频上传
        // Resolve/create the group topic for this model; on failure fall back to a normal send
        let topic_id = resolve_topic_id(client, &target, chat_id, model_name, topic_per_model, session.mode).await;

        // 封面图先上传固化（上面完成），但独立消息放到视频上传之后再发：
        // 上传阶段若触发限流需要换账号重试，此时群里还没有任何消息，切换不会造成重复发帖。
        // The cover is uploaded and committed first (above), but its standalone message is
        // posted after the video upload: an upload-phase flood triggers an account switch
        // before anything is posted, so failover cannot duplicate messages.
        let cover_in_album =
            !cover_sent_separately(send_video, committed_cover.is_some(), cover_first);

        // 串行上传每个视频片段及其缩略图，上传后立即固化。
        // Serial upload + immediate commit for each video part and its thumbnail.
        let do_upload_and_commit = |done: Arc<AtomicUsize>| {
            let client = client;
            let part_metas = &part_metas;
            let peer = &peer;
            async move {
                let mut parts: Vec<CommittedPart> = Vec::new();
                for (part_path, meta) in effective_parts.iter().zip(part_metas.iter()) {
                    // 缩略图很小：与视频上传并行，省掉一次串行 RPC 往返。
                    // The thumbnail is tiny; upload it alongside the video to save one RTT.
                    let thumb_fut = async {
                        if let Some(tp) = meta.thumb_path.as_ref() {
                            let t = upload_with_progress(client, session, tp, Arc::clone(&done), upload_total).await;
                            let _ = fs::remove_file(tp);
                            t.map(Some)
                        } else {
                            Ok::<Option<Uploaded>, String>(None)
                        }
                    };
                    let video_fut = upload_with_progress(client, session, part_path, Arc::clone(&done), upload_total);
                    let (thumb_uploaded, video_uploaded) = tokio::try_join!(thumb_fut, video_fut)?;

                    // 直接构建 InputMediaUploadedDocument，避免访问私有字段
                    // Build InputMediaUploadedDocument directly to avoid accessing private fields
                    let ext = part_path.extension().and_then(|e| e.to_str()).unwrap_or("mp4").to_lowercase();
                    let mime = if ext == "mkv" { "video/x-matroska" } else { "video/mp4" };
                    let raw_media: tl::enums::InputMedia = tl::types::InputMediaUploadedDocument {
                        nosound_video: false,
                        force_file: false,
                        spoiler: false,
                        file: video_uploaded.raw,
                        thumb: thumb_uploaded.map(|t| t.raw),
                        mime_type: mime.to_string(),
                        attributes: vec![
                            tl::types::DocumentAttributeFilename {
                                file_name: part_path.file_name().unwrap().to_string_lossy().to_string()
                            }.into(),
                            tl::types::DocumentAttributeVideo {
                                round_message: false,
                                supports_streaming: true,
                                nosound: false,
                                duration: meta.duration,
                                w: meta.w,
                                h: meta.h,
                                preload_prefix_size: None,
                                video_start_ts: None,
                                video_codec: None,
                            }.into(),
                        ],
                        stickers: None,
                        ttl_seconds: None,
                        video_cover: None,
                        video_timestamp: None,
                    }.into();

                    let committed = commit_media(client, peer, raw_media).await?;
                    parts.push(CommittedPart { media: committed });
                }
                Ok::<_, String>(parts)
            }
        };

        let mut committed_parts = do_upload_and_commit(Arc::clone(&done)).await?;

        // 所有上传完成后才发封面图独立消息，然后发送视频。
        // Post the standalone cover only after every upload succeeded, then send the video.
        if let Some(ref cover_media) = committed_cover
            && !cover_in_album
        {
            send_standalone_photo(client, peer, cover_media, topic_id).await?;
            println!("      cover image sent as its own message, uploading the video next…");
        }

        let total_parts = committed_parts.len();

        // 相册里要放的封面图：已经单独发过时为 None（视频相册里不再重复）
        // The cover to include in the album: `None` once it has been posted on its own
        let album_cover: Option<grammers_client::media::Media> =
            if cover_in_album { committed_cover.clone() } else { None };

        // 辅助函数：从固化的 Media 重建 InputMedia 列表（用于发送重试）
        // Helper: rebuild InputMedia list from committed Media (for send retry)
        let build_items = |cover: &Option<grammers_client::media::Media>,
                           parts: &Vec<CommittedPart>| -> Vec<InputMedia> {
            let mut items: Vec<InputMedia> = Vec::new();
            if let Some(c) = cover {
                items.push(InputMedia::new().copy_media(c));
            }
            for (idx, part) in parts.iter().enumerate() {
                let mut item = InputMedia::new().copy_media(&part.media);
                if send_caption && total_parts > 1 {
                    let (text, entities) = build_caption(
                        model_name, timestamp, duration_str, file_name, file_size_str,
                        Some((idx + 1, total_parts)),
                    );
                    item = item.fmt_entities(entities).caption(text);
                } else if let Some((text, entities)) = &caption {
                    item = item.fmt_entities(entities.clone()).caption(text.clone());
                }
                items.push(item);
            }
            items
        };

        // 每批最多 10 条发送相册。固化后的 InputMedia 不会有 FILE_PART_MISSING，
        // 但仍保留重传逻辑以防万一。
        // Send album in batches of max 10. Committed InputMedia won't get FILE_PART_MISSING,
        // but keep re-upload logic as a safety net.
        const MAX_ALBUM: usize = 10;
        const MAX_SEND: u32 = 5;
        const MAX_REUPLOAD: u32 = 3;
        const SEND_RETRY_DELAY: Duration = Duration::from_secs(30);

        let mut reupload_count = 0u32;
        loop {
            let n_batches = build_items(&album_cover, &committed_parts).len().div_ceil(MAX_ALBUM);
            let mut need_reupload = false;
            let mut send_err = String::new();

            'batches: for batch_idx in 0..n_batches {
                let start = batch_idx * MAX_ALBUM;
                let mut send_attempt = 0u32;
                loop {
                    let mut batch: Vec<InputMedia> = build_items(&album_cover, &committed_parts)
                        .into_iter().skip(start).take(MAX_ALBUM).collect();
                    // 话题模式：每批相册都作为该话题内的消息发送（send_album 只使用首项的 reply_to）
                    // Topic mode: every album batch is posted inside the topic
                    // (send_album only uses the first item's reply_to)
                    if let Some(tid) = topic_id
                        && !batch.is_empty()
                    {
                        let first = batch.remove(0);
                        batch.insert(0, first.reply_to(Some(tid)));
                    }
                    match client.send_album(peer, batch).await {
                        Ok(_) => break,
                        Err(e) => {
                            let msg = format!("send_album (batch {}) failed: {}", batch_idx + 1, e);
                            if is_permanent_telegram_error(&msg) {
                                return Err(msg);
                            }
                            if msg.contains("FILE_PART_MISSING") {
                                need_reupload = true;
                                send_err = msg;
                                break 'batches;
                            }
                            send_attempt += 1;
                            if send_attempt >= MAX_SEND {
                                invalidate_topic_on_error(chat_id, model_name, &msg);
                                return Err(msg);
                            }
                            eprintln!("send failed (attempt {}/{}): {}. retrying in {:?}…",
                                send_attempt, MAX_SEND, msg, SEND_RETRY_DELAY);
                            tokio::time::sleep(SEND_RETRY_DELAY).await;
                        }
                    }
                }
            }

            if !need_reupload { break; }

            reupload_count += 1;
            if reupload_count > MAX_REUPLOAD {
                return Err(format!("{} (re-upload limit reached)", send_err));
            }
            eprintln!("FILE_PART_MISSING, re-uploading all files (attempt {}/{})…",
                reupload_count, MAX_REUPLOAD);
            done.store(0, Ordering::Relaxed);
            committed_parts = do_upload_and_commit(Arc::clone(&done)).await?;
        }
    } else if let Some(img) = effective_cover {
        // 仅发送封面图（文字说明关闭时就是一张纯图片）
        // Send the cover image only (a bare photo when the caption is off)
        let uploaded = upload_with_progress(client, session, img, Arc::clone(&done), upload_total).await?;
        if let Some(ref tmp) = converted_cover { let _ = fs::remove_file(tmp); }
        if let Some(ref tmp) = resized_cover { let _ = fs::remove_file(tmp); }
        let topic_id = resolve_topic_id(client, &target, chat_id, model_name, topic_per_model, session.mode).await;
        const MAX_SEND: u32 = 5;
        const SEND_RETRY_DELAY: Duration = Duration::from_secs(30);
        let mut send_attempt = 0u32;
        loop {
            let mut msg = InputMessage::new()
                .photo(uploaded.clone())
                .reply_to(topic_id);
            if let Some((text, entities)) = &caption {
                msg = msg.fmt_entities(entities.clone()).text(text.clone());
            }
            match client.send_message(peer, msg).await {
                Ok(_) => break,
                Err(e) => {
                    send_attempt += 1;
                    let err = format!("send_message (photo) failed: {}", e);
                    if is_permanent_telegram_error(&err) {
                        return Err(err);
                    }
                    if send_attempt >= MAX_SEND {
                        invalidate_topic_on_error(chat_id, model_name, &err);
                        return Err(err);
                    }
                    eprintln!("send failed (attempt {}/{}): {}. retrying in {:?}…",
                        send_attempt, MAX_SEND, err, SEND_RETRY_DELAY);
                    tokio::time::sleep(SEND_RETRY_DELAY).await;
                }
            }
        }
    } else if let Some((text, entities)) = &caption {
        // 无视频、无封面图：只发送文字消息，发送失败立即重试（不重新上传）
        // No video and no cover: send the text message only; retry without re-uploading
        let topic_id = resolve_topic_id(client, &target, chat_id, model_name, topic_per_model, session.mode).await;
        const MAX_SEND: u32 = 3;
        const SEND_RETRY_DELAY: Duration = Duration::from_secs(30);
        let mut send_attempt = 0u32;
        loop {
            let msg = InputMessage::new()
                .fmt_entities(entities.clone())
                .reply_to(topic_id)
                .text(text.clone());
            match client.send_message(peer, msg).await {
                Ok(_) => break,
                Err(e) => {
                    send_attempt += 1;
                    let err = format!("send_message (text) failed: {}", e);
                    if is_permanent_telegram_error(&err) {
                        return Err(err);
                    }
                    if send_attempt >= MAX_SEND {
                        invalidate_topic_on_error(chat_id, model_name, &err);
                        return Err(err);
                    }
                    eprintln!("send failed (attempt {}/{}): {}. retrying in {:?}…",
                        send_attempt, MAX_SEND, err, SEND_RETRY_DELAY);
                    tokio::time::sleep(SEND_RETRY_DELAY).await;
                }
            }
        }
    } else {
        // 视频、封面图和文字全关：没有任何内容可发（read_config 已提前拦截，这里兜底）
        // Video, cover and caption all off: nothing to send (read_config catches this first)
        return Err(
            "nothing to send: send_video, send_cover and send_caption are all off".to_string()
        );
    }

    Ok(())
}

// ─── 参数与自检模式 / Parameters and self-test mode ──────────────────────────

/// 模块参数集合 / Module parameter set.
struct TgConfig {
    api_id: i32,
    api_hash: String,
    bot_token: String,
    chat_id: i64,
    proxy: String,
    username: String,
    send_video: bool,
    /// 是否附带文字说明 / Whether to attach the text caption
    send_caption: bool,
    /// 是否同时发送封面图 / Whether to also send the cover image
    send_cover: bool,
    /// 封面图是否先单独作为一条消息发出（再发视频）/ Whether the cover is posted on its own first
    cover_first: bool,
    topic_per_model: bool,
    /// 登录方式（机器人 / 用户账号）/ Login mode (bot / user account)
    mode: SessionMode,
    /// 发送成功后是否请求主程序删除本地数据 / Whether to ask the host to delete local data after sending
    delete_after_send: bool,
}

/// 规范化 Chat ID：去掉首尾空格后必须是数字。
///
/// 超级群组形如 `-100xxxxxxxxxx`；普通群是负数（最多 12 位）；用户是正数。
///
/// Normalize the Chat ID: trim it and require a number.
///
/// Supergroups look like `-100xxxxxxxxxx`, basic groups are negative (up to 12 digits) and
/// users are positive.
fn normalize_chat_id(raw: &str) -> Result<i64, String> {
    let s = raw.trim();
    if s.is_empty() {
        return Err("chat_id is required".to_string());
    }
    s.parse().map_err(|_| {
        "chat_id must be a number (a supergroup id looks like -100xxxxxxxxxx)".to_string()
    })
}

/// 规范化 Username：去掉首尾空格与用户习惯输入的 `@`。
/// Normalize the username: trim it and drop the leading `@` people copy along with it.
fn normalize_username(raw: &str) -> String {
    raw.trim().trim_start_matches('@').to_string()
}

/// 校验「至少要有一样东西可发」：视频、封面图、文字说明不能同时关闭。
/// 纯函数，方便单元测试（`read_config` 读的是环境变量）。
///
/// Validate that there is at least one thing to send: the video, the cover image and the text
/// caption cannot all be off. Pure so it can be unit tested (`read_config` reads env vars).
fn validate_payload_flags(
    send_video: bool,
    send_caption: bool,
    send_cover: bool,
) -> Result<(), String> {
    if !send_video && !send_caption && !send_cover {
        return Err(
            "nothing to send: send_video, send_caption and send_cover are all off — turn on at \
             least one of them"
                .to_string(),
        );
    }
    Ok(())
}

/// 读取并验证所有模块参数（正常模式与自检模式共用）。
/// Read and validate all module parameters (shared by normal and self-test mode).
fn read_config() -> Result<TgConfig, String> {
    let api_id: i32 = {
        let s = param("api_id", "");
        if s.is_empty() { return Err("api_id is required".to_string()); }
        s.parse().map_err(|_| "api_id must be a number".to_string())?
    };
    let api_hash  = param("api_hash", "");
    // 已登录的用户会话不需要 api_hash / A signed-in user session needs no api_hash
    let session_label = param("session_label", "");
    let mode = SessionMode::from_param(&param("account", "auto"))
        .resolve_with_label(api_id, &session_label);
    let bot_token = param("bot_token", "");
    if mode == SessionMode::Bot {
        // 先报「没有任何可用账号」——这是最常见的配置错误，api_hash 只是它的附带项
        // Report "no usable account" first: it is the usual mistake, and api_hash is secondary
        if bot_token.is_empty() {
            return Err(format!(
                "no Telegram account to post with: api_id {} has no signed-in user session, and \
                 no bot_token either — click \"Login\" to sign in with your Telegram account, or \
                 fill in the Bot Token (plus the API Hash)",
                api_id
            ));
        }
        if api_hash.is_empty() {
            return Err("api_hash is required for a bot account".to_string());
        }
    }
    let chat_id: i64 = normalize_chat_id(&param("chat_id", ""))?;

    let send_video = param_bool("send_video", true);
    // 文字说明默认关闭：只上传视频，不往群里发任何文字
    // The text caption is off by default: upload the video, post no text at all
    let send_caption = param_bool("send_caption", false);
    // 封面图默认开启 / The cover image is on by default
    let send_cover = param_bool("send_cover", true);
    // 默认先把封面图单独发出去，再上传视频 / By default the cover is posted on its own first
    let cover_first = param_bool("cover_first", true);
    // 三项全关 = 没有任何内容可发，提前报错而不是连上 Telegram 后才发现
    // All three off means there is nothing to send; fail early instead of after connecting
    validate_payload_flags(send_video, send_caption, send_cover)?;

    Ok(TgConfig {
        api_id,
        api_hash,
        bot_token,
        chat_id,
        proxy: param("proxy", ""),
        username: normalize_username(&param("username", "")),
        send_video,
        send_caption,
        send_cover,
        cover_first,
        // 是否把每个模特的视频发送到该模特专属的群组话题中
        // Whether to send each model's video into a group forum topic dedicated to that model
        topic_per_model: param_bool("topic_per_model", true),
        // 上传成功后是否删除本地数据（默认开启，用于释放 VPS 空间）
        // Whether to delete local data after a successful upload (on by default to free VPS space)
        delete_after_send: param_bool("delete_after_send", true),
        mode,
    })
}

/// 自检模式下使用的话题名称（固定名称，重复测试复用同一话题，不会重复创建）。
/// Topic title used in self-test mode (fixed so repeated tests reuse the same topic).
const TEST_TOPIC_NAME: &str = "Test";

/// 生成一个 3 秒的测试视频（含音轨），用于验证完整上传链路。
/// Generate a 3-second test clip (with audio) to verify the whole upload path.
fn make_test_clip() -> Result<PathBuf, String> {
    let out = tmp_dir().join("telegram_selftest.mp4");

    // 优先 H.264（Telegram 客户端兼容性最好），失败则回退 mpeg4
    // Prefer H.264 (best Telegram client compatibility), fall back to mpeg4
    let codecs: [&[&str]; 2] = [
        &["-c:v", "libx264", "-preset", "ultrafast", "-pix_fmt", "yuv420p", "-c:a", "aac"],
        &["-c:v", "mpeg4", "-pix_fmt", "yuv420p", "-c:a", "aac"],
    ];

    for codec_args in codecs {
        let _ = fs::remove_file(&out);
        let status = Command::new("ffmpeg")
            .args([
                "-y",
                "-f", "lavfi", "-i", "testsrc=duration=3:size=640x360:rate=15",
                "-f", "lavfi", "-i", "sine=frequency=440:duration=3",
            ])
            .args(codec_args)
            .args(["-shortest", "-movflags", "+faststart"])
            .arg(&out)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map_err(|e| format!("ffmpeg not found: {}", e))?;

        let ok = status.success()
            && fs::metadata(&out).map(|m| m.len() > 0).unwrap_or(false);
        if ok {
            return Ok(out);
        }
    }
    Err("ffmpeg failed to generate the test clip".to_string())
}

/// 自检用的封面图：从测试视频里截一帧，验证「先发封面图」链路。
/// Cover image used by the self-test: one frame of the test clip, so the "cover first" path is
/// actually exercised by the Test button.
fn make_test_cover(clip: &Path) -> Result<PathBuf, String> {
    let out = tmp_dir().join("telegram_selftest_cover.png");
    let _ = fs::remove_file(&out);
    let status = Command::new("ffmpeg")
        .args(["-y", "-ss", "1", "-i"])
        .arg(clip)
        .args(["-frames:v", "1"])
        .arg(&out)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|e| format!("ffmpeg not found: {}", e))?;
    if !status.success() || !out.exists() {
        return Err("ffmpeg failed to generate the test cover image".to_string());
    }
    Ok(out)
}

/// 自检主流程：连接 → 解析目标会话 → 解析/创建话题 → 上传并发送测试视频（或文字）。
/// 任一步骤失败都会返回错误，便于在界面上定位问题。
///
/// Self-test flow: connect → resolve target chat → resolve/create topic →
/// upload and send the test clip (or a text message).
/// Any failing step returns an error so the problem is visible in the UI.
async fn self_test(cfg: &TgConfig) -> Result<(), String> {
    println!(
        "[1/5] connecting to Telegram as {} ({})…",
        cfg.mode.label(),
        session_path(cfg.api_id, cfg.mode).display()
    );

    let session = primary_session(cfg.api_id, cfg.mode, &param("session_label", ""));
    let client = connect_client(
        cfg.api_id,
        &cfg.api_hash,
        &cfg.bot_token,
        &cfg.proxy,
        session.mode,
        Some(&session.path),
    )
    .await?;
    println!("      connected (session: {})", session.display_name());

    let target = resolve_target(&client, cfg.chat_id, &cfg.username).await?;
    // 打印 Telegram 眼中的这个会话：群还是频道、是否已开启话题
    // Show what Telegram thinks this chat is: group vs channel, topics on or off
    println!("[2/5] target chat resolved: {}", target.describe());
    let peer = target.peer;

    // 话题失败在自检中视为失败（正常发送时会退化为普通消息，但自检需要暴露问题）
    // A topic failure is fatal in the self-test (normal runs fall back to a plain message,
    // but the self-test must surface the problem)
    let topic_id = if cfg.topic_per_model {
        println!("[3/5] resolving topic \"{}\"…", TEST_TOPIC_NAME);
        if !cfg.mode.can_list_topics() {
            println!(
                "      note: bot accounts cannot list forum topics (BOT_METHOD_INVALID), \
                 so only the local topic cache is used"
            );
        }
        let id = ensure_topic(
            &client,
            &target,
            cfg.chat_id,
            TEST_TOPIC_NAME,
            cfg.mode.can_list_topics(),
        )
        .await
        .map_err(|e| format!("topic setup failed: {}", e))?;
        println!("      topic ready (id {})", id);
        Some(id)
    } else {
        println!("[3/5] topic mode is off, sending to the main chat");
        None
    };

    // 自检消息同样遵守 send_caption：关闭时不带任何文字，这样自检就能验证「只发视频」
    // The self-test honours send_caption too: with it off no text is posted, so a passing
    // self-test proves video-only sending works end to end.
    let caption = cfg.send_caption.then(|| {
        format!(
            "🧪 Test message from StripchatRecorder\n\n\
             If you can read this, the {} can post to this chat{}.\n\
             Mode: {}",
            if cfg.mode == SessionMode::User { "account" } else { "bot" },
            if topic_id.is_some() { " and create/reuse forum topics" } else { "" },
            if cfg.send_video { "text + video upload" } else { "text only (send_video is off)" },
        )
    });

    if cfg.send_video {
        println!("[4/5] generating a 3-second test clip…");
        let clip = make_test_clip()?;
        let size = fs::metadata(&clip).map(|m| m.len() as usize).unwrap_or(0);
        let (duration, w, h) = video_meta(&clip).unwrap_or((3.0, 640, 360));
        let thumb_path = extract_video_thumbnail(&clip, &tmp_dir()).ok();
        let thumb_size = thumb_path
            .as_ref()
            .and_then(|p| fs::metadata(p).ok())
            .map(|m| m.len() as usize)
            .unwrap_or(0);

        // 封面图：send_cover 开启时截一帧，cover_first 开启时先单独把它发出去，
        // 与真实发送流程完全一致（这样「测试」按钮也能验证新链路）。
        //
        // Cover: grab one frame when send_cover is on, and with cover_first on post it on its
        // own first — exactly like a real send, so the Test button covers the new path too.
        let cover_path = if cfg.send_cover { make_test_cover(&clip).ok() } else { None };
        let cover_size = cover_path
            .as_ref()
            .and_then(|p| fs::metadata(p).ok())
            .map(|m| m.len() as usize)
            .unwrap_or(0);

        let total = size + thumb_size + cover_size;
        let done = Arc::new(AtomicUsize::new(0));

        if let Some(ref cp) = cover_path
            && cover_sent_separately(true, true, cfg.cover_first)
        {
            println!("[5/5] uploading the cover image first ({})…", format_bytes(cover_size as u64));
            let uploaded =
                upload_with_progress(&client, &session, cp, Arc::clone(&done), total).await?;
            let raw: tl::enums::InputMedia = tl::types::InputMediaUploadedPhoto {
                spoiler: false,
                file: uploaded.raw,
                stickers: None,
                ttl_seconds: None,
                live_photo: false,
                video: None,
            }.into();
            let media = commit_media(&client, &peer, raw).await?;
            let msg = InputMessage::new().copy_media(&media).reply_to(topic_id);
            client.send_message(peer, msg).await
                .map_err(|e| format!("send_message (cover) failed: {}", e))?;
            println!("      test cover image sent first (cover_first is on)");
        }

        println!("[5/5] uploading {} ({})…", clip.display(), format_bytes(size as u64));
        let thumb_fut = async {
            match thumb_path.as_ref() {
                Some(tp) => {
                    let t = upload_with_progress(&client, &session, tp, Arc::clone(&done), total).await;
                    t.map(Some)
                }
                None => Ok::<Option<Uploaded>, String>(None),
            }
        };
        let video_fut = upload_with_progress(&client, &session, &clip, Arc::clone(&done), total);
        let (thumb_uploaded, video_uploaded) = tokio::try_join!(thumb_fut, video_fut)?;

        println!("      upload finished, committing media…");
        let raw_media: tl::enums::InputMedia = tl::types::InputMediaUploadedDocument {
            nosound_video: false,
            force_file: false,
            spoiler: false,
            file: video_uploaded.raw,
            thumb: thumb_uploaded.map(|t| t.raw),
            mime_type: "video/mp4".to_string(),
            attributes: vec![
                tl::types::DocumentAttributeFilename {
                    file_name: clip.file_name().unwrap().to_string_lossy().to_string(),
                }.into(),
                tl::types::DocumentAttributeVideo {
                    round_message: false,
                    supports_streaming: true,
                    nosound: false,
                    duration,
                    w,
                    h,
                    preload_prefix_size: None,
                    video_start_ts: None,
                    video_codec: None,
                }.into(),
            ],
            stickers: None,
            ttl_seconds: None,
            video_cover: None,
            video_timestamp: None,
        }.into();

        let committed = commit_media(&client, &peer, raw_media).await?;

        let mut item = InputMedia::new().copy_media(&committed);
        if let Some(text) = &caption {
            item = item.caption(text.clone());
        }
        if let Some(tid) = topic_id {
            item = item.reply_to(Some(tid));
        }
        client.send_album(peer, vec![item]).await
            .map_err(|e| format!("send_album failed: {}", e))?;
        if caption.is_some() {
            println!("      test video sent");
        } else {
            println!("      test video sent (no caption: send_caption is off)");
        }

        let _ = fs::remove_file(&clip);
        if let Some(tp) = thumb_path { let _ = fs::remove_file(tp); }
        if let Some(cp) = cover_path { let _ = fs::remove_file(cp); }
    } else if let Some(text) = caption {
        println!("[4/5] video upload is off, sending a text message…");
        let msg = InputMessage::new().reply_to(topic_id).text(text);
        client.send_message(peer, msg).await
            .map_err(|e| format!("send_message failed: {}", e))?;
        println!("      test message sent");
    } else {
        // 视频和文字都关：自检没有任何东西可发
        // Video and caption both off: the self-test has nothing to send
        return Err(
            "nothing to send in the self-test: send_video and send_caption are both off".to_string()
        );
    }

    Ok(())
}

/// 自检模式入口（`PP_TEST=1`）：不需要 `PP_INPUT`，只验证凭据、话题和上传链路。
/// Self-test entry point (`PP_TEST=1`): no `PP_INPUT`; verifies credentials, topic and upload path.
fn run_test() -> Result<(), String> {
    let cfg = read_config()?;

    let result = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("tokio runtime error: {}", e))?
        .block_on(self_test(&cfg));

    // 无论成功与否都清理生成的测试文件（失败时也不会残留）
    // Clean up the generated test files either way (nothing is left behind on failure)
    let _ = fs::remove_file(tmp_dir().join("telegram_selftest.mp4"));
    let _ = fs::remove_file(tmp_dir().join("telegram_selftest.tg_thumb.png"));
    let _ = fs::remove_file(tmp_dir().join("telegram_selftest_cover.png"));

    result?;
    println!("SELF-TEST PASSED");
    Ok(())
}

/// 登录行协议的状态前缀 / Prefix used by login state lines.
const LOGIN_STATE_PREFIX: &str = "LOGIN_STATE:";

/// 向标准输出打印一条登录状态（立即刷新，宿主机按行读取）。
/// Print one login state line to stdout (flushed immediately; the host reads lines).
fn login_state(state: &str) {
    use std::io::Write;
    println!("{}{}", LOGIN_STATE_PREFIX, state);
    let _ = std::io::stdout().flush();
}

/// 登录模式使用的 stdin 行读取器（整个登录过程复用同一个缓冲区）。
/// Line reader used by login mode (the same buffer is reused for the whole login).
type LoginStdin = tokio::io::Lines<tokio::io::BufReader<tokio::io::Stdin>>;

/// 创建登录模式的 stdin 行读取器。
/// Create the line reader used by login mode.
fn login_stdin() -> LoginStdin {
    use tokio::io::AsyncBufReadExt;
    tokio::io::BufReader::new(tokio::io::stdin()).lines()
}

/// 从 stdin 读取一行命令（`命令:值`），去掉首尾空白；EOF 返回 `None`。
///
/// 必须复用同一个读取器：每次新建 `BufReader` 会丢弃已经缓冲的后续行。
///
/// Read one command line from stdin (`command:value`), trimmed; `None` on EOF.
///
/// The same reader must be reused: creating a new `BufReader` each time would discard
/// lines that are already buffered.
async fn read_login_command(lines: &mut LoginStdin) -> Option<String> {
    match lines.next_line().await {
        Ok(Some(line)) => Some(line.trim().to_string()),
        _ => None,
    }
}

/// grammers 0.12 移除了 `is_premium()`；直接从原始 TL 对象读取 premium 标记。
/// grammers 0.12 removed `is_premium()`; read the flag from the raw TL object.
fn user_is_premium(user: &grammers_client::peer::User) -> bool {
    match &user.raw {
        tl::enums::User::User(u) => u.premium,
        tl::enums::User::Empty(_) => false,
    }
}

/// 把用户信息格式化为 `名称:是否高级会员`。
/// Format a signed-in user as `name:premium`.
fn login_user_summary(user: &grammers_client::peer::User) -> String {
    let name = match user.username() {
        Some(u) => format!("@{}", u),
        None => {
            let full = user.full_name();
            if full.trim().is_empty() {
                user.id()
                    .bare_id()
                    .map(|id| id.to_string())
                    .unwrap_or_else(|| "unknown".to_string())
            } else {
                full
            }
        }
    };
    format!("{}:{}", name, if user_is_premium(user) { "premium" } else { "standard" })
}

/// 交互式登录：通过 stdin/stdout 行协议完成「手机号 → 验证码 →（两步验证密码）」，
/// 成功后把授权写入用户会话文件 `{api_id}_user.db`，之后 `account=user` 即可直接使用。
///
/// Interactive login: drives "phone → code → (2FA password)" over a line protocol on
/// stdin/stdout and stores the authorization in the user session file `{api_id}_user.db`,
/// which `account=user` then reuses.
///
/// 协议见文件头注释 / See the module documentation for the protocol.
async fn login_flow() -> Result<(), String> {
    let api_id: i32 = {
        let s = param("api_id", "");
        if s.is_empty() { return Err("api_id is required".to_string()); }
        s.parse().map_err(|_| "api_id must be a number".to_string())?
    };
    let api_hash = param("api_hash", "");
    if api_hash.is_empty() { return Err("api_hash is required".to_string()); }
    let proxy = param("proxy", "");
    let mode = SessionMode::User;
    let label = param("session_label", "");
    let path = user_session_path(api_id, &label);

    let is_force = {
        let val = param("force_login", "");
        val == "1" || val.eq_ignore_ascii_case("true")
            || env::var("PP_FORCE_LOGIN").map(|v| v == "1" || v.eq_ignore_ascii_case("true")).unwrap_or(false)
            || env::args().any(|a| a == "--fresh" || a == "--force")
    };

    if is_force && path.exists() {
        eprintln!("fresh login requested: removing existing session {}", path.display());
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(format!("{}-wal", path.display()));
        let _ = std::fs::remove_file(format!("{}-shm", path.display()));
    }

    // 已存在的用户会话直接复用，无需再次登录
    // Reuse an existing user session instead of signing in again
    let client = open_client(api_id, &proxy, mode, Some(&path), false).await?;
    login_state(&format!("session:{}", path.display()));

    if !is_force && client
        .is_authorized()
        .await
        .map_err(|e| format!("is_authorized failed: {}", e))?
    {
        let me = client
            .get_me()
            .await
            .map_err(|e| format!("get_me failed: {}", e))?;
        login_state(&format!("already:{}", login_user_summary(&me)));
        return Ok(());
    }

    login_state("ready");

    // 整个登录过程复用同一个 stdin 读取器 / One stdin reader is reused for the whole login
    let mut stdin = login_stdin();

    // 第一步：等待手机号 / Step 1: wait for the phone number
    let phone = loop {
        let Some(cmd) = read_login_command(&mut stdin).await else {
            return Err("stdin closed before a phone number was provided".to_string());
        };
        if let Some(v) = cmd.strip_prefix("phone:") {
            let v = v.trim().to_string();
            if v.is_empty() {
                login_state("error:phone number is empty");
                continue;
            }
            break v;
        }
        if cmd == "quit" {
            login_state("cancelled");
            return Ok(());
        }
    };

    let token = match client.request_login_code(&phone, &api_hash).await {
        Ok(t) => t,
        Err(e) => {
            login_state(&format!("error:{}", e));
            return Err(format!("request_login_code failed: {}", e));
        }
    };
    login_state("code_sent");

    // 第二步：等待验证码（输入错误可以重新输入）/ Step 2: wait for the code (retryable)
    let mut password_token: Option<grammers_client::client::PasswordToken> = None;
    loop {
        let Some(cmd) = read_login_command(&mut stdin).await else {
            return Err("stdin closed before the login code was provided".to_string());
        };
        if cmd == "quit" {
            login_state("cancelled");
            return Ok(());
        }

        // 两步验证密码 / Two-factor authentication password
        if let Some(pw) = cmd.strip_prefix("password:") {
            let Some(token) = password_token.take() else {
                login_state("error:no password was requested");
                continue;
            };
            match client.check_password(token, pw.trim()).await {
                Ok(user) => {
                    login_state(&format!("done:{}", login_user_summary(&user)));
                    return Ok(());
                }
                Err(grammers_client::SignInError::InvalidPassword(new_token)) => {
                    password_token = Some(new_token);
                    login_state("invalid_password");
                }
                Err(e) => {
                    login_state(&format!("error:{}", e));
                    return Err(format!("check_password failed: {}", e));
                }
            }
            continue;
        }

        let Some(code) = cmd.strip_prefix("code:") else {
            continue;
        };

        match client.sign_in(&token, code.trim()).await {
            Ok(user) => {
                login_state(&format!("done:{}", login_user_summary(&user)));
                return Ok(());
            }
            Err(grammers_client::SignInError::PasswordRequired(tok)) => {
                let hint = tok.hint().unwrap_or("").to_string();
                password_token = Some(tok);
                login_state(&format!("password_required:{}", hint));
            }
            Err(grammers_client::SignInError::InvalidCode) => login_state("invalid_code"),
            Err(grammers_client::SignInError::SignUpRequired) => {
                login_state(
                    "error:this phone number has no Telegram account — sign up in an official app first",
                );
                return Err("sign up with an official client is required".to_string());
            }
            Err(e) => {
                login_state(&format!("error:{}", e));
                return Err(format!("sign_in failed: {}", e));
            }
        }
    }
}

/// 登录模式入口（`--login`）：交互式生成用户会话文件，不需要输入视频。
/// Login entry point (`--login`): interactively create the user session file (no input video).
fn run_login() -> Result<(), String> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("tokio runtime error: {}", e))?
        .block_on(login_flow())
}

/// 模块主逻辑：读取参数、准备文件、带重试的上传发送。
/// Main module logic: read parameters, prepare files, upload and send with retries.
fn run() -> Result<(), String> {
    let input_str = env::var("PP_INPUT").map_err(|_| "PP_INPUT not set".to_string())?;
    let input = PathBuf::from(&input_str);
    if !input.exists() { return Err(format!("Input file not found: {}", input.display())); }

    // 读取并验证必填参数 / Read and validate required parameters
    let TgConfig {
        api_id,
        api_hash,
        bot_token,
        chat_id,
        proxy,
        username,
        send_video,
        send_caption,
        send_cover,
        cover_first,
        topic_per_model,
        delete_after_send,
        mode,
    } = read_config()?;


    // 从文件名解析元数据 / Parse metadata from filename
    let stem = input.file_stem().and_then(|s| s.to_str()).unwrap_or("recording");
    let (model_name, timestamp) = parse_stem(stem);
    let file_size = fs::metadata(&input).map(|m| m.len()).unwrap_or(0);
    let duration  = pp_utils::video_duration(&input).unwrap_or(0.0);
    let ts_str    = if timestamp.is_empty() { "—".to_string() } else { timestamp };
    let dur_str   = format_duration(duration);
    let name_str  = input.file_name().and_then(|n| n.to_str()).unwrap_or("").to_string();
    let size_str  = format_bytes(file_size);

    // 始终从视频中截取一帧作为封面图，不再查找 contact sheet。
    // Always extract one frame from the video as the cover image; never use a contact sheet.
    let (cover_path_buf, is_auto_cover) = if send_cover {
        match extract_video_frame(&input, "tg_cover") {
            Ok(p) => {
                eprintln!("extracted video frame as cover: {}", p.display());
                (Some(p), true)
            }
            Err(e) => {
                eprintln!("could not extract cover frame from video: {}", e);
                (None, false)
            }
        }
    } else {
        (None, false)
    };
    let cover = cover_path_buf.as_deref();

    // 构造本次发送可用的会话列表：主会话 + 已登录的备用会话。
    // Build the session list for this send: the primary session plus signed-in fallbacks.
    let label = param("session_label", "");
    let mut sessions = vec![primary_session(api_id, mode, &label)];
    if sessions[0].mode != SessionMode::Bot {
        for fallback in parse_fallback_labels(&param("fallback_sessions", "")) {
            if fallback == sessions[0].label {
                continue;
            }
            let path = user_session_path(api_id, &fallback);
            if !path.is_file() {
                eprintln!(
                    "FAILOVER: skipping backup session {} — no session file at {} (sign in with session_label={} first)",
                    fallback, path.display(), fallback
                );
                continue;
            }
            sessions.push(SessionIdentity {
                api_id,
                mode: SessionMode::User,
                label: fallback,
                path,
            });
        }
    }
    eprintln!(
        "TELEGRAM_SESSIONS: {}",
        sessions.iter().map(|s| s.display_name()).collect::<Vec<_>>().join(" -> ")
    );

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("tokio runtime error: {}", e))?;

    let workspace = send_video.then(UploadWorkspace::new).transpose()?;
    let effective_parts = match &workspace {
        Some(workspace) => prepare_video_parts(&input, &workspace.0)?,
        None => Vec::new(),
    };

    let mut final_error: Option<String> = None;
    for (idx, session) in sessions.iter().enumerate() {
        eprintln!(
            "TELEGRAM_SESSION[{}/{}]: {} ({})",
            idx + 1,
            sessions.len(),
            session.display_name(),
            session.path.display()
        );

        // 每个会话独立连接、独立上传；Telegram 的 InputFile ID 与账号/DC 绑定，
        // 切换账号后必须把整个文件重新上传，不能复用已上传的分块。
        // Every session connects and uploads independently: Telegram InputFile ids are bound to
        // the account/DC, so a switch must re-upload the whole file from scratch.
        let attempt_result: Result<(), String> = runtime.block_on(async {
            const MAX_OUTER: u32 = 3;
            const RECONNECT_DELAY: Duration = Duration::from_secs(30);

            let client = match connect_client(
                api_id,
                &api_hash,
                &bot_token,
                &proxy,
                session.mode,
                Some(&session.path),
            )
            .await
            {
                Ok(c) => c,
                Err(e) => return Err(e),
            };
            let mut attempt = 0u32;
            loop {
                let result = upload_and_send(
                    &client, session, chat_id, &username,
                    &model_name, &ts_str, &dur_str, &name_str, &size_str,
                    &input, cover.as_deref(), send_video, topic_per_model,
                    &effective_parts, workspace.as_ref().map(|dir| dir.0.as_path()),
                    send_caption, send_cover, cover_first,
                ).await;
                match result {
                    Ok(()) => return Ok(()),
                    Err(e) => {
                        if is_permanent_telegram_error(&e) {
                            return Err(e);
                        }
                        // 上传阶段限流：立即结束当前会话，交给外层切换账号。
                        // Upload-phase flood: end this session immediately so the caller switches.
                        if e.starts_with(FAILOVER_TAG) {
                            return Err(e);
                        }
                        attempt += 1;
                        if attempt >= MAX_OUTER {
                            return Err(e);
                        }
                        eprintln!(
                            "upload failed on {} (attempt {}/{}): {}. retrying in {:?}…",
                            session.display_name(), attempt, MAX_OUTER, e, RECONNECT_DELAY
                        );
                        tokio::time::sleep(RECONNECT_DELAY).await;
                    }
                }
            }
        });

        cleanup_session_copy(&session.path);

        match attempt_result {
            Ok(()) => break,
            Err(e) => {
                let failover = e.starts_with(FAILOVER_TAG);
                let visible = untag_failover(&e).to_string();
                if failover && idx + 1 < sessions.len() {
                    eprintln!(
                        "FAILOVER: {} hit upload limit ({}). switching to {} and re-uploading…",
                        session.display_name(),
                        visible,
                        sessions[idx + 1].display_name()
                    );
                    continue;
                }
                final_error = Some(visible);
                break;
            }
        }
    }

    if let Some(e) = final_error {
        return Err(e);
    }

    // 清理自动截取的临时封面图 / Clean up temporary auto-extracted cover image
    if is_auto_cover {
        if let Some(ref p) = cover_path_buf {
            let _ = fs::remove_file(p);
        }
    }

    println!("OUTPUT:{}", input.display());

    // 上传成功后请求主程序删除本地数据（成片、分片目录、元数据、模块副产物）。
    // 必须放在 OUTPUT 之后：主程序只在节点成功时才执行删除请求。
    //
    // After a successful upload, ask the host to delete the local data (video, segment
    // directory, metadata, module side outputs). This must come after OUTPUT, because the
    // host only honours the request when the node succeeded.
    if delete_after_send {
        println!("STATUS:Upload complete. Deleting local data");
        println!("DELETE_INPUT");
    }

    Ok(())
}

/// 程序入口：处理 `--describe` / `--login` / `--refresh` 参数或执行主逻辑。
/// Entry point: handle `--describe` / `--login` / `--refresh` argument or run the main logic.
fn main() {
    let args: Vec<String> = env::args().collect();
    let arg1 = args.get(1).map(|s| s.as_str());
    if arg1 == Some("--describe") {
        print!("{}", DESCRIBE);
        return;
    }
    // 确保临时目录存在 / Ensure temp directory exists
    tmp_dir();

    // 登录模式（--login）：由主程序的「登录」按钮驱动，交互式生成用户会话文件
    // Login mode (--login): driven by the host's "Login" button, creates the user session file
    if arg1 == Some("--login") {
        if let Err(e) = run_login() {
            eprintln!("{}", e);
            std::process::exit(1);
        }
        return;
    }

    // 刷新模式（--refresh）：验证并刷新已授权用户会话（无需手机号或验证码）
    // Refresh mode (--refresh): verify and refresh an authorized user session
    if arg1 == Some("--refresh") {
        let mut api_id = param("api_id", "");
        if api_id.is_empty() {
            if let Some(arg2) = args.get(2) {
                api_id = arg2.to_string();
            }
        }
        if api_id.is_empty() {
            eprintln!("api_id is required for --refresh");
            std::process::exit(1);
        }
        let proxy = param("proxy", "");
        let label = param("session_label", "");
        let id: i32 = match api_id.parse() {
            Ok(n) => n,
            Err(_) => {
                eprintln!("api_id must be a number");
                std::process::exit(1);
            }
        };
        if let Err(e) = run_refresh(id, &proxy, &label) {
            eprintln!("{}", e);
            std::process::exit(1);
        }
        return;
    }

    // 自检模式（PP_TEST=1）：由主程序「测试」按钮触发，不需要输入视频
    // Self-test mode (PP_TEST=1): triggered by the host's "Test" button, no input video needed
    let is_test = matches!(
        env::var("PP_TEST").unwrap_or_default().to_lowercase().as_str(),
        "1" | "true" | "yes"
    );

    let result = if is_test { run_test() } else { run() };
    if let Err(e) = result {
        eprintln!("{}", e);
        std::process::exit(1);
    }
}

// ─── 单元测试 / Unit tests ────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chat_id_and_username_are_normalised() {
        // 从 Telegram 客户端复制出来的值常带空格和 @
        // Values copied out of Telegram clients often carry spaces and a leading @
        assert_eq!(normalize_chat_id(" -1002795956983 ").unwrap(), -1_002_795_956_983);
        assert_eq!(normalize_chat_id("-1002795956983").unwrap(), -1_002_795_956_983);
        assert!(normalize_chat_id("   ").is_err());
        assert!(normalize_chat_id("mygroup").is_err());
        assert_eq!(normalize_username(" @mygroup "), "mygroup");
        assert_eq!(normalize_username("mygroup"), "mygroup");
        assert_eq!(normalize_username(""), "");
    }

    #[test]
    fn session_mode_parsing() {
        assert_eq!(SessionMode::from_param("user"), SessionMode::User);
        assert_eq!(SessionMode::from_param(" USER "), SessionMode::User);
        assert_eq!(SessionMode::from_param("bot"), SessionMode::Bot);
        // 未填写或写错时按 auto 处理：已登录的真实账号优先
        // Empty or unknown values mean "auto": the signed-in real account wins
        assert_eq!(SessionMode::from_param(""), SessionMode::Auto);
        assert_eq!(SessionMode::from_param("auto"), SessionMode::Auto);
        assert_eq!(SessionMode::from_param("anything"), SessionMode::Auto);
    }

    #[test]
    fn auto_prefers_the_signed_in_user_session() {
        // 已经登录过的 api_id：auto 必须选真实账号，而不是机器人
        // For an api_id that has been logged in, auto must pick the real account, not the bot
        let signed_in = 1_357_592;
        if session_path(signed_in, SessionMode::User).exists() {
            assert_eq!(SessionMode::Auto.resolve(signed_in), SessionMode::User);
        }
        // 显式选择不会被 auto 覆盖 / An explicit choice is never overridden
        assert_eq!(SessionMode::User.resolve(0), SessionMode::User);
        assert_eq!(SessionMode::Bot.resolve(signed_in), SessionMode::Bot);
        // 没有用户会话时回退到机器人 / No user session falls back to the bot
        assert_eq!(SessionMode::Auto.resolve(0), SessionMode::Bot);
    }

    #[test]
    fn only_user_sessions_may_list_topics() {
        assert!(SessionMode::User.can_list_topics());
        assert!(SessionMode::Auto.can_list_topics());
        // 机器人调用 messages.getForumTopics 会返回 BOT_METHOD_INVALID
        // Bots get BOT_METHOD_INVALID from messages.getForumTopics
        assert!(!SessionMode::Bot.can_list_topics());
    }

    #[test]
    fn upload_file_ids_are_positive_and_unique() {
        // 旧实现在 debug 构建下会因 i64 乘法溢出而 panic
        // The old implementation panicked on i64 multiplication overflow in debug builds
        let a = upload_file_id(Path::new("/tmp/telegram_selftest.mp4"));
        let b = upload_file_id(Path::new("/tmp/telegram_selftest.mp4"));
        let c = upload_file_id(Path::new("/tmp/another_recording.mp4"));
        assert!(a > 0, "file_id must be positive, got {}", a);
        assert!(b > 0 && c > 0);
        assert_ne!(a, b, "two uploads must not share a file_id");
        assert_ne!(a, c, "different paths must not share a file_id");
    }

    #[test]
    fn upload_engine_defaults_to_wzgram() {
        assert_eq!(parse_upload_engine("wzgram"), UploadEngine::Wzgram);
        assert_eq!(parse_upload_engine(""), UploadEngine::Wzgram);
        assert_eq!(parse_upload_engine("unknown"), UploadEngine::Wzgram);
        assert_eq!(parse_upload_engine("GRAMMERS"), UploadEngine::Grammers);
        assert_eq!(parse_upload_engine(" rust "), UploadEngine::Grammers);
        assert!(!should_use_wzgram(UploadEngine::Wzgram, 1024));
        assert!(should_use_wzgram(
            UploadEngine::Wzgram,
            BIG_FILE_THRESHOLD as u64
        ));
        assert!(!should_use_wzgram(
            UploadEngine::Grammers,
            BIG_FILE_THRESHOLD as u64
        ));
    }

    #[test]
    fn session_files_are_separate_per_mode() {
        let bot = session_path(4242, SessionMode::Bot);
        let user = session_path(4242, SessionMode::User);
        assert_ne!(bot, user);
        assert!(bot.to_string_lossy().ends_with("4242.db"));
        assert!(user.to_string_lossy().ends_with("4242_user.db"));
    }

    #[test]
    fn topic_title_is_trimmed_and_truncated() {
        assert_eq!(topic_title("  alice  "), "alice");
        assert_eq!(topic_title("   "), "unknown");
        let long = "x".repeat(MAX_TOPIC_TITLE_LEN + 50);
        assert_eq!(topic_title(&long).chars().count(), MAX_TOPIC_TITLE_LEN);
    }

    #[test]
    fn topic_model_key_is_case_insensitive() {
        assert_eq!(topic_model_key(" Alice "), "alice");
        assert_eq!(topic_model_key("ALICE"), topic_model_key("alice"));
    }

    #[test]
    fn basic_group_ids_are_chats_not_channels() {
        // -123456789 是普通群，话题只存在于超级群组里
        // -123456789 is a basic group; topics only exist in supergroups
        let group = build_peer_ref(-123_456_789);
        assert_eq!(group.id.kind(), grammers_session::types::PeerKind::Chat);
        let target = Target { peer: group, kind: ChatKind::BasicGroup, title: None };
        let problem = target.topic_problem(-123_456_789).expect("basic groups cannot host topics");
        assert!(problem.contains("basic group"));
        assert!(problem.contains("Topics"));
    }

    #[test]
    fn topics_depend_on_the_forum_flag() {
        let peer = build_peer_ref(-1_002_795_956_983);
        let on = Target {
            peer,
            kind: ChatKind::Supergroup { forum: true },
            title: Some("Group".to_string()),
        };
        assert_eq!(on.topic_problem(-1_002_795_956_983), None);

        let off = Target { kind: ChatKind::Supergroup { forum: false }, ..on.clone() };
        let problem = off.topic_problem(-1_002_795_956_983).expect("topics are off");
        assert!(problem.contains("topics are disabled"));

        let broadcast = Target { kind: ChatKind::Broadcast, ..on.clone() };
        assert!(broadcast.topic_problem(-1_002_795_956_983).unwrap().contains("broadcast channel"));
    }

    #[test]
    fn target_description_shows_id_title_and_kind() {
        let target = Target {
            peer: build_peer_ref(-1_002_795_956_983),
            kind: ChatKind::Supergroup { forum: true },
            title: Some("My Group".to_string()),
        };
        let text = target.describe();
        assert!(text.contains("-1002795956983"), "{}", text);
        assert!(text.contains("My Group"), "{}", text);
        assert!(text.contains("topics on"), "{}", text);
    }

    #[test]
    fn channel_invalid_errors_get_an_actionable_hint() {
        // 用户遇到的就是这个错误：CHANNEL_INVALID 本身看不出问题在哪
        // This is the error the user hit: CHANNEL_INVALID alone says nothing useful
        let msg = topic_error_hint(
            "request error: rpc error 400: CHANNEL_INVALID caused by messages.createForumTopic",
            -1_002_795_956_983,
        );
        assert!(msg.starts_with("createForumTopic failed:"));
        assert!(msg.contains("-1002795956983"), "{}", msg);
        assert!(msg.contains("supergroup"), "{}", msg);

        let admin = topic_error_hint("rpc error 400: CHAT_ADMIN_REQUIRED", -1);
        assert!(admin.contains("Manage Topics"), "{}", admin);
    }

    #[test]
    fn ten_digit_basic_group_ids_are_groups_not_channels() {
        // 普通群 ID 已经到 10 位（-1234567890）。旧实现把它当成频道 234567890，
        // 于是 createForumTopic 返回 CHANNEL_INVALID。
        // Basic-group ids already reach 10 digits (-1234567890). The old mapping treated it as
        // channel 234567890, which made createForumTopic fail with CHANNEL_INVALID.
        let group = build_peer_ref(-1_234_567_890);
        assert_eq!(group.id.kind(), grammers_session::types::PeerKind::Chat);
        assert_eq!(group.id, grammers_session::types::PeerId::chat_unchecked(1_234_567_890));
    }

    #[test]
    fn a_channel_interpretation_is_offered_as_fallback() {
        // -<channel_id> 与 <channel_id> 这两种少写 100 前缀的写法要有备选解释
        // Both "missing 100 prefix" spellings need a channel fallback
        let from_negative = peer_candidates(-2_795_956_983);
        assert_eq!(from_negative.len(), 2);
        assert_eq!(from_negative[0].kind(), grammers_session::types::PeerKind::Chat);
        assert_eq!(from_negative[1].kind(), grammers_session::types::PeerKind::Channel);

        let from_positive = peer_candidates(2_795_956_983);
        assert_eq!(from_positive.len(), 2);
        assert_eq!(from_positive[0].kind(), grammers_session::types::PeerKind::User);
        assert_eq!(from_positive[1].kind(), grammers_session::types::PeerKind::Channel);

        // 已经是 -100… / 100… 时没有歧义
        // No ambiguity once the id already carries -100… / 100…
        assert_eq!(peer_candidates(-1_002_795_956_983).len(), 1);
        assert_eq!(peer_candidates(1_002_795_956_983).len(), 1);
    }

    #[test]
    fn bare_supergroup_ids_resolve_to_channels() {
        // -1002795956983 与省略负号的 1002795956983 必须解析为同一个频道
        // -1002795956983 and 1002795956983 must resolve to the same channel
        let with_sign = build_peer_ref(-1_002_795_956_983);
        let without_sign = build_peer_ref(1_002_795_956_983);
        assert_eq!(with_sign.id, without_sign.id);
        assert_eq!(with_sign.id.kind(), grammers_session::types::PeerKind::Channel);
        // 普通用户 ID 仍然是用户
        // A regular user id stays a user
        assert_eq!(
            build_peer_ref(123_456_789).id.kind(),
            grammers_session::types::PeerKind::User
        );
    }

    #[test]
    fn upload_parts_use_a_two_gigabyte_cap() {
        assert_eq!(VIDEO_PART_LIMIT, 2_000_000_000);
    }

    /// `--describe` 必须声明「发送后删除本地数据」参数，且默认开启。
    /// `--describe` must declare the delete-after-send parameter, enabled by default.
    #[test]
    fn describe_declares_delete_after_send() {
        let v: serde_json::Value = serde_json::from_str(DESCRIBE).expect("DESCRIBE is valid JSON");
        let params = v["params"].as_array().expect("params is an array");
        let p = params
            .iter()
            .find(|p| p["key"] == "delete_after_send")
            .expect("delete_after_send param is declared");
        assert_eq!(p["type"], "boolean");
        assert_eq!(p["default"], true);
    }

    /// 并发上传参数必须存在且默认 16：与 Telegram-Fast-Uploader 一致，16 线程满速上传。
    /// The concurrent-upload parameter must exist and default to 16 — matching Telegram-Fast-Uploader.
    #[test]
    fn describe_declares_parallel_upload_workers() {
        let v: serde_json::Value = serde_json::from_str(DESCRIBE).expect("DESCRIBE is valid JSON");
        let params = v["params"].as_array().expect("params is an array");

        let p = params
            .iter()
            .find(|p| p["key"] == "upload_workers")
            .expect("upload_workers param is declared");
        assert_eq!(p["type"], "number");
        assert_eq!(p["default"], DEFAULT_UPLOAD_WORKERS);
        assert_eq!(DEFAULT_UPLOAD_WORKERS, 16);
    }

    /// 批量并发参数必须存在且默认 3。
    /// The batch_size parameter must exist and default to 3.
    #[test]
    fn describe_declares_batch_size() {
        let v: serde_json::Value = serde_json::from_str(DESCRIBE).expect("DESCRIBE is valid JSON");
        let params = v["params"].as_array().expect("params is an array");

        let p = params
            .iter()
            .find(|p| p["key"] == "batch_size")
            .expect("batch_size param is declared");
        assert_eq!(p["type"], "number");
        assert_eq!(p["default"], 3);
    }

    /// 并发分块数归一化：0 取默认，超过上限截断，永远不会是 0 个 worker。
    /// Worker normalisation: 0 means the default, above the cap is clamped, never zero workers.
    #[test]
    fn upload_workers_are_normalised() {
        assert_eq!(normalize_upload_workers(0), DEFAULT_UPLOAD_WORKERS);
        assert_eq!(normalize_upload_workers(1), 1);
        assert_eq!(normalize_upload_workers(8), 8);
        assert_eq!(normalize_upload_workers(16), 16);
        assert_eq!(normalize_upload_workers(999), MAX_UPLOAD_WORKERS);
    }

    /// 进度行必须节流：同一百分比不重复上报，间隔内不重复上报，完成时立即上报。
    /// Progress must be throttled: no duplicate percentages, no bursts, immediate final line.
    #[test]
    fn progress_reporting_is_throttled() {
        assert!(should_report_progress(120, 0, Duration::from_secs(1), false));
        assert!(!should_report_progress(120, 120, Duration::from_secs(1), false));
        assert!(!should_report_progress(130, 120, Duration::from_millis(10), false));
        assert!(should_report_progress(130, 120, PROGRESS_INTERVAL, false));
        // 100MB 文件会在 250ms 内完成很多分块；最终 100% 不应被节流吞掉。
        // Many parts can finish within 250ms; the final 100% line must still be emitted.
        assert!(should_report_progress(PROGRESS_SCALE, 9900, Duration::ZERO, true));
    }

    /// Telegram 的 flood-wait 错误形式很多，必须全部解析成秒数。
    /// All common flood-wait error shapes must be parsed into seconds.
    #[test]
    fn rpc_wait_errors_are_parsed() {
        assert_eq!(wait_secs_from_rpc_name("FLOOD_WAIT_42", None), Some(42));
        assert_eq!(wait_secs_from_rpc_name("FLOOD_WAIT_X", Some(15u32)), Some(15));
        assert_eq!(wait_secs_from_rpc_name("FLOOD_PREMIUM_WAIT_90", None), Some(90));
        assert_eq!(wait_secs_from_rpc_name("FLOOD_PREMIUM_WAIT_X", Some(7u32)), Some(7));
        assert_eq!(wait_secs_from_rpc_name("FLOOD_WAIT_0", None), Some(1));
        assert_eq!(
            wait_secs_from_rpc_name("FLOOD_WAIT_999999", None),
            Some(MAX_RPC_WAIT_SECS)
        );
        assert_eq!(wait_secs_from_rpc_name("CHANNEL_INVALID", Some(3u32)), None);
    }

    /// 通用退避序列：第一次失败用第一个值，超出后重复最后一个值。
    /// Generic backoff: first failure uses the first value, then repeats the last one.
    #[test]
    fn retry_backoff_uses_sequence() {
        let delays = [1u64, 2, 4, 8, 15, 30];
        assert_eq!(backoff_secs(0, &delays), 1);
        assert_eq!(backoff_secs(3, &delays), 8);
        assert_eq!(backoff_secs(99, &delays), 30);
    }

    /// 默认只上传视频、不带任何文字：`send_caption` 必须存在且默认关闭。
    /// Video-only by default: `send_caption` must exist and default to off.
    #[test]
    fn describe_declares_caption_off_by_default() {
        let v: serde_json::Value = serde_json::from_str(DESCRIBE).expect("DESCRIBE is valid JSON");
        let params = v["params"].as_array().expect("params is an array");
        let caption = params
            .iter()
            .find(|p| p["key"] == "send_caption")
            .expect("send_caption param is declared");
        assert_eq!(caption["type"], "boolean");
        assert_eq!(caption["default"], false);
        let cover = params
            .iter()
            .find(|p| p["key"] == "send_cover")
            .expect("send_cover param is declared");
        assert_eq!(cover["type"], "boolean");
        assert_eq!(cover["default"], true);
    }

    /// 封面图默认先单独发一条，再发视频：`cover_first` 必须存在且默认开启。
    /// The cover is posted on its own before the video by default: `cover_first` must exist
    /// and default to on.
    #[test]
    fn describe_declares_cover_first() {
        let v: serde_json::Value = serde_json::from_str(DESCRIBE).expect("DESCRIBE is valid JSON");
        let params = v["params"].as_array().expect("params is an array");
        let p = params
            .iter()
            .find(|p| p["key"] == "cover_first")
            .expect("cover_first param is declared");
        assert_eq!(p["type"], "boolean");
        assert_eq!(p["default"], true);
    }

    /// 封面图单独发送的判定：只有在同时发视频时才需要拆成两条消息。
    /// Whether the cover gets its own message: only worth splitting when the video is sent too.
    #[test]
    fn the_cover_is_split_off_only_alongside_a_video() {
        // 视频 + 封面图 + 先发封面图 = 两条消息（图片在前，视频在后）
        // video + cover + cover-first = two messages (image first, then the video)
        assert!(cover_sent_separately(true, true, true));
        // 关闭 cover_first 回到「封面图放进视频相册」
        // Turning cover_first off puts the cover back inside the video album
        assert!(!cover_sent_separately(true, true, false));
        // 没有封面图时没什么可拆的 / Nothing to split when there is no cover
        assert!(!cover_sent_separately(true, false, true));
        // 只发图片时本来就只有一条图片消息，不需要再拆
        // Without the video the cover already is the only message
        assert!(!cover_sent_separately(false, true, true));
        assert!(!cover_sent_separately(false, false, true));
    }

    /// 关掉文字说明后仍然要能发出去：视频 / 封面图 / 文字至少留一样。
    /// With the caption off there must still be a payload: at least one of video / cover / text.
    #[test]
    fn payload_flags_need_one_enabled_item() {
        // 只发视频（用户要的行为）/ Video only (what the user asked for)
        assert!(validate_payload_flags(true, false, false).is_ok());
        assert!(validate_payload_flags(true, false, true).is_ok());
        assert!(validate_payload_flags(true, true, true).is_ok());
        // 全关必须报错，而不是连上 Telegram 后才发现没东西可发
        // All off must fail instead of connecting first and discovering nothing to send
        let err = validate_payload_flags(false, false, false).unwrap_err();
        assert!(err.contains("nothing to send"), "unexpected message: {}", err);
        // 纯文字 / text only
        assert!(validate_payload_flags(false, true, false).is_ok());
    }

    #[test]
    fn session_labels_are_sanitised_and_deduplicated() {
        assert_eq!(sanitize_session_label(" Backup-1 "), "backup-1");
        assert_eq!(sanitize_session_label("../etc/passwd"), "etc_passwd");
        assert_eq!(sanitize_session_label("***"), "");
        assert_eq!(
            user_session_path(42, ""),
            config_dir_path().join("42_user.db")
        );
        assert_eq!(
            user_session_path(42, "backup1"),
            config_dir_path().join("42_user_backup1.db")
        );
        assert_eq!(
            parse_fallback_labels(" backup1, Backup-1 ; backup2\nbackup1,, "),
            vec!["backup1".to_string(), "backup-1".to_string(), "backup2".to_string()]
        );
    }

    #[test]
    fn flood_errors_are_tagged_for_failover() {
        assert!(is_flood_error("upload failed: FLOOD_PREMIUM_WAIT_X"));
        assert!(is_flood_error("FLOOD_WAIT_42"));
        assert!(!is_flood_error("CHANNEL_INVALID"));

        let tagged = tag_flood_error("save part failed: FLOOD_WAIT_30".to_string());
        assert!(tagged.starts_with(FAILOVER_TAG));
        assert_eq!(untag_failover(&tagged), "save part failed: FLOOD_WAIT_30");
        assert_eq!(
            tag_flood_error("CHANNEL_INVALID".to_string()),
            "CHANNEL_INVALID"
        );
        // 已经打过标记的错误不能重复加前缀 / Never stack the internal marker
        assert_eq!(tag_flood_error(tagged.clone()), tagged);
    }

    #[test]
    fn describe_declares_multi_session_params() {
        let v: serde_json::Value = serde_json::from_str(DESCRIBE).expect("DESCRIBE is valid JSON");
        let params = v["params"].as_array().expect("params is an array");
        let label = params
            .iter()
            .find(|p| p["key"] == "session_label")
            .expect("session_label is declared");
        assert_eq!(label["type"], "string");
        assert_eq!(label["default"], "");
        let fallback = params
            .iter()
            .find(|p| p["key"] == "fallback_sessions")
            .expect("fallback_sessions is declared");
        assert_eq!(fallback["type"], "string");
        assert_eq!(fallback["default"], "");
    }
}
