//! 录制管理器 / Recording Manager
//!
//! 管理所有主播的录制会话生命周期，包括：
//! - 启动/停止录制（HLS 分片下载 + fMP4 转 TS + ffmpeg 合并）
//! - 录制完成后自动触发后处理流水线
//! - 启动时合并遗留的未完成录制片段
//!
//! Manages the lifecycle of all streamer recording sessions, including:
//! - Starting/stopping recordings (HLS segment download + fMP4 to TS + ffmpeg merge)
//! - Automatically triggering the post-processing pipeline after recording completes
//! - Merging leftover incomplete recording segments on startup

use crate::config::settings::AppState;
use crate::core::emitter::{Emitter, EmitterExt};
use crate::core::error::{AppError, Result};
use crate::recording::hls::{get_url_prefix, parse_playlist};
use crate::streaming::stripchat::StripchatApi;
use chrono::Local;
use parking_lot::RwLock;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{Arc, LazyLock};
use tokio::sync::{Semaphore, mpsc};

/// ffmpeg 分片转封装并发限制信号量（快速进程，每次耗时约数十毫秒）。
/// Concurrency limit semaphore for fast fMP4->TS segment conversion (tens of milliseconds each).
static FFMPEG_CONVERT_SEMAPHORE: LazyLock<Semaphore> = LazyLock::new(|| Semaphore::new(8));

/// ffmpeg 完整视频合并并发限制信号量（I/O密集型长任务，避免阻塞活跃录制转封装）。
/// Dedicated semaphore for long-running video merges so they never starve active segment conversions.
static FFMPEG_MERGE_SEMAPHORE: LazyLock<Semaphore> = LazyLock::new(|| Semaphore::new(2));

/// 单个录制会话的状态 / State of a single recording session
#[derive(Debug, Clone)]
pub struct RecordingSession {
    /// 主播用户名 / Streamer username
    #[allow(dead_code)]
    pub username: String,
    /// 录制会话目录路径（存放 .ts 分片）/ Recording session directory path (stores .ts segments)
    pub dir_path: PathBuf,
    /// 录制开始时间 / Recording start time
    pub started_at: chrono::DateTime<chrono::Utc>,
    /// 停止录制的信号发送端 / Sender to signal recording stop
    stop_tx: mpsc::Sender<()>,
}

/// 录制管理器，管理所有主播的录制会话。
/// Recording manager that manages all streamer recording sessions.
pub struct RecorderManager {
    /// 应用状态 / Application state
    state: Arc<AppState>,
    /// 活跃录制会话表（用户名 -> 会话）/ Active recording sessions (username -> session)
    sessions: RwLock<HashMap<String, RecordingSession>>,
    /// 自然结束（非手动停止）的主播集合，用于触发自动重录 / Streamers that stopped naturally (not manually), for auto-restart
    pub naturally_stopped: RwLock<HashSet<String>>,
    /// 正在手动停止录制的主播集合 / Streamers currently being manually stopped
    manually_stopping: RwLock<HashSet<String>>,
    /// 各 CDN 节点的首选 TLD 缓存（跨会话共享）/ Preferred TLD cache per CDN node (shared across sessions)
    preferred_tld_by_node: Arc<parking_lot::Mutex<HashMap<String, String>>>,
    /// 正在合并的会话目录集合 / Set of session directories currently being merged
    pub merging_dirs: RwLock<HashSet<PathBuf>>,
    /// 等待合并的会话目录集合 / Set of session directories waiting to merge
    pub waiting_merge_dirs: RwLock<HashSet<PathBuf>>,
}

impl RecorderManager {
    /// 创建新的录制管理器实例。
    /// Create a new recorder manager instance.
    pub fn new(state: Arc<AppState>) -> Arc<Self> {
        Arc::new(Self {
            state,
            sessions: RwLock::new(HashMap::new()),
            naturally_stopped: RwLock::new(HashSet::new()),
            manually_stopping: RwLock::new(HashSet::new()),
            preferred_tld_by_node: Arc::new(parking_lot::Mutex::new(HashMap::new())),
            merging_dirs: RwLock::new(HashSet::new()),
            waiting_merge_dirs: RwLock::new(HashSet::new()),
        })
    }

    /// 判断指定主播是否正在录制。
    /// Check if a specific streamer is currently being recorded.
    pub fn is_recording(&self, username: &str) -> bool {
        self.sessions.read().contains_key(username)
    }

    /// 获取 CDN TLD 缓存的共享引用（供 StripchatApi 使用）。
    /// Get a shared reference to the CDN TLD cache (for use by StripchatApi).
    pub fn cdn_tld_cache(&self) -> Arc<parking_lot::Mutex<HashMap<String, String>>> {
        Arc::clone(&self.preferred_tld_by_node)
    }

    /// 获取当前应用设置。
    /// Get the current application settings.
    pub fn get_settings(&self) -> crate::config::settings::Settings {
        self.state.get_settings()
    }

    /// 判断指定路径是否被某个活跃录制会话锁定（路径在会话目录下）。
    /// Check if a path is locked by an active recording session (path is under a session directory).
    pub fn is_file_locked(&self, path: &std::path::Path) -> bool {
        self.sessions
            .read()
            .values()
            .any(|s| path.starts_with(&s.dir_path))
    }

    /// 获取所有活跃录制会话的目录路径和开始时间列表。
    /// Get a list of all active recording session directory paths and start times.
    pub fn get_active_sessions(&self) -> Vec<(PathBuf, chrono::DateTime<chrono::Utc>)> {
        self.sessions
            .read()
            .values()
            .map(|s| (s.dir_path.clone(), s.started_at))
            .collect()
    }

    /// 返回当前活跃录制会话数量。
    /// Return the number of currently active recording sessions.
    pub fn active_count(&self) -> usize {
        self.sessions.read().len()
    }

    /// 启动录制（通用版本，接受任意 emitter）。
    /// 创建会话目录，启动异步录制循环，录制完成后自动合并分片并触发后处理。
    ///
    /// Start recording (generic version, accepts any emitter).
    /// Creates the session directory, starts the async recording loop,
    /// and automatically merges segments and triggers post-processing after completion.
    ///
    /// # 返回值 / Returns
    /// 录制会话目录路径 / Recording session directory path
    pub async fn start_recording_with_emitter(
        self: &Arc<Self>,
        username: &str,
        playlist_url: &str,
        emitter: Arc<dyn Emitter>,
    ) -> Result<String> {
        if self.is_recording(username) {
            return Err(AppError::AlreadyRecording(username.to_string()));
        }

        let settings = self.state.get_settings();
        if settings.max_concurrent > 0 && self.active_count() >= settings.max_concurrent {
            return Err(AppError::Other(
                "Max concurrent recordings reached".to_string(),
            ));
        }

        let timestamp = Local::now().format("%Y%m%d_%H%M%S");
        let session_dir = PathBuf::from(&settings.output_dir)
            .join(username)
            .join(format!("{}_{}", username, timestamp));
        fs::create_dir_all(&session_dir)?;

        let (stop_tx, stop_rx) = mpsc::channel(1);

        let session = RecordingSession {
            username: username.to_string(),
            dir_path: session_dir.clone(),
            started_at: chrono::Utc::now(),
            stop_tx,
        };

        self.sessions.write().insert(username.to_string(), session);

        emitter.emit(
            "recording-started",
            &serde_json::json!({
                "username": username,
                "dir_path": session_dir.to_string_lossy()
            }),
        );

        // 录制开始时立即为合并目标视频预创建 meta 文件，status = "recording"
        // Pre-create meta file for the merge target video when recording starts, status = "recording"
        {
            let target_path = settings.video_path_for_session(&session_dir);
            // 最终视频可能位于独立目录，需先确保其父目录存在
            // Final videos may live in a separate directory; make sure its parent exists
            if let Some(parent) = target_path.parent() {
                fs::create_dir_all(parent)?;
            }
            let started_at = {
                let local: chrono::DateTime<chrono::Local> = chrono::Utc::now().into();
                local.to_rfc3339()
            };
            let meta = crate::recording::meta::VideoMeta {
                meta_version: crate::recording::meta::META_VERSION,
                status: "recording".to_string(),
                started_at,
                size_bytes: 0,
                video_duration_secs: None,
                pp_results: None,
                module_outputs: None,
            };
            crate::recording::meta::write_meta(&settings, &target_path, &meta);
        }

        let result_path = session_dir.to_string_lossy().to_string();
        let mut session_dir = session_dir;
        let manager = Arc::clone(self);
        let username = username.to_string();
        let playlist_url = playlist_url.to_string();
        // merge_format 不在此处固化，合并时从 AppState 动态读取以支持录制中修改生效
        // merge_format is NOT captured here; read from AppState at merge time to support live changes

        tokio::spawn(async move {
            if let Err(e) = manager
                .recording_loop(
                    &username,
                    &playlist_url,
                    &mut session_dir,
                    stop_rx,
                    Arc::clone(&emitter),
                )
                .await
            {
                tracing::error!("Recording error → {}: {}", username, e);
            }

            let record_duration_secs = manager.sessions.read().get(&username).map(|s| {
                chrono::Utc::now()
                    .signed_duration_since(s.started_at)
                    .num_seconds()
                    .max(0) as u64
            });

            manager.sessions.write().remove(&username);

            let was_manual = manager.manually_stopping.write().remove(&username);
            if !was_manual {
                manager.naturally_stopped.write().insert(username.clone());
            }

            manager
                .finish_file(username, session_dir, emitter, record_duration_secs)
                .await;
        });

        Ok(result_path)
    }

