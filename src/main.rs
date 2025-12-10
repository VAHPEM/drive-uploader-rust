#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

use reqwest::blocking::{Client, multipart};
use reqwest::StatusCode;
use serde::Deserialize;
use serde_json::json;
use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{mpsc::channel, Arc, Mutex};
use std::thread;
use dirs;

const MAX_FILE_SIZE: u64 = 1_000_000_000; // 1 GB
const DRIVE_ROOT_NAME: &str = "ImportantFiles";
const MAX_THREADS: usize = 8; // số worker thread

#[derive(Clone)]
struct OAuthConfig {
    client_id: String,
    client_secret: String,
    refresh_token: String,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
}

// Mỗi job = (đường dẫn file full, id folder cha trên Drive)
type Job = (PathBuf, String);

fn main() -> Result<(), Box<dyn Error>> {
    let oauth = OAuthConfig {
        client_id: "Your client ID".into(),
        client_secret: "Your client secret".into(),
        refresh_token: "Your refresh token".into(),
    };

    let local_root = dirs::document_dir().ok_or("Could not find Documents folder")?;

    // HTTP client + access token chia sẻ giữa các thread
    let client = Arc::new(Client::new());
    let token = Arc::new(Mutex::new(get_token(&client, &oauth)?));

    // 1. Tạo folder gốc ImportantFiles
    let drive_root_id =
        create_drive_folder(&client, &oauth, &token, DRIVE_ROOT_NAME, None)?;

    // 2. Tạo channel cho job upload file
    let (tx, rx) = channel::<Job>();
    let rx = Arc::new(Mutex::new(rx));

    // 3. Spawn worker threads
    for _ in 0..MAX_THREADS {
        let rx = Arc::clone(&rx);
        let client = Arc::clone(&client);
        let oauth = oauth.clone();
        let token = Arc::clone(&token);

        thread::spawn(move || loop {
            // Lấy job từ channel (blocking)
            let msg = {
                let guard = rx.lock().unwrap();
                guard.recv()
            };

            let (file_path, parent_id) = match msg {
                Ok(job) => job,
                Err(_) => break, // channel đóng, thoát thread
            };

            if let Err(e) = upload_file(&client, &oauth, &token, &parent_id, &file_path) {
                eprintln!("Failed to upload {}: {}", file_path.display(), e);
            }
        });
    }

    // 4. Đệ quy duyệt thư mục, tạo folder trên Drive và đẩy file vào queue
    upload_folder_recursive(
        &client,
        &oauth,
        &token,
        &local_root,
        &drive_root_id,
        &tx,
    )?;

    // 5. Đóng đầu gửi để worker biết là hết việc
    drop(tx);

    // đợi cho thread upload xong (thô sơ nhưng đủ dùng)
    thread::sleep(std::time::Duration::from_secs(5));

    Ok(())
}

/// Lấy access token từ refresh token
fn get_token(client: &Client, oauth: &OAuthConfig) -> Result<String, Box<dyn Error>> {
    let resp = client
        .post("https://oauth2.googleapis.com/token")
        .form(&[
            ("client_id", oauth.client_id.as_str()),
            ("client_secret", oauth.client_secret.as_str()),
            ("refresh_token", oauth.refresh_token.as_str()),
            ("grant_type", "refresh_token"),
        ])
        .send()?;

    let status = resp.status();
    let body = resp.text()?;

    if !status.is_success() {
        eprintln!("Token request failed: {}", status);
        eprintln!("Body: {}", body);
        return Err("token error".into());
    }

    let tok: TokenResponse = serde_json::from_str(&body)?;
    Ok(tok.access_token)
}

