use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{exit, Command, Stdio};

use serde::Deserialize;
use serde_json::Value;

#[derive(Deserialize)]
struct Config {
    screenshot: ScreenshotConfig,
    upload: UploadConfig,
    filename: FilenameConfig,
    clipboard: ClipboardConfig,
    notification: NotificationConfig,
    cleanup: CleanupConfig,
}

#[derive(Deserialize)]
struct ScreenshotConfig {
    tool: String,
    args: Vec<String>,
    output: String,
    temp_dir: String,
    allowed_mime_types: Vec<String>,
}

#[derive(Deserialize)]
struct UploadConfig {
    url: String,
    filename_param: String,
    content_type: String,
    response_url_path: String,
    response_error_path: String,
}

#[derive(Deserialize)]
struct FilenameConfig {
    extension: String,
    random_bytes: usize,
}

#[derive(Deserialize)]
struct ClipboardConfig {
    tool: String,
    args: Vec<String>,
    use_stdin: bool,
}

#[derive(Deserialize)]
struct NotificationConfig {
    tool: String,
    message: String,
    args: Vec<String>,
    include_screenshot_as_icon: bool,
}

#[derive(Deserialize)]
struct CleanupConfig {
    delete_temp_file: bool,
}

fn load_config() -> Config {
    let home = std::env::var("HOME").unwrap_or_else(|_| {
        eprintln!("Error: $HOME is not set");
        exit(1);
    });
    let path = Path::new(&home).join(".config/zenbuser/zenbuser.toml");
    let contents = fs::read_to_string(&path).unwrap_or_else(|_| {
        eprintln!("Error: Could not read config at {}", path.display());
        exit(1);
    });
    toml::from_str(&contents).unwrap_or_else(|e| {
        eprintln!("Error: Failed to parse config: {}", e);
        exit(1);
    })
}

fn random_filename(cfg: &FilenameConfig) -> String {
    let mut buf = vec![0u8; cfg.random_bytes];
    fs::File::open("/dev/urandom")
        .and_then(|mut f| { f.read_exact(&mut buf) })
        .expect("Failed to read /dev/urandom");
    let hex: String = buf.iter().map(|b| format!("{:02x}", b)).collect();
    format!("{}.{}", hex, cfg.extension)
}

fn capture_screenshot(cfg: &ScreenshotConfig, temp_file: &Path) {
    match cfg.output.as_str() {
        "stdout" => {
            let out = Command::new(&cfg.tool)
                .args(&cfg.args)
                .output()
                .unwrap_or_else(|_| {
                    eprintln!("Error: Failed to run {}", cfg.tool);
                    exit(1);
                });
            if out.stdout.is_empty() {
                eprintln!("Error: Screenshot not created!");
                exit(1);
            }
            fs::write(temp_file, &out.stdout).expect("Failed to write screenshot");
        }
        "file" => {
            let status = Command::new(&cfg.tool)
                .args(&cfg.args)
                .arg(temp_file)
                .status()
                .unwrap_or_else(|_| {
                    eprintln!("Error: Failed to run {}", cfg.tool);
                    exit(1);
                });
            if !status.success() || !temp_file.exists() {
                eprintln!("Error: Screenshot not created!");
                exit(1);
            }
        }
        other => {
            eprintln!("Error: Unknown screenshot output mode '{}'", other);
            exit(1);
        }
    }

    let out = Command::new("file")
        .args(["--mime-type", "-b"])
        .arg(temp_file)
        .output()
        .expect("Failed to run file");
    let mime = String::from_utf8(out.stdout).unwrap().trim().to_string();

    if !cfg.allowed_mime_types.iter().any(|m| m == &mime) {
        fs::remove_file(temp_file).ok();
        eprintln!(
            "Error: MIME type '{}' not allowed. Allowed: {:?}",
            mime, cfg.allowed_mime_types
        );
        exit(1);
    }
}

fn upload_screenshot(cfg: &UploadConfig, filename_cfg: &FilenameConfig, temp_file: &Path) -> String {
    let upload_name = random_filename(filename_cfg);
    let url = format!("{}?{}={}", cfg.url, cfg.filename_param, upload_name);
    let out = Command::new("curl")
        .args([
            "-s",
            "-H", &format!("Content-Type: {}", cfg.content_type),
            "--data-binary", &format!("@{}", temp_file.display()),
            &url,
        ])
        .output()
        .expect("Failed to run curl");
    String::from_utf8(out.stdout).unwrap()
}

fn extract_url(cfg: &UploadConfig, response: &str) -> String {
    let json: Value = serde_json::from_str(response).unwrap_or_else(|_| {
        eprintln!("Error: Failed to parse upload response JSON");
        exit(1);
    });

    let get = |path: &str| -> Option<&Value> {
        path.split('.').fold(Some(&json), |acc, key| acc?.get(key))
    };

    if let Some(url) = get(&cfg.response_url_path).and_then(|v| v.as_str()) {
        return url.to_string();
    }

    let error = get(&cfg.response_error_path)
        .and_then(|v| v.as_str())
        .unwrap_or("Unknown error");
    eprintln!("Error: {}", error);
    exit(1);
}

fn copy_to_clipboard(cfg: &ClipboardConfig, text: &str) {
    if cfg.use_stdin {
        let mut child = Command::new(&cfg.tool)
            .args(&cfg.args)
            .stdin(Stdio::piped())
            .spawn()
            .unwrap_or_else(|_| {
                eprintln!("Error: Failed to run {}", cfg.tool);
                exit(1);
            });
        child.stdin.as_mut().unwrap().write_all(text.as_bytes()).unwrap();
        child.wait().unwrap();
    } else {
        Command::new(&cfg.tool)
            .args(&cfg.args)
            .arg(text)
            .status()
            .unwrap_or_else(|_| {
                eprintln!("Error: Failed to run {}", cfg.tool);
                exit(1);
            });
    }
}

fn send_notification(cfg: &NotificationConfig, temp_file: &Path) {
    let mut cmd = Command::new(&cfg.tool);
    cmd.arg(&cfg.message);
    cmd.args(&cfg.args);
    if cfg.include_screenshot_as_icon {
        cmd.args(["-i", &temp_file.display().to_string()]);
    }
    cmd.status().ok();
}

fn main() {
    let cfg = load_config();

    let temp_filename = random_filename(&cfg.filename);
    let temp_file = PathBuf::from(&cfg.screenshot.temp_dir).join(&temp_filename);

    capture_screenshot(&cfg.screenshot, &temp_file);

    let response = upload_screenshot(&cfg.upload, &cfg.filename, &temp_file);
    let url = extract_url(&cfg.upload, &response);

    copy_to_clipboard(&cfg.clipboard, &url);
    send_notification(&cfg.notification, &temp_file);

    if cfg.cleanup.delete_temp_file {
        fs::remove_file(&temp_file).ok();
    }
}