    /// Merge a completed file without interrupting the ongoing download session.
    async fn finish_file(
        self: &Arc<Self>,
        username: String,
        session_dir: PathBuf,
        emitter: Arc<dyn Emitter>,
        record_duration_secs: Option<u64>,
    ) {
        let manager = self;
        // 录制结束时读取最新的 merge_format，确保设置变更能在本次合并中生效
        // Read the latest merge_format when recording ends so any in-flight setting change takes effect
        let settings = manager.state.get_settings();
        let merge_format = settings.merge_format.clone();
        // 合并目标位于最终视频目录（可能与分片目录不同）
        // The merge target lives in the final video directory (may differ from the segment directory)
        let merged_path = settings.video_path_for_session_with_format(&session_dir, &merge_format);

        let session_dir_clone = session_dir.clone();
        let username_clone = username.clone();
        let merge_format_clone = merge_format.clone();
        let merged_path_clone = merged_path.clone();
        let state_clone = Arc::clone(&manager.state);
        let emitter_clone = Arc::clone(&emitter);
        let manager_clone = Arc::clone(manager);

        emitter.emit(
            "recording-merge-waiting",
            &serde_json::json!({
                "username": username,
                "session_dir": session_dir.to_string_lossy(),
                "merge_format": merge_format,
            }),
        );

        manager
            .waiting_merge_dirs
            .write()
            .insert(session_dir.clone());

        // 确保合并目标视频的 meta 文件存在，并更新 status = "merging_waiting"
        // Ensure meta file exists and update status = "merging_waiting"
        {
            if let Some(parent) = merged_path.parent() {
                let _ = fs::create_dir_all(parent);
            }
            let stem = session_dir
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("unknown");
            let started_at = crate::commands::recording_cmd::parse_timestamp_from_stem_pub(stem)
                .unwrap_or_else(|| {
                    let local: chrono::DateTime<chrono::Local> = chrono::Utc::now().into();
                    local.to_rfc3339()
                });
            crate::recording::meta::ensure_meta(&settings, &merged_path, &started_at);
            crate::recording::meta::set_status(&settings, &merged_path, "merging_waiting");
        }

        // 后处理线程句柄：合并线程内 spawn，此处 join。
        // Postprocess thread handles: spawned inside the merge thread, joined here.
        let pp_handles: Arc<std::sync::Mutex<Vec<std::thread::JoinHandle<()>>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let pp_handles_inner = Arc::clone(&pp_handles);

        let video_duration_secs = tokio::task::spawn_blocking(move || {
            let _startup_guard = state_clone
                .startup_lock
                .lock()
                .unwrap_or_else(|e| e.into_inner());

            manager_clone
                .waiting_merge_dirs
                .write()
                .remove(&session_dir_clone);
            manager_clone
                .merging_dirs
                .write()
                .insert(session_dir_clone.clone());

            emitter_clone.emit(
                "recording-merging",
                &serde_json::json!({
                    "username": username_clone,
                    "session_dir": session_dir_clone.to_string_lossy(),
                    "merge_format": merge_format_clone,
                }),
            );

            // 更新 meta status = "merging" / Update meta status = "merging"
            let merge_settings = state_clone.get_settings();
            crate::recording::meta::set_status(&merge_settings, &merged_path_clone, "merging");

            let session_dir_str = session_dir_clone.to_string_lossy().to_string();
            let duration = merge_segments(
                &session_dir_clone,
                &merged_path_clone,
                &username_clone,
                &emitter_clone,
                &session_dir_str,
                &merge_settings,
            );

            manager_clone
                .merging_dirs
                .write()
                .remove(&session_dir_clone);

            // 有流水线时：先释放 startup_lock 再启动后处理——后处理包含分钟级
            // 的 Telegram 上传，若在锁内运行，其他录制的合并会被上传阻塞
            // （表现为卡在 "waiting for merge"）。
            // With a pipeline: release startup_lock *before* starting post-processing —
            // post-processing includes minutes-long Telegram uploads; running it inside
            // the lock blocks other recordings' merges (stuck at "waiting for merge").
            let pipeline_non_empty = {
                let pipeline = state_clone.get_pipeline();
                !pipeline.nodes.is_empty()
            };
            drop(_startup_guard);
            if pipeline_non_empty && duration.is_some() && merged_path_clone.exists() {
                let pp_state = Arc::clone(&state_clone);
                let pp_emitter = Arc::clone(&emitter_clone);
                let pp_merged_path = merged_path_clone.clone();
                let handle = std::thread::spawn(move || {
                    // 有后处理流水线：status → "pp_waiting"（由 run_postprocess_for_path 设置）
                    // Has pipeline: status → "pp_waiting" (set by run_postprocess_for_path)
                    let pipeline = pp_state.get_pipeline();
                    crate::commands::postprocess_cmd::run_postprocess_for_path(
                        &pp_merged_path,
                        &pipeline,
                        &pp_emitter,
                        &pp_state,
                    );
                });
                pp_handles_inner.lock().unwrap_or_else(|e| e.into_inner()).push(handle);
            }

            if pipeline_non_empty {
                // 有后处理流水线：后处理完成后由 pipeline/finish 逻辑处理状态
                // （已在上面 spawn 的线程内处理）。此处无需额外动作。
                // Has pipeline: post-processing thread handles the status transitions.
                if duration.is_some() && merged_path_clone.exists() {
                    // 已进入后处理队列，等待其完成 / Queued for post-processing
                }
            } else if duration.is_some() && merged_path_clone.exists() {
                // 无后处理流水线：直接标记为 finish
                // No pipeline: mark as finish directly
                crate::recording::meta::set_status(&merge_settings, &merged_path_clone, "finish");
            } else if duration.is_none() {
                // 合并失败（无分片或 ffmpeg 出错）：删除孤立的 meta 文件和空会话目录，
                // 避免 meta 永久卡在 "merging" 状态。
                // Merge failed (no segments or ffmpeg error): delete orphaned meta file and
                // empty session dir to prevent meta from being stuck at "merging" forever.
                crate::recording::meta::delete_meta(&merge_settings, &merged_path_clone);
                tracing::info!(
                    "Merge produced no output for {} → deleted meta, cleaning up session dir",
                    username_clone
                );
                if session_dir_clone.exists()
                    && let Err(e) = std::fs::remove_dir_all(&session_dir_clone)
                {
                    tracing::warn!(
                        "Failed to remove empty session dir {:?}: {}",
                        session_dir_clone,
                        e
                    );
                }
            }

            duration
        })
        .await
        .unwrap_or(None);

        // 等待本会话的后处理线程结束（后处理自身有自己的并发闸门）
        // Wait for this session's postprocess thread (postprocess has its own concurrency gate)
        let handles: Vec<std::thread::JoinHandle<()>> = std::mem::take(
            &mut *pp_handles.lock().unwrap_or_else(|e| e.into_inner()),
        );
        for handle in handles {
            let _ = handle.join();
        }

        let merged_video_path = merged_path.to_string_lossy().to_string();
        emitter.emit(
            "recording-stopped",
            &serde_json::json!({
                "username": username,
                "session_dir": session_dir.to_string_lossy(),
                "video_path": merged_video_path,
                "record_duration_secs": record_duration_secs,
                "video_duration_secs": video_duration_secs,
                "is_recording": manager.is_recording(&username),
            }),
        );
    }

