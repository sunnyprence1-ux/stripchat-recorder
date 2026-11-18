use pp_utils::{
    emit_progress, extract_video_frame, format_bytes, format_speed, param, param_bool, parse_stem,
};
use reqwest::blocking::{Client, Response};
use serde_json::{json, Value};
use std::env;
use std::fs::{self, File};
use std::io::{BufRead, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const DESCRIBE: &str = r#"{
    "id": "upload_gdrive",
    "name": "Google Drive Upload 0.1.0",
    "description": "Upload finished videos to Google Drive, one folder per model",
    "loginable": true,
    "testable": true,
    "params": [
        {
            "key": "client_id",
            "label": "Google OAuth Client ID",
            "type": "string",
            "default": ""
        },
        {
            "key": "client_secret",
            "label": "Google OAuth Client Secret",
            "type": "string",
            "default": ""
        },
        {
            "key": "root_folder",
            "label": "Root folder name in Google Drive",
            "type": "string",
            "default": "Stripchat"
        },
        {
            "key": "upload_cover",
            "label": "Also upload the cover image if one exists",
            "type": "boolean",
            "default": "true"
        },
        {
            "key": "delete_after_send",
            "label": "Delete the local video after a successful upload",
            "type": "boolean",
            "default": "false"
        }
    ]
}"#;

const DRIVE_API: &str = "https://www.googleapis.com/drive/v3";
const DRIVE_UPLOAD_API: &str = "https://www.googleapis.com/upload/drive/v3";
const OAUTH_DEVICE_URL: &str = "https://oauth2.googleapis.com/device/code";
const OAUTH_TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const SCOPE: &str = "https://www.googleapis.com/auth/drive.file";
const CHUNK_SIZE: usize = 8 * 1024 * 1024;

fn http_client() -> Client {
    Client::builder()
        .timeout(Duration::from_secs(600))
        .build()
        .map_err(|e| format!("HTTP client error: {}", e))
        .unwrap_or_else(|_| Client::new())
}

fn app_root() -> PathBuf {
    let candidate = env::var("PP_EXE_DIR")
        .ok()
        .map(PathBuf::from)
        .or_else(|| {
            env::current_exe()
                .ok()
                .and_then(|p| p.parent().map(|p| p.to_path_buf()))
        })
        .unwrap_or_else(|| PathBuf::from("."));
    if candidate
        .file_name()
        .map(|name| name == "modules")
        .unwrap_or(false)
    {
        candidate
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or(candidate)
    } else {
        candidate
    }
}

fn token_path() -> PathBuf {
    if let Ok(dir) = env::var("PP_GDRIVE_TOKEN_DIR") {
        return PathBuf::from(dir).join("oauth.json");
    }
    app_root().join("config/upload_gdrive/oauth.json")
}

fn read_token() -> Result<Value, String> {
    let path = token_path();
    if !path.exists() {
        return Err("Google Drive is not signed in yet".to_string());
    }
    let content = fs::read_to_string(&path).map_err(|e| format!("read token file failed: {}", e))?;
    serde_json::from_str(&content).map_err(|e| format!("invalid token file: {}", e))
}

fn write_token(value: &Value) -> Result<(), String> {
    let path = token_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("create token dir failed: {}", e))?;
    }
    let content = serde_json::to_string_pretty(value).map_err(|e| format!("serialize token failed: {}", e))?;
    fs::write(&path, content + "\n").map_err(|e| format!("write token file failed: {}", e))
}

fn json_text(resp: Response) -> Result<(u16, Value), String> {
    let status = resp.status().as_u16();
    let text = resp
        .text()
        .map_err(|e| format!("HTTP body read failed: {}", e))?;
    let value = if text.trim().is_empty() {
        Value::Null
    } else {
        serde_json::from_str(&text).map_err(|e| format!("HTTP JSON parse failed: {} ({})", e, text))?
    };
    Ok((status, value))
}

fn drive_account_name(client: &Client, token: &str) -> String {
    let url = format!("{}/about?fields=user", DRIVE_API);
    if let Ok(resp) = client
        .get(&url)
        .header("Authorization", format!("Bearer {}", token))
        .send()
    {
        if let Ok((200, body)) = json_text(resp) {
            if let Some(email) = body.pointer("/user/emailAddress").and_then(|v| v.as_str()) {
                return email.to_string();
            }
            if let Some(name) = body.pointer("/user/displayName").and_then(|v| v.as_str()) {
                return name.to_string();
            }
        }
    }
    "Google Drive".to_string()
}

