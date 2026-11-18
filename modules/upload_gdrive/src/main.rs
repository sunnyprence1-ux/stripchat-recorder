use pp_utils::{
    emit_progress, extract_video_frame, format_bytes, format_speed, param, param_bool, parse_stem,
};
use reqwest::blocking::{Client, Response};
use serde_json::{json, Value};
use std::env;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const DESCRIBE: &str = r#"{
    "id": "upload_gdrive",
    "name": "Google Drive Upload 0.2.0",
    "description": "Upload finished videos to Google Drive, one folder per model",
    "loginable": true,
    "testable": true,
    "params": [
        {
            "key": "root_folder",
            "label": "Root folder name in Google Drive",
            "type": "string",
            "default": "Stripchat"
        },
        {
            "key": "upload_cover",
            "label": "Also upload an extracted cover frame before the video",
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

fn gcloud_config_dir() -> PathBuf {
    if let Ok(dir) = env::var("PP_GDRIVE_TOKEN_DIR") {
        return PathBuf::from(dir).join("gcloud");
    }
    app_root().join("config/upload_gdrive/gcloud")
}

fn gcloud_bin() -> PathBuf {
    let bundled = PathBuf::from("/opt/google-cloud-sdk/bin/gcloud");
    if bundled.exists() {
        bundled
    } else {
        PathBuf::from("gcloud")
    }
}

fn gcloud_command() -> Command {
    let mut command = Command::new(gcloud_bin());
    command
        .env("CLOUDSDK_CONFIG", gcloud_config_dir())
        .env("PYTHONUNBUFFERED", "1");
    command
}

fn gcloud_account() -> Option<String> {
    let output = gcloud_command()
        .args([
            "auth",
            "list",
            "--filter=status:ACTIVE",
            "--format=value(account)",
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|line| line.trim())
        .find(|line| !line.is_empty())
        .map(|line| line.to_string())
}

fn gcloud_access_token() -> Result<String, String> {
    let output = gcloud_command()
        .args(["auth", "print-access-token"])
        .output()
        .map_err(|e| format!("could not run gcloud: {}", e))?;
    if !output.status.success() {
        let detail = String::from_utf8_lossy(&output.stderr);
        let message = if detail.trim().is_empty() {
            "Google Drive is not signed in. Click Login in the pipeline.".to_string()
        } else {
            format!(
                "Google Drive is not signed in. Click Login in the pipeline. ({})",
                detail.trim()
            )
        };
        return Err(message);
    }
    let token = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if token.is_empty() {
        return Err("Google Drive is not signed in. Click Login in the pipeline.".to_string());
    }
    Ok(token)
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

fn login_state(state: &str) {
    println!("LOGIN_STATE:{}", state);
    let _ = std::io::stdout().flush();
}

fn run_login() -> Result<(), String> {
    let force = env::var("PP_FORCE_LOGIN")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
        || env::args().any(|a| a == "--fresh" || a == "--force");

    if !force {
        if let Some(email) = gcloud_account() {
            if gcloud_access_token().is_ok() {
                login_state(&format!("already:{}", email));
                return Ok(());
            }
        }
    } else {
        // A fresh login must not reuse an old gcloud config with different scopes.
        let _ = fs::remove_dir_all(gcloud_config_dir());
    }

    let script = app_root().join("gcloud_login.py");
    if !script.exists() {
        login_state("error:gcloud_login.py is missing from the image");
        return Err(format!("missing {}", script.display()));
    }

    let mut command = Command::new("python3");
    command
        .arg(&script)
        .env("PP_EXE_DIR", app_root())
        .env("CLOUDSDK_CONFIG", gcloud_config_dir())
        .env("PYTHONUNBUFFERED", "1")
        .env("PP_FORCE_LOGIN", if force { "1" } else { "0" })
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());

    let status = command
        .status()
        .map_err(|e| format!("could not start Google login: {}", e))?;
    if !status.success() {
        return Err("Google login failed".to_string());
    }
    Ok(())
}

fn access_token() -> Result<String, String> {
    gcloud_access_token()
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
    let token = access_token()?;
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
    let token = access_token()?;
    let root_folder = {
        let value = param("root_folder", "Stripchat");
        if value.trim().is_empty() {
            "Stripchat".to_string()
        } else {
            value.trim().to_string()
        }
    };
    let root_id = ensure_folder(&client, &token, &root_folder, None)?;
    let email = gcloud_account().unwrap_or_else(|| "Google Drive".to_string());
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