    /// Rotate only between complete HLS segments, keeping the download history intact.
    async fn rotate_file(
        self: &Arc<Self>,
        username: &str,
        session_dir: &mut PathBuf,
        emitter: &Arc<dyn Emitter>,
    ) -> Result<()> {
        let settings = self.state.get_settings();
        let parent = session_dir.parent().unwrap_or(session_dir).to_path_buf();
        let (now, next_dir) = loop {
            let now = chrono::Utc::now();
            let timestamp = now.with_timezone(&Local).format("%Y%m%d_%H%M%S");
            let candidate = parent.join(format!("{}_{}", username, timestamp));
            // 分片目录与最终视频（含 meta）都不能与候选名冲突
            // Neither the segment dir nor the final video (or its meta) may collide with the candidate name
            let video_candidates = ["mp4", "mkv", settings.merge_format.as_str()].map(|fmt| {
                settings.video_path_for_session_with_format(&candidate, fmt)
            });
            let taken = candidate.exists()
                || video_candidates.iter().any(|p| p.exists())
                || video_candidates.iter().any(|p| {
                    crate::recording::meta::meta_path_for(&settings, p)
                        .map(|m| m.exists())
                        .unwrap_or(false)
                });
            if !taken {
                fs::create_dir(&candidate)?;
                break (now, candidate);
            }
            // Wait for a unique timestamp.
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        };
        let target = settings.video_path_for_session(&next_dir);
        if let Some(p) = target.parent() {
            let _ = fs::create_dir_all(p);
        }
        crate::recording::meta::ensure_meta(&settings, &target, &now.to_rfc3339());
        crate::recording::meta::set_status(&settings, &target, "recording");
        let previous = std::mem::replace(session_dir, next_dir);
        // Lock the completed directory before releasing its active-session protection.
        self.waiting_merge_dirs.write().insert(previous.clone());
        let duration = self.sessions.write().get_mut(username).map(|session| {
            let elapsed = now
                .signed_duration_since(session.started_at)
                .num_seconds()
                .max(0) as u64;
            session.dir_path = session_dir.clone();
            session.started_at = now;
            elapsed
        });
        emitter.emit(
            "recording-started",
            &serde_json::json!({
                "username": username,
                "dir_path": session_dir.to_string_lossy(),
            }),
        );
        let manager = Arc::clone(self);
        let username = username.to_string();
        let emitter = Arc::clone(emitter);
        tokio::spawn(async move {
            manager
                .finish_file(username, previous, emitter, duration)
                .await;
        });
        Ok(())
    }

    /// 手动停止录制（标记为手动停止，防止自动重录）。
    /// Manually stop recording (marks as manually stopped to prevent auto-restart).
    pub async fn stop_recording(self: &Arc<Self>, username: &str) -> Result<()> {
        let session = self
            .sessions
            .read()
            .get(username)
            .cloned()
            .ok_or_else(|| AppError::NotRecording(username.to_string()))?;
        self.manually_stopping.write().insert(username.to_string());
        let _ = session.stop_tx.send(()).await;
        Ok(())
    }

    /// 自动停止录制（不标记为手动停止，允许自动重录）。
    /// Automatically stop recording (not marked as manually stopped, allows auto-restart).
    pub async fn stop_recording_auto(self: &Arc<Self>, username: &str) -> Result<()> {
        let session = self
            .sessions
            .read()
            .get(username)
            .cloned()
            .ok_or_else(|| AppError::NotRecording(username.to_string()))?;
        let _ = session.stop_tx.send(()).await;
        Ok(())
    }

    /// 录制主循环：持续拉取 HLS 播放列表、下载新分片、转换为 TS 格式并写入会话目录。
    /// 代理设置和 Mouflon 密钥在每次循环迭代时动态读取，变更后立即生效。
    ///
    /// Recording main loop: continuously fetches HLS playlists, downloads new segments,
    /// converts to TS format, and writes to the session directory.
    /// Proxy settings and Mouflon keys are read dynamically each iteration and take effect immediately.
    async fn recording_loop(
        self: &Arc<Self>,
        username: &str,
        playlist_url: &str,
        session_dir: &mut PathBuf,
        mut stop_rx: mpsc::Receiver<()>,
        emitter: Arc<dyn Emitter>,
    ) -> Result<()> {
        // 初始设置快照，用于检测变更 / Initial settings snapshot for change detection
        let mut last_settings = self.state.get_settings();
        let mut api = Arc::new(StripchatApi::new(
            last_settings.api_proxy_url.as_deref(),
            last_settings.cdn_proxy_url.as_deref(),
            last_settings.sc_mirror_url.as_deref(),
            Arc::clone(&self.preferred_tld_by_node),
        )?
        .with_mouflon_keys(self.state.get_mouflon_keys())
        .with_resolution_selection(
            last_settings.preferred_resolution,
            &last_settings.resolution_preference,
        ));
        let mut current_playlist_url = playlist_url.to_string();
        let mut url_prefix = get_url_prefix(&current_playlist_url);

        let mut downloaded_sequences: HashSet<u32> = HashSet::new();
        let mut downloaded_urls: HashSet<String> = HashSet::new();
        let mut segment_file_index: u64 = 0;
        let mut mp4_header: Option<Vec<u8>> = None;
        let mut cached_init_url: Option<String> = None;
        let mut recorded_secs = 0.0;
        let mut last_sequence: Option<u32> = None;
        let mut retry_count = 0;
        let mut playlist_refresh_failures = 0;
        let mut consecutive_cdn_failures: usize = 0;
        let mut last_size_snapshot: Option<(u64, std::time::Instant)> = None;
        // 累计成功下载的分片数 / Total successfully downloaded segments
        let mut total_downloaded: u64 = 0;
        // 累计下载失败的分片数 / Total failed segment downloads
        let mut total_failed: u64 = 0;
        const MAX_RETRIES: u32 = 10;
        const MAX_PLAYLIST_REFRESH_FAILURES: u32 = 5;
        const CDN_FAILURE_REFRESH_THRESHOLD: usize = 3;

        tracing::info!("Started recording {} → {:?}", username, session_dir);

        loop {
            // 检测代理/密钥设置变更，变更时重建 api 实例使其立即生效
            // Detect proxy/key setting changes and rebuild api instance for immediate effect
            let current_settings = self.state.get_settings();
            let current_mouflon_keys = self.state.get_mouflon_keys();
            let proxy_changed = current_settings.api_proxy_url != last_settings.api_proxy_url
                || current_settings.cdn_proxy_url != last_settings.cdn_proxy_url
                || current_settings.sc_mirror_url != last_settings.sc_mirror_url;
            let resolution_changed = current_settings.preferred_resolution
                != last_settings.preferred_resolution
                || current_settings.resolution_preference != last_settings.resolution_preference;
            let keys_changed = current_mouflon_keys != *api.mouflon_keys();
            if proxy_changed || resolution_changed || keys_changed {
                match StripchatApi::new(
                    current_settings.api_proxy_url.as_deref(),
                    current_settings.cdn_proxy_url.as_deref(),
                    current_settings.sc_mirror_url.as_deref(),
                    Arc::clone(&self.preferred_tld_by_node),
                ) {
                    Ok(new_api) => {
                        api = Arc::new(new_api
                            .with_mouflon_keys(current_mouflon_keys)
                            .with_resolution_selection(
                                current_settings.preferred_resolution,
                                &current_settings.resolution_preference,
                            ));
                        tracing::info!(
                            "Recording {}: api client rebuilt due to settings change",
                            username
                        );
                    }
                    Err(e) => {
                        tracing::warn!(
                            "Recording {}: failed to rebuild api client: {}",
                            username,
                            e
                        );
                    }
                }
                last_settings = current_settings.clone();
            }
            let mouflon_keys = api.mouflon_keys().clone();

            // Graceful stop: finish the current segment round before merging.
            // Cancelling fetch_segments here used to leave ffmpeg conversions
            // half-written, which produced cut/missing chunks in the final video.
            if stop_rx.try_recv().is_ok() {
                tracing::info!("Stop signal received → {}", username);
                break;
            }

            let mut wait_next_round = true;
            let result = self
                .fetch_segments(
                    &api,
                    &current_playlist_url,
                    &url_prefix,
                    &mouflon_keys,
                    session_dir,
                    username,
                    &mut downloaded_sequences,
                    &mut downloaded_urls,
                    &mut mp4_header,
                    &mut cached_init_url,
                    &mut recorded_secs,
                    &mut last_sequence,
                    &mut segment_file_index,
                    &emitter,
                )
                .await;
            let had_written = matches!(&result, Ok((n, _)) if *n > 0);
            match result {
                Ok((n, cdn_fail)) => {
                    if cdn_fail > 0 {
                        consecutive_cdn_failures += cdn_fail;
                        total_failed += cdn_fail as u64;
                    }
                    if n > 0 {
                        consecutive_cdn_failures = 0;
                        retry_count = 0;
                        total_downloaded += n as u64;
                        let size_bytes = dir_size_bytes(&*session_dir).unwrap_or(0);
                        let now = std::time::Instant::now();
                        let speed_bps = last_size_snapshot.map(|(prev_size, prev_time)| {
                            let dt = now.duration_since(prev_time).as_secs_f64();
                            let ds = size_bytes.saturating_sub(prev_size) as f64;
                            if dt > 0.0 { ds / dt } else { 0.0 }
                        });
                        last_size_snapshot = Some((size_bytes, now));

                        // 计算对应的视频文件路径（合并目标），用于 meta 更新和前端匹配
                        // Compute the corresponding video file path (merge target) for meta update and frontend matching
                        let settings = self.state.get_settings();
                        let video_path =
                            settings.video_path_for_session(session_dir.as_path());

                        // 将实时文件大小写入 meta JSON
                        // Write real-time file size to meta JSON
                        if let Some(mut meta) =
                            crate::recording::meta::read_meta(&settings, &video_path)
                            && meta.size_bytes != size_bytes
                        {
                            meta.size_bytes = size_bytes;
                            crate::recording::meta::write_meta(&settings, &video_path, &meta);
                        }

                        let mut payload = serde_json::json!({
                            "path": video_path.to_string_lossy(),
                            "segment_count": downloaded_sequences.len(),
                            "size_bytes": size_bytes,
                            "segments_downloaded": total_downloaded,
                            "segments_failed": total_failed,
                        });
                        if let Some(spd) = speed_bps {
                            payload["speed_bps"] = serde_json::json!(spd);
                        }
                        emitter.emit("recording-file-update", &payload);
                    } else {
                        retry_count += 1;
                    }
                    if consecutive_cdn_failures >= CDN_FAILURE_REFRESH_THRESHOLD {
                        tracing::error!(
                            "Fetch error → {}: {} consecutive CDN failures, refreshing playlist",
                            username, consecutive_cdn_failures
                        );
                        consecutive_cdn_failures = 0;
                        match api.get_stream_info(username, true).await {
                            Ok(info) => {
                                if let Some(new_url) = info.playlist_url {
                                    tracing::info!("Refreshed playlist URL → {}", username);
                                    url_prefix = get_url_prefix(&new_url);
                                    current_playlist_url = new_url;
                                    playlist_refresh_failures = 0;
                                    retry_count = 0;
                                    wait_next_round = false;
                                } else if !info.is_recordable {
                                    tracing::warn!("Stream no longer recordable → {} (status: {}), stopping", username, info.status);
                                    break;
                                } else {
                                    tracing::warn!("No playlist URL yet → {} (status: {}), retrying", username, info.status);
                                    playlist_refresh_failures += 1;
                                }
                            }
                            Err(refresh_err) => {
                                tracing::error!("Playlist refresh failed → {}: {}", username, refresh_err);
                                playlist_refresh_failures += 1;
                            }
                        }
                        if playlist_refresh_failures >= MAX_PLAYLIST_REFRESH_FAILURES {
                            tracing::warn!("Stream ended → {} (playlist refresh failed {} times)", username, playlist_refresh_failures);
                            break;
                        }
                    }
                }
                Err(e) => {
                    tracing::error!("Fetch error → {}: {}, attempting playlist refresh", username, e);
                    consecutive_cdn_failures = 0;
                    match api.get_stream_info(username, true).await {
                        Ok(info) => {
                            if let Some(new_url) = info.playlist_url {
                                tracing::info!("Refreshed playlist URL → {}", username);
                                url_prefix = get_url_prefix(&new_url);
                                current_playlist_url = new_url;
                                playlist_refresh_failures = 0;
                                retry_count = 0;
                                wait_next_round = false;
                            } else if !info.is_recordable {
                                tracing::warn!("Stream no longer recordable → {} (status: {}), stopping", username, info.status);
                                break;
                            } else {
                                tracing::warn!("No playlist URL yet → {} (status: {}), retrying", username, info.status);
                                playlist_refresh_failures += 1;
                            }
                        }
                        Err(refresh_err) => {
                            tracing::error!("Playlist refresh failed → {}: {}", username, refresh_err);
                            playlist_refresh_failures += 1;
                        }
                    }
                    if playlist_refresh_failures >= MAX_PLAYLIST_REFRESH_FAILURES {
                        tracing::warn!("Stream ended → {} (playlist refresh failed {} times)", username, playlist_refresh_failures);
                        break;
                    }
                }
            }
            if retry_count >= MAX_RETRIES {
                tracing::warn!("Stream ended → {} (max retries)", username);
                break;
            }
            if wait_next_round {
                // 自适应轮询间隔：若本轮有新分片写入，说明正在产生数据，缩短等待至 500ms 紧随直播边缘；
                // 若未发现新分片，则等待 1000ms 避免过度请求
                // Adaptive polling: 500ms when actively receiving segments, 1000ms when idle
                let sleep_ms = if had_written {
                    500
                } else {
                    1000
                };
                tokio::time::sleep(tokio::time::Duration::from_millis(sleep_ms)).await;
            }

            // Honour a stop that arrived while the round was in flight, but
            // only after all of its segments are fully written and renamed.
            if stop_rx.try_recv().is_ok() {
                tracing::info!("Stop signal received → {}", username);
                break;
            }
        }

        tracing::info!("Finished recording {} → {:?}", username, session_dir);
        Ok(())
    }