/// Tạo folder trên Drive, trả về id
fn create_drive_folder(
    client: &Client,
    oauth: &OAuthConfig,
    access_token: &Arc<Mutex<String>>,
    name: &str,
    parent_id: Option<&str>,
) -> Result<String, Box<dyn Error>> {
    let mut metadata = json!({
        "name": name,
        "mimeType": "application/vnd.google-apps.folder",
    });

    if let Some(p) = parent_id {
        metadata["parents"] = json!([p]);
    }

    let tk = { access_token.lock().unwrap().clone() };

    let resp = client
        .post("https://www.googleapis.com/drive/v3/files")
        .bearer_auth(&tk)
        .json(&metadata)
        .send()?;

    if resp.status() == StatusCode::UNAUTHORIZED {
        let new = get_token(client, oauth)?;
        *access_token.lock().unwrap() = new;
        return Err("token expired while creating folder".into());
    }

    let v: serde_json::Value = resp.error_for_status()?.json()?;
    let id = v["id"]
        .as_str()
        .ok_or("Folder created but no id in response")?
        .to_string();

    Ok(id)
}

/// Đệ quy: tạo folder con trên Drive + gửi file vào hàng đợi
fn upload_folder_recursive(
    client: &Client,
    oauth: &OAuthConfig,
    access_token: &Arc<Mutex<String>>,
    local_dir: &Path,
    drive_parent_id: &str,
    tx: &std::sync::mpsc::Sender<Job>,
) -> Result<(), Box<dyn Error>> {
    if !local_dir.is_dir() {
        return Err(format!("{} is not a directory", local_dir.display()).into());
    }

    for entry in fs::read_dir(local_dir)? {
        let entry = entry?;
        let path = entry.path();

        if path.is_dir() {
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("folder");

            let drive_id =
                create_drive_folder(client, oauth, access_token, name, Some(drive_parent_id))?;

            // đệ quy folder con
            if let Err(e) =
                upload_folder_recursive(client, oauth, access_token, &path, &drive_id, tx)
            {
                eprintln!("Failed to walk folder {}: {}", path.display(), e);
            }
        } else {
            // file: kiểm tra size + push job
            let meta = match fs::metadata(&path) {
                Ok(m) => m,
                Err(e) => {
                    eprintln!("Skip file {}: can't read metadata ({})", path.display(), e);
                    continue;
                }
            };

            if meta.len() > MAX_FILE_SIZE {
                eprintln!("Skip file {}: >1GB", path.display());
                continue;
            }

            // Gửi job: (full_path, drive_parent_id)
            if let Err(e) = tx.send((path.clone(), drive_parent_id.to_string())) {
                eprintln!("Failed to enqueue job for {}: {}", path.display(), e);
            }
        }
    }

    Ok(())
}

/// Upload 1 file vào folder Drive parent_id
fn upload_file(
    client: &Client,
    oauth: &OAuthConfig,
    access_token: &Arc<Mutex<String>>,
    parent_id: &str,
    file_path: &Path,
) -> Result<(), Box<dyn Error>> {
    let file_name = file_path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or("Invalid file name")?;

    let metadata = json!({
        "name": file_name,
        "parents": [parent_id],
    });

    let meta_part =
        multipart::Part::text(metadata.to_string()).mime_str("application/json")?;

    let file_part = match multipart::Part::file(file_path) {
        Ok(p) => p.mime_str("application/octet-stream")?,
        Err(e) => {
            // iCloud file chưa tải, file vừa bị xóa, v.v.
            return Err(format!("cannot open file: {}", e).into());
        }
    };

    let form = multipart::Form::new()
        .part("metadata", meta_part)
        .part("file", file_part);

    let tk = { access_token.lock().unwrap().clone() };

    let resp = client
        .post("https://www.googleapis.com/upload/drive/v3/files?uploadType=multipart")
        .bearer_auth(&tk)
        .multipart(form)
        .send()?;

    let status = resp.status();

    if status == StatusCode::UNAUTHORIZED {
        let new = get_token(client, oauth)?;
        *access_token.lock().unwrap() = new;
        return Err("token expired while uploading file".into());
    }

    if !status.is_success() {
        let body = resp.text()?;
        return Err(format!("upload failed: {} - {}", status, body).into());
    }

    Ok(())
}