fn login_state(state: &str) {
    println!("LOGIN_STATE:{}", state);
    let _ = std::io::stdout().flush();
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn request_device_code(client: &Client, client_id: &str) -> Result<Value, String> {
    let resp = client
        .post(OAUTH_DEVICE_URL)
        .form(&[("client_id", client_id), ("scope", SCOPE)])
        .send()
        .map_err(|e| format!("device code request failed: {}", e))?;
    let (status, body) = json_text(resp)?;
    if status != 200 {
        return Err(format!("device code request failed (HTTP {}): {:?}", status, body));
    }
    Ok(body)
}

fn poll_device_token(
    client: &Client,
    client_id: &str,
    client_secret: &str,
    device_code: &str,
) -> Result<Value, String> {
    let resp = client
        .post(OAUTH_TOKEN_URL)
        .form(&[
            ("client_id", client_id),
            ("client_secret", client_secret),
            ("device_code", device_code),
            ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
        ])
        .send()
        .map_err(|e| format!("token poll failed: {}", e))?;
    let (status, body) = json_text(resp)?;
    if status == 200 {
        return Ok(body);
    }
    let code = body.get("error").and_then(|v| v.as_str()).unwrap_or("");
    if code == "authorization_pending" || code == "slow_down" {
        return Err("pending".to_string());
    }
    Err(format!("token poll failed (HTTP {}): {:?}", status, body))
}

fn run_login() -> Result<(), String> {
    let client = http_client();
    let force = env::var("PP_FORCE_LOGIN").map(|v| v == "1" || v == "true").unwrap_or(false)
        || env::args().any(|a| a == "--fresh" || a == "--force");

    let path = token_path();
    if path.exists() && !force {
        if let Ok(token) = read_token() {
            let email = token
                .get("email")
                .and_then(|v| v.as_str())
                .unwrap_or("Google Drive");
            login_state(&format!("already:{}", email));
            return Ok(());
        }
    }
    if force {
        let _ = fs::remove_file(&path);
    }

    let client_id = param("client_id", "");
    let client_secret = param("client_secret", "");
    if client_id.trim().is_empty() {
        login_state("error:Google OAuth Client ID is required");
        return Err("client_id is required".to_string());
    }
    if client_secret.trim().is_empty() {
        login_state("error:Google OAuth Client Secret is required");
        return Err("client_secret is required".to_string());
    }

    let device = request_device_code(&client, &client_id)?;
    let verification_url = device
        .get("verification_url")
        .or_else(|| device.get("verification_uri"))
        .and_then(|v| v.as_str())
        .unwrap_or("https://www.google.com/device")
        .to_string();
    let user_code = device
        .get("user_code")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let device_code = device
        .get("device_code")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if user_code.is_empty() || device_code.is_empty() {
        login_state("error:device code response is incomplete");
        return Err("device code response is incomplete".to_string());
    }

    login_state(&format!("device:{}|{}", verification_url, user_code));

    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let line = line.trim();
        if line == "quit" {
            login_state("cancelled");
            return Ok(());
        }
        if line.is_empty() {
            continue;
        }
        match poll_device_token(&client, &client_id, &client_secret, &device_code) {
            Ok(mut token) => {
                if token.get("refresh_token").is_none() {
                    login_state("error:Google did not return a refresh token. Remove app access in your Google Account and try again.");
                    return Err("refresh token missing".to_string());
                }
                let access_token = token
                    .get("access_token")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let email = drive_account_name(&client, &access_token);
                token["email"] = json!(email);
                token["client_id"] = json!(client_id);
                token["client_secret"] = json!(client_secret);
                token["created_at"] = json!(now_secs());
                write_token(&token)?;
                login_state(&format!("done:{}", email));
                return Ok(());
            }
            Err(err) if err == "pending" => {
                login_state(&format!("device:{}|{}", verification_url, user_code));
            }
            Err(err) => {
                login_state(&format!("error:{}", err));
                return Err(err);
            }
        }
    }
    Err("stdin closed before login completed".to_string())
}