    /// 拉取一次播放列表并下载所有新分片。
    /// 返回 `(写入的分片数, CDN 失败的分片数)`。
    ///
    /// Fetch the playlist once and download all new segments.
    /// Returns `(number of segments written, number of CDN failures)`.
    #[allow(clippy::too_many_arguments)]
    async fn fetch_segments(
        self: &Arc<Self>,
        api: &Arc<StripchatApi>,
        playlist_url: &str,
        url_prefix: &str,
        mouflon_keys: &HashMap<String, String>,
        session_dir: &mut PathBuf,
        username: &str,
        downloaded_sequences: &mut HashSet<u32>,
        downloaded_urls: &mut HashSet<String>,
        mp4_header: &mut Option<Vec<u8>>,
        cached_init_url: &mut Option<String>,
        recorded_secs: &mut f64,
        last_sequence: &mut Option<u32>,
        segment_file_index: &mut u64,
        emitter: &Arc<dyn Emitter>,
    ) -> Result<(usize, usize)> {
        let playlist = api.fetch_playlist(playlist_url).await?;
        let (segments, init_url) = parse_playlist(&playlist, url_prefix, mouflon_keys)?;
        let init_url_path = |u: &str| u.split('?').next().unwrap_or(u).to_string();
        let new_init_path = init_url.as_deref().map(init_url_path);
        let cached_init_path = cached_init_url.as_deref().map(init_url_path);
        if new_init_path.is_some()
            && new_init_path != cached_init_path
            && let Some(ref url) = init_url
        {
            match api.download_segment(url).await {
                Ok(data) => {
                    tracing::info!("Cached init segment → {} ({} bytes)", username, data.len());
                    *mp4_header = Some(data);
                    *cached_init_url = Some(url.clone());
                }
                Err(e) => {
                    tracing::error!(
                        "Failed to download init segment: {}, skipping this round",
                        e
                    );
                    return Ok((0, 0));
                }
            }
        }

        // 1. 过滤出尚未下载的新分片，严格跳过已在历史集合中的分片（避免滑动窗口重叠造成误判定）
        // Filter out already downloaded segments; existing window overlap must never be treated as reset
        let mut pending_segments: Vec<crate::recording::hls::HlsSegment> = Vec::new();
        for segment in segments {
            let clean_url = segment.url.split(['?', '#']).next().unwrap_or(&segment.url);
            let is_duplicate = downloaded_sequences.contains(&segment.sequence)
                || downloaded_urls.contains(clean_url)
                || downloaded_urls.contains(&segment.url);

            if is_duplicate {
                continue;
            }

            // 检测流真实重连 / 序号大幅回退：仅对新分片且上一序号较大（>50）且序号重置回 10 以内或落差 > 50 时触发
            // Detect genuine stream reconnect / restart: only for unseen segments where sequence resets to near 0
            if let Some(prev) = *last_sequence {
                if prev > 50 && (segment.sequence <= 10 || prev.saturating_sub(segment.sequence) > 50) {
                    tracing::warn!(
                        "Stream sequence reset detected for {} (prev: {}, new: {}), resetting deduplication caches",
                        username, prev, segment.sequence
                    );
                    downloaded_sequences.clear();
                    downloaded_urls.clear();
                    *last_sequence = None;
                }
            }

            pending_segments.push(segment);
        }

        if pending_segments.is_empty() {
            return Ok((0, 0));
        }

        let new_segments = pending_segments.len();
        let mut written = 0;
        let mut cdn_failures = 0;

        // 2. 并发下载所有新分片（带重试与小退避），缩短下载耗时以紧跟直播边缘
        // Concurrently download new segments (with retries) to stay tightly at the live edge
        let mut join_set = tokio::task::JoinSet::new();
        for (idx, seg) in pending_segments.iter().enumerate() {
            let api = Arc::clone(api);
            let url = seg.url.clone();
            join_set.spawn(async move {
                let mut res = api.download_segment(&url).await;
                if res.is_err() || res.as_ref().map(|d| d.len() <= 1000).unwrap_or(false) {
                    for attempt in 1..=2 {
                        tokio::time::sleep(tokio::time::Duration::from_millis(attempt * 150)).await;
                        res = api.download_segment(&url).await;
                        if let Ok(ref d) = res {
                            if d.len() > 1000 {
                                break;
                            }
                        }
                    }
                }
                (idx, res)
            });
        }

        let mut download_results: Vec<Option<Result<Vec<u8>>>> = (0..pending_segments.len()).map(|_| None).collect();
        while let Some(join_res) = join_set.join_next().await {
            if let Ok((idx, download_res)) = join_res {
                download_results[idx] = Some(download_res);
            }
        }

        // 3. 按原始播放列表顺序处理转换和写入，保证文件命名和时间戳单调递增
        // Process conversion and append in original playlist order to maintain monotonic indexing
        for (segment, download_opt) in pending_segments.into_iter().zip(download_results) {
            let download_res = download_opt.unwrap_or_else(|| Err(AppError::Other("Download cancelled".to_string())));
            let clean_url = segment.url.split(['?', '#']).next().unwrap_or(&segment.url).to_string();
            match download_res {
                Ok(data) => {
                    if data.len() > 1000 {
                        let limit = self.state.get_settings().max_recording_duration_secs;
                        if recording_limit_reached(*recorded_secs, limit) {
                            self.rotate_file(username, session_dir, emitter).await?;
                            *recorded_secs = 0.0;
                            *last_sequence = None;
                            *segment_file_index = 0;
                        }

                        *segment_file_index += 1;
                        let ts_path = session_dir
                            .join(format!("{}_seg{:06}.ts", username, *segment_file_index));

                        let fmp4: Vec<u8> = match mp4_header.as_deref() {
                            Some(h) => {
                                let mut v = Vec::with_capacity(h.len() + data.len());
                                v.extend_from_slice(h);
                                v.extend_from_slice(&data);
                                v
                            }
                            None => data,
                        };

                        match convert_to_ts(fmp4, &ts_path).await {
                            Ok(_) => {
                                let discontinuity = match *last_sequence {
                                    Some(prev) => segment.sequence > prev.saturating_add(1) || segment.sequence < prev,
                                    None => false,
                                };
                                append_to_m3u8(session_dir, &ts_path, segment.duration_secs, discontinuity);
                                *last_sequence = Some(segment.sequence);
                                downloaded_sequences.insert(segment.sequence);
                                downloaded_urls.insert(clean_url);
                                downloaded_urls.insert(segment.url);
                                written += 1;
                                *recorded_secs += segment.duration_secs;
                            }
                            Err(e) => {
                                tracing::error!(
                                    "ffmpeg convert failed → segment {} (file index {}): {}",
                                    segment.sequence,
                                    *segment_file_index,
                                    e
                                );
                            }
                        }
                    } else {
                        tracing::warn!(
                            "Downloaded segment {} too small ({} bytes) after retries, skipping",
                            segment.sequence,
                            data.len()
                        );
                        cdn_failures += 1;
                    }
                }
                Err(e) => {
                    tracing::error!("Failed to download segment {}: {}", segment.sequence, e);
                    cdn_failures += 1;
                }
            }
        }

        if new_segments > 0 && cdn_failures == new_segments {
            return Err(AppError::Other(format!(
                "All {} new segments failed (CDN 404 / token expired), refreshing playlist",
                new_segments
            )));
        }

        Ok((written, cdn_failures))
    }
}
/// Zero disables rotation. Duration is measured from successfully recorded media.
fn recording_limit_reached(recorded_secs: f64, limit: u64) -> bool {
    limit > 0 && recorded_secs >= limit as f64
}

/// 检查 ffmpeg 是否在 PATH 中可用。
/// Check if ffmpeg is available on PATH.
pub fn ffmpeg_available() -> bool {
    Command::new("ffmpeg")
        .arg("-version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok()
}

/// 使用 ffmpeg 将 fMP4 数据转换为 MPEG-TS 格式（通过 stdin 管道传入）。
/// Convert fMP4 data to MPEG-TS format using ffmpeg (piped via stdin).
async fn convert_to_ts(fmp4_data: Vec<u8>, ts_path: &PathBuf) -> Result<()> {
    let _permit = FFMPEG_CONVERT_SEMAPHORE
        .acquire()
        .await
        .map_err(|e| AppError::Other(format!("ffmpeg semaphore: {}", e)))?;

    // Write to a temporary file first, then atomically rename on success.
    // This guarantees the merge never sees a half-written .ts segment if a
    // recording is stopped while ffmpeg is still converting it.
    let tmp_path = ts_path.with_extension("ts.tmp");
    let _ = fs::remove_file(&tmp_path);

    let mut child = {
        #[cfg(windows)]
        {
            tokio::process::Command::new("ffmpeg")
                .creation_flags(0x08000000)
                .args(["-y", "-loglevel", "warning", "-i", "pipe:0", "-c", "copy", "-muxdelay", "0", "-f", "mpegts"])
                .arg(&tmp_path)
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .spawn()
                .map_err(|e| AppError::Other(format!("Failed to spawn ffmpeg: {}", e)))?
        }
        #[cfg(not(windows))]
        {
            tokio::process::Command::new("ffmpeg")
                .args(["-y", "-loglevel", "warning", "-i", "pipe:0", "-c", "copy", "-muxdelay", "0", "-f", "mpegts"])
                .arg(&tmp_path)
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .spawn()
                .map_err(|e| AppError::Other(format!("Failed to spawn ffmpeg: {}", e)))?
        }
    };

    if let Some(mut stdin) = child.stdin.take() {
        use tokio::io::AsyncWriteExt;
        let _ = stdin.write_all(&fmp4_data).await;
        let _ = stdin.shutdown().await;
    }

    let output = child
        .wait_with_output()
        .await
        .map_err(|e| {
            let _ = fs::remove_file(&tmp_path);
            AppError::Other(format!("ffmpeg wait: {}", e))
        })?;

    if !output.status.success() {
        let err_msg = String::from_utf8_lossy(&output.stderr);
        let _ = fs::remove_file(&tmp_path);
        return Err(AppError::Other(format!(
            "ffmpeg exited with {}: {}",
            output.status,
            err_msg.trim()
        )));
    }

    fs::rename(&tmp_path, ts_path).map_err(|e| {
        let _ = fs::remove_file(&tmp_path);
        AppError::Other(format!("Failed to finalize segment {:?}: {}", ts_path, e))
    })?;
    Ok(())
}

/// 将 TS 分片文件名追加到会话目录的 playlist.m3u8（标准 HLS 格式）。
/// Append a TS segment filename to the session directory's playlist.m3u8 (standard HLS format).
///
/// 首次写入时自动添加 M3U8 文件头（`#EXTM3U`、`#EXT-X-VERSION:3`、`#EXT-X-TARGETDURATION:10`）。
/// 包含真实分片时长；若存在丢片或序号不连续则写入 `#EXT-X-DISCONTINUITY`。
/// Automatically writes the M3U8 header on first write.
/// Writes real segment duration and `#EXT-X-DISCONTINUITY` if segments were dropped.
fn append_to_m3u8(
    session_dir: &std::path::Path,
    ts_path: &std::path::Path,
    duration_secs: f64,
    discontinuity: bool,
) {
    let m3u8_path = session_dir.join("playlist.m3u8");
    let Some(filename) = ts_path.file_name().and_then(|n| n.to_str()) else {
        return;
    };

    // 首次创建时写入 M3U8 文件头 / Write M3U8 header on first creation
    let needs_header = !m3u8_path.exists();
    let mut file = match fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&m3u8_path)
    {
        Ok(f) => f,
        Err(e) => {
            tracing::error!("Failed to open playlist.m3u8: {}", e);
            return;
        }
    };

    if needs_header
        && let Err(e) = file.write_all(b"#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:10\n#EXT-X-MEDIA-SEQUENCE:0\n")
    {
        tracing::error!("Failed to write M3U8 header: {}", e);
        return;
    }

    if discontinuity && !needs_header {
        let _ = file.write_all(b"#EXT-X-DISCONTINUITY\n");
    }

    // 写入真实分片时长（默认兜底 2.0 秒）/ Write real duration (fallback 2.0s)
    let dur = if duration_secs > 0.0 && duration_secs.is_finite() {
        duration_secs
    } else {
        2.0
    };
    let line = format!("#EXTINF:{:.3},\n{}\n", dur, filename);
    if let Err(e) = file.write_all(line.as_bytes()) {
        tracing::error!("Failed to update playlist.m3u8: {}", e);
    }
}

/// 计算目录中所有文件的总大小（字节）。
/// Calculate the total size of all files in a directory (bytes).
pub fn dir_size_bytes(dir: &PathBuf) -> std::io::Result<u64> {
    let mut total = 0u64;
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let meta = entry.metadata()?;
        if meta.is_file() {
            total += meta.len();
        }
    }
    Ok(total)
}