fn refresh_access_token(client: &Client, token: &mut Value) -> Result<(), String> {
    let refresh_token = token
        .get("refresh_token")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if refresh_token.is_empty() {
        return Err("stored token has no refresh_token".to_string());
    }
    let client_id = token
        .get("client_id")
        .and_then(|v| v.as_str())
        .map(|v| v.to_string())
        .unwrap_or_else(|| param("client_id", ""));
    let client_secret = token
        .get("client_secret")
        .and_then(|v| v.as_str())
        .map(|v| v.to_string())
        .unwrap_or_else(|| param("client_secret", ""));
    if client_id.is_empty() || client_secret.is_empty() {
        return Err("client_id/client_secret are required to refresh the token".to_string());
    }

    let resp = client
        .post(OAUTH_TOKEN_URL)
        .form(&[
            ("client_id", client_id.as_str()),
            ("client_secret", client_secret.as_str()),
            ("refresh_token", refresh_token.as_str()),
            ("grant_type", "refresh_token"),
        ])
        .send()
        .map_err(|e| format!("token refresh failed: {}", e))?;
    let (status, body) = json_text(resp)?;
    if status != 200 {
        return Err(format!("token refresh failed (HTTP {}): {:?}", status, body));
    }
    let access_token = body
        .get("access_token")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let expires_in = body.get("expires_in").and_then(|v| v.as_u64()).unwrap_or(3600);
    token["access_token"] = json!(access_token);
    token["expires_in"] = json!(expires_in);
    token["created_at"] = json!(now_secs());
    write_token(token)
}

fn access_token(client: &Client) -> Result<String, String> {
    let mut token = read_token()?;
    let now = now_secs();
    let created = token.get("created_at").and_then(|v| v.as_u64()).unwrap_or(0);
    let expires_in = token.get("expires_in").and_then(|v| v.as_u64()).unwrap_or(3600);
    let access = token
        .get("access_token")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if access.is_empty() || created + expires_in.saturating_sub(60) <= now {
        refresh_access_token(client, &mut token)?;
        return Ok(token
            .get("access_token")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string());
    }
    Ok(access)
}

fn folder_query(name: &str, parent: Option<&str>) -> String {
    let escaped = name.replace('\'', "\\'");
    let mut q = format!(
        "name = '{}' and mimeType = 'application/vnd.google-apps.folder' and trashed = false",
        escaped
    );
    if let Some(parent) = parent {
        q.push_str(&format!(" and '{}' in parents", parent.replace('\'', "\\'")));
    }
    q
}

fn ensure_folder(client: &Client, token: &str, name: &str, parent: Option<&str>) -> Result<String, String> {
    let url = format!("{}/files", DRIVE_API);
    let query = folder_query(name, parent);
    let resp = client
        .get(&url)
        .header("Authorization", format!("Bearer {}", token))
        .query(&[("q", query.as_str()), ("fields", "files(id,name)")])
        .send()
        .map_err(|e| format!("folder lookup failed: {}", e))?;
    let (status, body) = json_text(resp)?;
    if status == 200 {
        if let Some(items) = body.get("files").and_then(|v| v.as_array()) {
            if let Some(item) = items.iter().find_map(|v| v.get("id").and_then(|id| id.as_str())) {
                return Ok(item.to_string());
            }
        }
    }
    if status != 200 {
        return Err(format!("folder lookup failed (HTTP {}): {:?}", status, body));
    }

    let mut body = json!({"name": name, "mimeType": "application/vnd.google-apps.folder"});
    if let Some(parent) = parent {
        body["parents"] = json!([parent]);
    }
    let resp = client
        .post(&url)
        .header("Authorization", format!("Bearer {}", token))
        .json(&body)
        .send()
        .map_err(|e| format!("folder create failed: {}", e))?;
    let (status, body) = json_text(resp)?;
    if status != 200 {
        return Err(format!("folder create failed (HTTP {}): {:?}", status, body));
    }
    body.get("id")
        .and_then(|v| v.as_str())
        .map(|v| v.to_string())
        .ok_or_else(|| "folder create returned no id".to_string())
}

fn mime_type(path: &Path) -> String {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase()
        .as_str()
    {
        "mp4" => "video/mp4",
        "mkv" => "video/x-matroska",
        "webm" => "video/webm",
        "ts" => "video/mp2t",
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "webp" => "image/webp",
        _ => "application/octet-stream",
    }
    .to_string()
}