/// 使用 ffmpeg 将会话目录中的所有 TS 分片合并为单个视频文件。
/// 合并过程中定期发送 `merge-progress` 事件，合并完成后删除会话目录。
///
/// Merge all TS segments in the session directory into a single video file using ffmpeg.
/// Periodically emits `merge-progress` events during merging; deletes the session directory after completion.
///
/// # 参数 / Parameters
/// - `session_dir`: 存放 .ts 分片的会话目录 / Session directory holding the .ts segments
/// - `output_path`: 合并后的视频路径（位于最终视频目录）/ Path of the merged video (inside the final video directory)
///
/// # 返回值 / Returns
/// 合并后视频的时长（秒），失败时返回 `None`。
/// Duration of the merged video (seconds), or `None` on failure.
fn merge_segments(
    session_dir: &PathBuf,
    output_path: &std::path::Path,
    username: &str,
    emitter: &Arc<dyn Emitter>,
    session_dir_str: &str,
    settings: &crate::config::settings::Settings,
) -> Option<u64> {
    let m3u8_path = session_dir.join("playlist.m3u8");
    if !m3u8_path.exists() {
        tracing::warn!(
            "playlist.m3u8 not found in {:?}, skipping merge",
            session_dir
        );
        return None;
    }

    // 在合并前写入 #EXT-X-ENDLIST 标记，使 M3U8 成为完整的 VOD 播放列表
    // Write #EXT-X-ENDLIST before merging to finalize the M3U8 as a complete VOD playlist
    if let Err(e) = fs::OpenOptions::new()
        .append(true)
        .open(&m3u8_path)
        .and_then(|mut f| f.write_all(b"#EXT-X-ENDLIST\n"))
    {
        tracing::warn!("Failed to write #EXT-X-ENDLIST: {}", e);
    }

    // 最终视频可能位于独立目录，确保其父目录存在
    // The final video may live in a separate directory; make sure its parent exists
    if let Some(parent) = output_path.parent()
        && let Err(e) = fs::create_dir_all(parent)
    {
        tracing::error!("Failed to create video dir {:?}: {}", parent, e);
        return None;
    }

    tracing::info!("Merging {} → {:?}", username, output_path);

    let total_bytes: u64 = fs::read_dir(session_dir)
        .map(|entries| {
            entries
                .flatten()
                .filter_map(|e| {
                    let p = e.path();
                    if p.extension().and_then(|x| x.to_str()) == Some("ts") {
                        fs::metadata(&p).ok().map(|m| m.len())
                    } else {
                        None
                    }
                })
                .sum()
        })
        .unwrap_or(0);

    let _permit = match tokio::runtime::Handle::try_current() {
        Ok(handle) => Some(
            handle
                .block_on(FFMPEG_MERGE_SEMAPHORE.acquire())
                .expect("ffmpeg merge semaphore closed"),
        ),
        Err(_) => tokio::runtime::Builder::new_current_thread()
            .build()
            .ok()
            .map(|rt| {
                rt.block_on(FFMPEG_MERGE_SEMAPHORE.acquire())
                    .expect("ffmpeg merge semaphore closed")
            }),
    };

    // 生成 concat 列表文件：以磁盘上实际存在的 .ts 分片为准并按文件名排序合并，
    // 同时对比 playlist.m3u8 记录缺失项，避免 m3u8 写入失败/不完整时导致合并视频缺段或顺序错乱。
    // Generate concat list file: use the actual .ts segments present on disk in filename order,
    // while checking playlist.m3u8 for missing entries, so merge is complete and correctly ordered.
    let concat_path = session_dir.join("concat_list.txt");
    let mut use_concat = false;
    let mut concat_lines = Vec::new();

    let mut listed_in_m3u8: std::collections::HashSet<String> = std::collections::HashSet::new();
    if let Ok(content) = fs::read_to_string(&m3u8_path) {
        for line in content.lines() {
            let trimmed = line.trim();
            if !trimmed.is_empty() && !trimmed.starts_with('#') {
                listed_in_m3u8.insert(trimmed.to_string());
            }
        }
    }

    let mut ts_files: Vec<String> = fs::read_dir(session_dir)
        .map(|entries| {
            entries
                .flatten()
                .filter_map(|e| {
                    let p = e.path();
                    if p.extension().and_then(|x| x.to_str()) == Some("ts") {
                        p.file_name().and_then(|n| n.to_str()).map(|s| s.to_string())
                    } else {
                        None
                    }
                })
                .collect()
        })
        .unwrap_or_default();
    ts_files.sort();

    if !listed_in_m3u8.is_empty() {
        for name in &ts_files {
            if !listed_in_m3u8.contains(name) {
                tracing::warn!(
                    "Merge: adding segment missing from playlist.m3u8: {}",
                    name
                );
            }
        }
    }

    for name in ts_files {
        let escaped = name.replace('\'', "'\\''");
        concat_lines.push(format!("file '{}'", escaped));
    }

    if !concat_lines.is_empty() {
        if let Ok(mut f) = fs::File::create(&concat_path) {
            let text = concat_lines.join("\n");
            if f.write_all(text.as_bytes()).is_ok() {
                use_concat = true;
            }
        }
    }

    let mut cmd = Command::new("ffmpeg");
    cmd.arg("-y");
    if use_concat {
        cmd.current_dir(session_dir)
            .args(["-f", "concat", "-safe", "0", "-i"])
            .arg(&concat_path);
    } else {
        cmd.args(["-allowed_extensions", "ALL", "-i"])
            .arg(&m3u8_path);
    }

    // 时间戳平滑与容错参数：重新生成 PTS、防止负时间戳导致的帧丢失（避免 +discardcorrupt 误丢正常帧）
    // Timestamp smoothing & robustness: regenerate PTS, avoid negative TS (without discardcorrupt)
    cmd.args([
        "-c",
        "copy",
        "-fflags",
        "+genpts",
        "-avoid_negative_ts",
        "make_zero",
    ]);

    // MP4/MOV 格式启用 faststart 标记（将 moov atom 移至文件头部，方便边下边播与即时定位）
    let is_mp4 = output_path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| {
            let l = e.to_ascii_lowercase();
            l == "mp4" || l == "m4v" || l == "mov"
        })
        .unwrap_or(false);
    if is_mp4 {
        cmd.args(["-movflags", "+faststart"]);
    }

    cmd.arg(&output_path)
        .stdout(Stdio::null())
        .stderr(Stdio::piped());

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("Failed to spawn ffmpeg → merge: {}", e);
            return None;
        }
    };

    // 独立线程排空 stderr 管道，防止大量警告撑爆 64KB 管道缓冲区导致 ffmpeg 死锁
    // Drain stderr concurrently in a background thread to prevent pipe buffer deadlock
    let stderr_reader = child.stderr.take().map(|mut r| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            use std::io::Read;
            let _ = r.read_to_end(&mut buf);
            buf
        })
    });

    let poll_interval = std::time::Duration::from_millis(500);
    loop {
        std::thread::sleep(poll_interval);
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                let out_bytes = fs::metadata(&output_path).map(|m| m.len()).unwrap_or(0);
                emitter.emit(
                    "merge-progress",
                    &serde_json::json!({
                        "session_dir": session_dir_str,
                        "video_path": output_path.to_string_lossy(),
                        "out_bytes": out_bytes,
                        "total_bytes": total_bytes,
                    }),
                );
            }
            Err(e) => {
                tracing::error!("ffmpeg wait error: {}", e);
                break;
            }
        }
    }

    let status = child.wait();
    let stderr_output = stderr_reader.and_then(|h| h.join().ok()).unwrap_or_default();
    match status {
        Ok(s) if s.success() => {
            tracing::info!("Merge complete: {:?}", output_path);
            emitter.emit(
                "merge-progress",
                &serde_json::json!({
                    "session_dir": session_dir_str,
                    "video_path": output_path.to_string_lossy(),
                    "out_bytes": total_bytes,
                    "total_bytes": total_bytes,
                }),
            );
            if let Err(e) = fs::remove_dir_all(session_dir) {
                tracing::error!("Failed to remove segment dir: {}", e);
            }
            let duration = get_video_duration(&output_path);

            // 更新 meta：填入实际大小、时长，status 暂设为 "merging"（调用方会进一步更新）
            // Update meta: fill in actual size and duration; status temporarily "merging"
            // (caller will update it further)
            let size_bytes = fs::metadata(&output_path).map(|m| m.len()).unwrap_or(0);
            if let Some(mut meta) = crate::recording::meta::read_meta(settings, output_path) {
                meta.size_bytes = size_bytes;
                meta.video_duration_secs = duration;
                // 保留 status 不变，由调用方根据是否有后处理流水线决定下一个状态
                // Keep status unchanged; caller decides next status based on pipeline
                crate::recording::meta::write_meta(settings, output_path, &meta);
            }

            duration
        }
        Ok(s) => {
            let err_msg = String::from_utf8_lossy(&stderr_output);
            tracing::warn!("ffmpeg merge exited with {}: {}", s, err_msg.trim());
            None
        }
        Err(e) => {
            tracing::error!("Failed to spawn ffmpeg → merge: {}", e);
            None
        }
    }
}

/// 使用 ffprobe 获取视频文件的时长（秒）。
/// Get the duration of a video file in seconds using ffprobe.
pub fn get_video_duration(path: &std::path::Path) -> Option<u64> {
    let output = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-show_entries",
            "format=duration",
            "-of",
            "default=noprint_wrappers=1:nokey=1",
        ])
        .arg(path)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()?;

    let s = String::from_utf8_lossy(&output.stdout);
    s.trim().parse::<f64>().ok().map(|d| d as u64)
}

/// 启动时扫描输出目录，合并所有遗留的未完成录制片段，并对未后处理的视频触发后处理。
/// On startup, scan the output directory to merge all leftover incomplete recording segments,
/// and trigger post-processing for videos that haven't been processed yet.
pub fn startup_merge_leftover_segments(
    output_dir: &std::path::Path,
    merge_format: &str,
    emitter: &Arc<dyn Emitter>,
    recorder: &Arc<RecorderManager>,
) -> Vec<PathBuf> {
    let state = Arc::clone(&recorder.state);
    let settings = state.get_settings();
    let pipeline = state.get_pipeline();

    let mut segment_dirs: Vec<PathBuf> = Vec::new();
    if output_dir.exists() {
        collect_segment_dirs(output_dir, &mut segment_dirs);
        segment_dirs.sort_by_key(|p| session_dir_timestamp(p));
    }

    let mut unprocessed_videos: Vec<PathBuf> = Vec::new();
    if !pipeline.nodes.is_empty() {
        let pp_results = state.data.read().pp_results.clone();
        for root in settings.video_scan_roots() {
            collect_unprocessed_videos(
                &root,
                merge_format,
                &pp_results,
                &mut unprocessed_videos,
            );
        }
        unprocessed_videos.sort_by_key(|p| {
            let stem = p.file_stem().and_then(|s| s.to_str()).unwrap_or("");
            session_dir_timestamp_from_stem(stem)
        });
    }

    if segment_dirs.is_empty() && unprocessed_videos.is_empty() {
        return Vec::new();
    }

    let _startup_guard = state.startup_lock.lock().unwrap_or_else(|e| e.into_inner());

    let mut merged_paths = Vec::new();
    let mut pp_handles: Vec<std::thread::JoinHandle<()>> = Vec::new();

    for video_path in unprocessed_videos {
        if !pipeline.nodes.is_empty() {
            let pp_state = Arc::clone(&state);
            let pp_emitter = Arc::clone(emitter);
            let pp_pipeline = pipeline.clone();
            let handle = std::thread::spawn(move || {
                crate::commands::postprocess_cmd::run_postprocess_for_path(
                    &video_path,
                    &pp_pipeline,
                    &pp_emitter,
                    &pp_state,
                );
            });
            pp_handles.push(handle);
        }
    }

    for path in &segment_dirs {
        let username = path
            .parent()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            .unwrap_or("unknown");
        recorder.waiting_merge_dirs.write().insert(path.clone());
        emitter.emit(
            "recording-merge-waiting",
            &serde_json::json!({
                "username": username,
                "session_dir": path.to_string_lossy(),
                "merge_format": merge_format,
            }),
        );

        // 预创建合并目标视频的 meta 文件
        // Pre-create meta file for the merge target video
        let stem = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("unknown");
        let target_path = settings.video_path_for_session_with_format(path, merge_format);
        if let Some(parent) = target_path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let started_at = crate::commands::recording_cmd::parse_timestamp_from_stem_pub(stem)
            .unwrap_or_else(|| {
                fs::metadata(path)
                    .ok()
                    .and_then(|m| m.modified().ok())
                    .map(|t| {
                        let dt: chrono::DateTime<chrono::Local> = t.into();
                        dt.to_rfc3339()
                    })
                    .unwrap_or_default()
            });
        crate::recording::meta::ensure_meta(&settings, &target_path, &started_at);
    }

    for path in segment_dirs {
        let stem = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("unknown")
            .to_string();
        let username = path
            .parent()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            .unwrap_or("unknown")
            .to_string();
        let output_path = settings.video_path_for_session_with_format(&path, merge_format);
        let path_str = path.to_string_lossy().to_string();

        recorder.waiting_merge_dirs.write().remove(&path);
        recorder.merging_dirs.write().insert(path.clone());
        emitter.emit(
            "recording-merging",
            &serde_json::json!({
                "username": username,
                "session_dir": path_str,
                "merge_format": merge_format,
            }),
        );

        // 更新 meta status = "merging" / Update meta status = "merging"
        crate::recording::meta::set_status(&settings, &output_path, "merging");

        let video_duration_secs =
            merge_segments(&path, &output_path, &stem, emitter, &path_str, &settings);

        recorder.merging_dirs.write().remove(&path);
        emitter.emit(
            "recording-stopped",
            &serde_json::json!({
                "username": username,
                "session_dir": path_str,
                "video_path": output_path.to_string_lossy(),
                "record_duration_secs": serde_json::Value::Null,
                "video_duration_secs": video_duration_secs,
            }),
        );

        if output_path.exists() {
            merged_paths.push(output_path.clone());
            if !pipeline.nodes.is_empty() {
                let pp_state = Arc::clone(&state);
                let pp_emitter = Arc::clone(emitter);
                let pp_pipeline = pipeline.clone();
                let handle = std::thread::spawn(move || {
                    crate::commands::postprocess_cmd::run_postprocess_for_path(
                        &output_path,
                        &pp_pipeline,
                        &pp_emitter,
                        &pp_state,
                    );
                });
                pp_handles.push(handle);
            } else {
                // 无后处理流水线：直接标记为 finish
                // No pipeline: mark as finish directly
                crate::recording::meta::set_status(&settings, &output_path, "finish");
            }
        }
    }

    for handle in pp_handles {
        let _ = handle.join();
    }

    merged_paths
}

pub fn startup_remove_empty_dirs(output_dir: &std::path::Path) {
    if !output_dir.exists() {
        return;
    }

    let removed = remove_empty_dirs_recursive(output_dir, false);
    if removed > 0 {
        tracing::info!(
            "Startup: removed {} empty directories under {:?}",
            removed,
            output_dir
        );
    }
}

fn remove_empty_dirs_recursive(dir: &std::path::Path, remove_self: bool) -> usize {
    let mut removed = 0;

    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => return 0,
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            removed += remove_empty_dirs_recursive(&path, true);
        }
    }

    if remove_self {
        let is_empty = fs::read_dir(dir)
            .map(|mut entries| entries.next().is_none())
            .unwrap_or(false);
        if is_empty && fs::remove_dir(dir).is_ok() {
            removed += 1;
        }
    }

    removed
}

fn collect_segment_dirs(dir: &std::path::Path, result: &mut Vec<PathBuf>) {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let has_segments = fs::read_dir(&path)
            .map(|mut e| {
                e.any(|f| {
                    f.ok()
                        .and_then(|f| {
                            f.path()
                                .extension()
                                .and_then(|x| x.to_str())
                                .map(|x| x == "ts")
                                .filter(|&b| b)
                                .map(|_| ())
                        })
                        .is_some()
                })
            })
            .unwrap_or(false);
        if has_segments {
            result.push(path);
        } else {
            collect_segment_dirs(&path, result);
        }
    }
}

fn session_dir_timestamp(path: &std::path::Path) -> String {
    let stem = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    session_dir_timestamp_from_stem(stem)
}

fn session_dir_timestamp_from_stem(stem: &str) -> String {
    let parts: Vec<&str> = stem.split('_').collect();
    if parts.len() >= 2 {
        let date = parts[parts.len() - 2];
        let time = parts[parts.len() - 1];
        if date.len() == 8
            && time.len() == 6
            && date.chars().all(|c| c.is_ascii_digit())
            && time.chars().all(|c| c.is_ascii_digit())
        {
            return format!("{}_{}", date, time);
        }
    }
    stem.to_string()
}