fn emit_upload_progress(done: u64, total: u64) {
    let scaled = if total == 0 {
        0
    } else {
        ((done as u128 * 10_000) / total as u128).min(10_000) as u32
    };
    emit_progress(scaled, 10_000);
}

fn file_exists(client: &Client, token: &str, name: &str, parent: &str) -> Result<Option<String>, String> {
    let escaped = name.replace('\'', "\\'");
    let parent_escaped = parent.replace('\'', "\\'");
    let q = format!(
        "name = '{}' and trashed = false and '{}' in parents",
        escaped, parent_escaped
    );
    let url = format!("{}/files", DRIVE_API);
    let resp = client
        .get(&url)
        .header("Authorization", format!("Bearer {}", token))
        .query(&[("q", q.as_str()), ("fields", "files(id,name)")])
        .send()
        .map_err(|e| format!("file lookup failed: {}", e))?;
    let (status, body) = json_text(resp)?;
    if status != 200 {
        return Err(format!("file lookup failed (HTTP {}): {:?}", status, body));
    }
    Ok(body
        .get("files")
        .and_then(|v| v.as_array())
        .and_then(|items| items.first())
        .and_then(|item| item.get("id").and_then(|id| id.as_str()))
        .map(|id| id.to_string()))
}

fn start_resumable_upload(
    client: &Client,
    token: &str,
    name: &str,
    folder_id: &str,
    mime: &str,
    size: u64,
) -> Result<String, String> {
    let url = format!("{}/files?uploadType=resumable&fields=id,name,size,webViewLink", DRIVE_UPLOAD_API);
    let body = json!({"name": name, "parents": [folder_id], "mimeType": mime});
    let resp = client
        .post(&url)
        .header("Authorization", format!("Bearer {}", token))
        .header("Content-Type", "application/json; charset=UTF-8")
        .header("X-Upload-Content-Type", mime)
        .header("X-Upload-Content-Length", size.to_string())
        .json(&body)
        .send()
        .map_err(|e| format!("upload session failed: {}", e))?;
    let status = resp.status().as_u16();
    if status != 200 {
        let (_, body) = json_text(resp)?;
        return Err(format!("upload session failed (HTTP {}): {:?}", status, body));
    }
    resp.headers()
        .get("location")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.to_string())
        .ok_or_else(|| "upload session returned no location".to_string())
}

fn upload_bytes(
    client: &Client,
    location: &str,
    token: &str,
    data: Vec<u8>,
    start: u64,
    total: u64,
) -> Result<Option<(u16, Value)>, String> {
    let end = start + data.len() as u64 - 1;
    let content_range = format!("bytes {}-{}/{}", start, end, total);
    let resp = client
        .put(location)
        .header("Authorization", format!("Bearer {}", token))
        .header("Content-Type", "application/octet-stream")
        .header("Content-Range", content_range)
        .body(data)
        .send()
        .map_err(|e| format!("upload chunk failed: {}", e))?;
    let status = resp.status().as_u16();
    if status == 308 {
        return Ok(None);
    }
    if status >= 400 {
        let (_, body) = json_text(resp)?;
        return Err(format!("upload chunk failed (HTTP {}): {:?}", status, body));
    }
    let (status, body) = json_text(resp)?;
    Ok(Some((status, body)))
}

fn upload_file(client: &Client, token: &str, path: &Path, folder_id: &str) -> Result<Value, String> {
    let name = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("upload.bin")
        .to_string();
    let size = fs::metadata(path).map(|m| m.len()).map_err(|e| format!("metadata failed: {}", e))?;
    if size == 0 {
        return Err(format!("{} is empty", path.display()));
    }
    let existing = file_exists(client, token, &name, folder_id)?;
    if let Some(id) = existing {
        eprintln!("Google Drive: {} already exists, skipping upload", name);
        return Ok(json!({"id": id, "name": name, "skipped": true}));
    }

    let mime = mime_type(path);
    let location = start_resumable_upload(client, token, &name, folder_id, &mime, size)?;
    let mut file = File::open(path).map_err(|e| format!("open input failed: {}", e))?;
    let mut uploaded: u64 = 0;
    let started = Instant::now();
    loop {
        let mut data = vec![0u8; CHUNK_SIZE];
        let mut read_total = 0usize;
        while read_total < data.len() {
            let n = file
                .read(&mut data[read_total..])
                .map_err(|e| format!("read input failed: {}", e))?;
            if n == 0 {
                break;
            }
            read_total += n;
        }
        if read_total == 0 {
            break;
        }
        data.truncate(read_total);
        let start = uploaded;
        match upload_bytes(client, &location, token, data, start, size)? {
            Some((_, body)) => {
                let elapsed = started.elapsed().as_secs_f64();
                let speed = if elapsed > 0.0 { size as f64 / elapsed } else { 0.0 };
                println!("STATUS:{}", format_speed(speed));
                return Ok(body);
            }
            None => {
                uploaded = start + read_total as u64;
                emit_upload_progress(uploaded, size);
            }
        }
    }
    Err("upload ended without a final response".to_string())
}