fn collect_unprocessed_videos(
    dir: &std::path::Path,
    merge_format: &str,
    pp_results: &[String],
    result: &mut Vec<PathBuf>,
) {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_unprocessed_videos(&path, merge_format, pp_results, result);
        } else if path.is_file() {
            let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
            if ext == merge_format {
                let path_str = path.to_string_lossy().to_string();
                // 不在 pp_results 目录中，说明从未执行过后处理
                // Not in pp_results directory means post-processing has never been run
                if !pp_results.contains(&path_str) {
                    result.push(path);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::settings::Settings;
    use crate::core::emitter::NoopEmitter;

    fn generate_test_ts(path: &std::path::Path, duration_secs: u32) {
        let status = Command::new("ffmpeg")
            .args([
                "-y",
                "-f",
                "lavfi",
                "-i",
                &format!("testsrc=duration={}:size=320x240:rate=30", duration_secs),
                "-c:v",
                "libx264",
                "-f",
                "mpegts",
            ])
            .arg(path)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        assert!(status.is_ok() && status.unwrap().success());
    }

    #[test]
    fn test_merge_segments_no_missing_segments() {
        let temp_dir = std::env::temp_dir().join(format!("rec_test_{}", std::process::id()));
        let session_dir = temp_dir.join("session_1");
        let _ = fs::remove_dir_all(&temp_dir);
        fs::create_dir_all(&session_dir).unwrap();

        // 3 个 2 秒的分片，总时长应为 6 秒
        let seg1 = session_dir.join("test_seg000001.ts");
        let seg2 = session_dir.join("test_seg000002.ts");
        let seg3 = session_dir.join("test_seg000003.ts");
        generate_test_ts(&seg1, 2);
        generate_test_ts(&seg2, 2);
        generate_test_ts(&seg3, 2);

        append_to_m3u8(&session_dir, &seg1, 2.0, false);
        append_to_m3u8(&session_dir, &seg2, 2.0, false);
        append_to_m3u8(&session_dir, &seg3, 2.0, false);

        let output_video = temp_dir.join("final.mp4");
        let emitter: Arc<dyn Emitter> = Arc::new(NoopEmitter);
        let settings = Settings::default();
        let session_dir_str = session_dir.to_string_lossy().to_string();

        let duration = merge_segments(
            &session_dir,
            &output_video,
            "test_user",
            &emitter,
            &session_dir_str,
            &settings,
        );

        assert_eq!(duration, Some(6));
        assert!(output_video.exists());
        let probed = get_video_duration(&output_video);
        assert_eq!(probed, Some(6));

        let _ = fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_merge_segments_with_discontinuity_reconnect() {
        let temp_dir = std::env::temp_dir().join(format!("rec_test_discont_{}", std::process::id()));
        let session_dir = temp_dir.join("session_2");
        let _ = fs::remove_dir_all(&temp_dir);
        fs::create_dir_all(&session_dir).unwrap();

        // 模拟推流重连：前两片序号正常，第三片发生序号重置（标记 discontinuity），共 4 片各 2 秒
        let seg1 = session_dir.join("test_seg000001.ts");
        let seg2 = session_dir.join("test_seg000002.ts");
        let seg3 = session_dir.join("test_seg000003.ts");
        let seg4 = session_dir.join("test_seg000004.ts");
        generate_test_ts(&seg1, 2);
        generate_test_ts(&seg2, 2);
        generate_test_ts(&seg3, 2);
        generate_test_ts(&seg4, 2);

        append_to_m3u8(&session_dir, &seg1, 2.0, false);
        append_to_m3u8(&session_dir, &seg2, 2.0, false);
        append_to_m3u8(&session_dir, &seg3, 2.0, true); // 重连发生不连续
        append_to_m3u8(&session_dir, &seg4, 2.0, false);

        let output_video = temp_dir.join("final_reconnect.mp4");
        let emitter: Arc<dyn Emitter> = Arc::new(NoopEmitter);
        let settings = Settings::default();
        let session_dir_str = session_dir.to_string_lossy().to_string();

        let duration = merge_segments(
            &session_dir,
            &output_video,
            "test_user",
            &emitter,
            &session_dir_str,
            &settings,
        );

        assert_eq!(duration, Some(8));
        assert!(output_video.exists());
        let probed = get_video_duration(&output_video);
        assert_eq!(probed, Some(8));

        let _ = fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_merge_segments_fallback_when_m3u8_corrupted() {
        let temp_dir = std::env::temp_dir().join(format!("rec_test_fallback_{}", std::process::id()));
        let session_dir = temp_dir.join("session_3");
        let _ = fs::remove_dir_all(&temp_dir);
        fs::create_dir_all(&session_dir).unwrap();

        // 损坏的 playlist.m3u8（无分片行），仅存放 .ts 分片
        fs::write(session_dir.join("playlist.m3u8"), b"#EXTM3U\n#EXT-X-VERSION:3\n").unwrap();
        let seg1 = session_dir.join("test_seg000001.ts");
        let seg2 = session_dir.join("test_seg000002.ts");
        generate_test_ts(&seg1, 2);
        generate_test_ts(&seg2, 2);

        let output_video = temp_dir.join("final_fallback.mp4");
        let emitter: Arc<dyn Emitter> = Arc::new(NoopEmitter);
        let settings = Settings::default();
        let session_dir_str = session_dir.to_string_lossy().to_string();

        let duration = merge_segments(
            &session_dir,
            &output_video,
            "test_user",
            &emitter,
            &session_dir_str,
            &settings,
        );

        assert_eq!(duration, Some(4));
        assert!(output_video.exists());
        let probed = get_video_duration(&output_video);
        assert_eq!(probed, Some(4));

        let _ = fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_merge_segments_ignores_temporary_ts_files() {
        let temp_dir = std::env::temp_dir().join(format!("rec_test_tmp_{}", std::process::id()));
        let session_dir = temp_dir.join("session_tmp");
        let _ = fs::remove_dir_all(&temp_dir);
        fs::create_dir_all(&session_dir).unwrap();

        let seg1 = session_dir.join("test_seg000001.ts");
        let seg2 = session_dir.join("test_seg000002.ts");
        generate_test_ts(&seg1, 2);
        generate_test_ts(&seg2, 2);
        append_to_m3u8(&session_dir, &seg1, 2.0, false);
        append_to_m3u8(&session_dir, &seg2, 2.0, false);

        // Simulate an in-flight conversion that was interrupted: the temp file
        // must never be included by the merge.
        fs::write(session_dir.join("test_seg000003.ts.tmp"), b"partial").unwrap();

        let output_video = temp_dir.join("final_tmp.mp4");
        let emitter: Arc<dyn Emitter> = Arc::new(NoopEmitter);
        let settings = Settings::default();
        let session_dir_str = session_dir.to_string_lossy().to_string();

        let duration = merge_segments(
            &session_dir,
            &output_video,
            "test_user",
            &emitter,
            &session_dir_str,
            &settings,
        );

        assert_eq!(duration, Some(4));
        assert_eq!(get_video_duration(&output_video), Some(4));
        let _ = fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_sliding_window_deduplication_no_false_resets() {
        use crate::recording::hls::HlsSegment;
        let mut downloaded_sequences: HashSet<u32> = HashSet::new();
        let mut downloaded_urls: HashSet<String> = HashSet::new();
        let mut last_sequence: Option<u32> = None;

        let simulate_round = |segments: Vec<HlsSegment>,
                              downloaded_sequences: &mut HashSet<u32>,
                              downloaded_urls: &mut HashSet<String>,
                              last_sequence: &mut Option<u32>|
         -> Vec<u32> {
            let mut pending_sequences = Vec::new();
            for segment in segments {
                let clean_url = segment.url.split(['?', '#']).next().unwrap_or(&segment.url);
                let is_duplicate = downloaded_sequences.contains(&segment.sequence)
                    || downloaded_urls.contains(clean_url)
                    || downloaded_urls.contains(&segment.url);

                if is_duplicate {
                    continue;
                }

                if let Some(prev) = *last_sequence {
                    if prev > 50 && (segment.sequence <= 10 || prev.saturating_sub(segment.sequence) > 50) {
                        downloaded_sequences.clear();
                        downloaded_urls.clear();
                        *last_sequence = None;
                    }
                }

                pending_sequences.push(segment.sequence);
                *last_sequence = Some(segment.sequence);
                downloaded_sequences.insert(segment.sequence);
                downloaded_urls.insert(clean_url.to_string());
                downloaded_urls.insert(segment.url.clone());
            }
            pending_sequences
        };

        let seg = |seq: u32| HlsSegment {
            url: format!("https://cdn.example.com/hls/model_{}.mp4", seq),
            sequence: seq,
            duration_secs: 2.0,
        };

        // Round 1: Live window [100, 101, 102]
        let r1 = simulate_round(
            vec![seg(100), seg(101), seg(102)],
            &mut downloaded_sequences,
            &mut downloaded_urls,
            &mut last_sequence,
        );
        assert_eq!(r1, vec![100, 101, 102]);
        assert_eq!(last_sequence, Some(102));

        // Round 2: Sliding window advances by 1 [101, 102, 103]
        // 101 and 102 MUST be skipped as duplicates without triggering false reset!
        let r2 = simulate_round(
            vec![seg(101), seg(102), seg(103)],
            &mut downloaded_sequences,
            &mut downloaded_urls,
            &mut last_sequence,
        );
        assert_eq!(r2, vec![103]);
        assert_eq!(last_sequence, Some(103));

        // Round 3: Sliding window advances by 1 [102, 103, 104]
        let r3 = simulate_round(
            vec![seg(102), seg(103), seg(104)],
            &mut downloaded_sequences,
            &mut downloaded_urls,
            &mut last_sequence,
        );
        assert_eq!(r3, vec![104]);
        assert_eq!(last_sequence, Some(104));

        // Round 4: Genuine stream restart from sequence 1 [1, 2, 3]
        let r4 = simulate_round(
            vec![seg(1), seg(2), seg(3)],
            &mut downloaded_sequences,
            &mut downloaded_urls,
            &mut last_sequence,
        );
        assert_eq!(r4, vec![1, 2, 3]);
        assert_eq!(last_sequence, Some(3));
    }
}