fn run() -> Result<(), String> {
    let input_str = env::var("PP_INPUT").map_err(|_| "PP_INPUT not set".to_string())?;
    let input = PathBuf::from(&input_str);
    if !input.exists() {
        return Err(format!("Input file not found: {}", input.display()));
    }
    let client = http_client();
    let token = access_token(&client)?;
    let root_folder = {
        let value = param("root_folder", "Stripchat");
        if value.trim().is_empty() {
            "Stripchat".to_string()
        } else {
            value.trim().to_string()
        }
    };
    let root_id = ensure_folder(&client, &token, &root_folder, None)?;
    let stem = input
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("video");
    let (model, _timestamp) = parse_stem(stem);
    let model_folder = if model.trim().is_empty() { "Unknown".to_string() } else { model.trim().to_string() };
    let model_id = ensure_folder(&client, &token, &model_folder, Some(&root_id))?;

    emit_progress(0, 100);

    // 先从视频中截取一帧，并先于视频上传；不再查找 contact sheet。
    // Extract one video frame first and upload it before the video; never use a contact sheet.
    let cover_path = if param_bool("upload_cover", true) {
        match extract_video_frame(&input, "gdrive_cover") {
            Ok(path) => Some(path),
            Err(e) => {
                eprintln!("Google Drive: could not extract cover frame: {}", e);
                None
            }
        }
    } else {
        None
    };
    if let Some(ref cover) = cover_path {
        match upload_file(&client, &token, cover, &model_id) {
            Ok(_) => eprintln!("Google Drive: cover frame uploaded first"),
            Err(e) => eprintln!("Google Drive: cover frame upload failed: {}", e),
        }
    }

    let upload_result = upload_file(&client, &token, &input, &model_id);
    if let Some(cover) = cover_path {
        let _ = fs::remove_file(cover);
    }
    let uploaded = upload_result?;
    let uploaded_name = uploaded
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("file");
    let uploaded_size = uploaded.get("size").and_then(|v| v.as_u64()).unwrap_or(0);
    eprintln!(
        "Google Drive: uploaded {} ({})",
        uploaded_name,
        format_bytes(uploaded_size)
    );
    emit_progress(100, 100);

    println!("OUTPUT:{}", input.display());
    if param_bool("delete_after_send", false) {
        println!("DELETE_INPUT");
    }
    Ok(())
}

fn run_test() -> Result<(), String> {
    let client = http_client();
    let token = access_token(&client)?;
    let root_folder = {
        let value = param("root_folder", "Stripchat");
        if value.trim().is_empty() {
            "Stripchat".to_string()
        } else {
            value.trim().to_string()
        }
    };
    let root_id = ensure_folder(&client, &token, &root_folder, None)?;
    let stored = read_token()?;
    let email = stored.get("email").and_then(|v| v.as_str()).unwrap_or("Google Drive");
    println!("Google Drive: signed in as {}", email);
    println!("Google Drive: root folder {} ({})", root_folder, root_id);
    println!("SELF-TEST PASSED");
    Ok(())
}

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.get(1).map(|s| s.as_str()) == Some("--describe") {
        print!("{}", DESCRIBE);
        return;
    }
    if args.get(1).map(|s| s.as_str()) == Some("--login") {
        if let Err(e) = run_login() {
            eprintln!("{}", e);
            std::process::exit(1);
        }
        return;
    }
    if args.get(1).map(|s| s.as_str()) == Some("--test") || env::var("PP_TEST").map(|v| v == "1").unwrap_or(false) {
        if let Err(e) = run_test() {
            eprintln!("{}", e);
            std::process::exit(1);
        }
        return;
    }
    if let Err(e) = run() {
        eprintln!("{}", e);
        std::process::exit(1);
    }
}
