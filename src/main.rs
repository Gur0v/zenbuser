use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail, ensure};
use getrandom::getrandom;
use serde::Deserialize;
use serde_json::Value;
use url::Url;

const VERSION: &str = env!("CARGO_PKG_VERSION");

macro_rules! out {
    ($silent:expr, $($arg:tt)*) => {
        if !$silent { println!($($arg)*); }
    };
}

macro_rules! err {
    ($silent:expr, $($arg:tt)*) => {
        if !$silent { eprintln!($($arg)*); }
    };
}

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
    #[serde(default = "default_timeout")]
    timeout_secs: u64,
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

fn default_timeout() -> u64 {
    30
}

struct TempFile(PathBuf, bool);

impl TempFile {
    fn new(path: PathBuf, auto_delete: bool) -> Self {
        Self(path, auto_delete)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        if self.1 {
            fs::remove_file(&self.0).ok();
        }
    }
}

fn load_config() -> Result<Config> {
    let home = std::env::var("HOME").context("$HOME is not set")?;
    let path = Path::new(&home).join(".config/zenbuser/zenbuser.toml");
    let contents = fs::read_to_string(&path)
        .with_context(|| format!("Could not read config at {}", path.display()))?;
    toml::from_str(&contents).context("Failed to parse config")
}

fn validate_config(cfg: &Config) -> Result<()> {
    ensure!(cfg.filename.random_bytes >= 8, "filename.random_bytes must be >= 8");
    ensure!(!cfg.screenshot.allowed_mime_types.is_empty(), "screenshot.allowed_mime_types must not be empty");

    let temp_dir = Path::new(&cfg.screenshot.temp_dir);
    ensure!(temp_dir.is_dir(), "screenshot.temp_dir {:?} does not exist or is not a directory", temp_dir);

    let parsed = Url::parse(&cfg.upload.url).context("upload.url is not a valid URL")?;
    ensure!(parsed.scheme() == "https", "upload.url must use HTTPS");

    ensure!(cfg.upload.timeout_secs > 0, "upload.timeout_secs must be > 0");

    Ok(())
}

fn random_hex(n: usize) -> Result<String> {
    let mut buf = vec![0u8; n];
    getrandom(&mut buf).map_err(|e| anyhow::anyhow!("Failed to generate random bytes: {}", e))?;
    Ok(buf.iter().map(|b| format!("{:02x}", b)).collect())
}

fn random_filename(cfg: &FilenameConfig) -> Result<String> {
    Ok(format!("{}.{}", random_hex(cfg.random_bytes)?, cfg.extension))
}

fn capture_screenshot(cfg: &ScreenshotConfig, temp_file: &Path) -> Result<()> {
    match cfg.output.as_str() {
        "stdout" => {
            let out = Command::new(&cfg.tool)
                .args(&cfg.args)
                .output()
                .with_context(|| format!("Failed to run '{}'", cfg.tool))?;
            ensure!(
                out.status.success(),
                "'{}' exited with {}: {}",
                cfg.tool,
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            );
            ensure!(!out.stdout.is_empty(), "Screenshot tool produced no output");
            fs::write(temp_file, &out.stdout).context("Failed to write screenshot to temp file")?;
        }
        "file" => {
            let out = Command::new(&cfg.tool)
                .args(&cfg.args)
                .arg(temp_file)
                .output()
                .with_context(|| format!("Failed to run '{}'", cfg.tool))?;
            ensure!(
                out.status.success(),
                "'{}' exited with {}: {}",
                cfg.tool,
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            );
            ensure!(temp_file.exists(), "Screenshot tool exited successfully but created no file");
        }
        other => bail!("Unknown screenshot output mode '{}'", other),
    }

    let out = Command::new("file")
        .args(["--mime-type", "-b"])
        .arg(temp_file)
        .output()
        .context("Failed to run 'file' for MIME type detection")?;
    ensure!(out.status.success(), "'file' exited with {}", out.status);

    let mime = String::from_utf8(out.stdout)
        .context("'file' returned non-UTF8 output")?
        .trim()
        .to_string();

    ensure!(
        cfg.allowed_mime_types.iter().any(|m| m == &mime),
        "MIME type '{}' is not allowed. Allowed: {:?}",
        mime,
        cfg.allowed_mime_types
    );

    Ok(())
}

fn upload_screenshot(cfg: &UploadConfig, filename_cfg: &FilenameConfig, temp_file: &Path) -> Result<String> {
    let upload_name = random_filename(filename_cfg)?;
    let mut url = Url::parse(&cfg.url).context("upload.url is invalid")?;
    url.query_pairs_mut()
        .append_pair(&cfg.filename_param, &upload_name);

    let out = Command::new("curl")
        .args([
            "-s",
            "--fail-with-body",
            "--max-time", &cfg.timeout_secs.to_string(),
            "-H", &format!("Content-Type: {}", cfg.content_type),
            "--data-binary", &format!("@{}", temp_file.display()),
            url.as_str(),
        ])
        .output()
        .context("Failed to run 'curl'")?;

    ensure!(
        out.status.success(),
        "curl failed ({}): {}",
        out.status,
        String::from_utf8_lossy(&out.stderr).trim()
    );

    String::from_utf8(out.stdout).context("Upload response is not valid UTF-8")
}

fn json_path<'a>(value: &'a Value, path: &str) -> Option<&'a Value> {
    path.split('.').fold(Some(value), |acc, key| acc?.get(key))
}

fn extract_url(cfg: &UploadConfig, response: &str) -> Result<String> {
    let json: Value = serde_json::from_str(response).context("Failed to parse upload response as JSON")?;

    if let Some(url_str) = json_path(&json, &cfg.response_url_path).and_then(|v| v.as_str()) {
        let parsed = Url::parse(url_str)
            .with_context(|| format!("Server returned an invalid URL: '{}'", url_str))?;
        ensure!(
            parsed.scheme() == "https" || parsed.scheme() == "http",
            "Server returned a URL with unexpected scheme: '{}'", parsed.scheme()
        );
        return Ok(url_str.to_string());
    }

    let error = json_path(&json, &cfg.response_error_path)
        .and_then(|v| v.as_str())
        .unwrap_or("Unknown error (neither URL nor error field found in response)");
    bail!("{}", error)
}

fn copy_to_clipboard(cfg: &ClipboardConfig, text: &str) -> Result<()> {
    if cfg.use_stdin {
        let mut child = Command::new(&cfg.tool)
            .args(&cfg.args)
            .stdin(Stdio::piped())
            .spawn()
            .with_context(|| format!("Failed to run '{}'", cfg.tool))?;
        child
            .stdin
            .as_mut()
            .context("Failed to open stdin for clipboard tool")?
            .write_all(text.as_bytes())
            .context("Failed to write to clipboard tool stdin")?;
        let status = child.wait().context("Failed to wait on clipboard tool")?;
        ensure!(status.success(), "'{}' exited with {}", cfg.tool, status);
    } else {
        let out = Command::new(&cfg.tool)
            .args(&cfg.args)
            .arg(text)
            .output()
            .with_context(|| format!("Failed to run '{}'", cfg.tool))?;
        ensure!(
            out.status.success(),
            "'{}' exited with {}: {}",
            cfg.tool,
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

fn send_notification(cfg: &NotificationConfig, temp_file: &Path) -> Result<()> {
    let mut cmd = Command::new(&cfg.tool);
    cmd.arg(&cfg.message);
    cmd.args(&cfg.args);
    if cfg.include_screenshot_as_icon {
        cmd.args(["-i", &temp_file.display().to_string()]);
    }
    let out = cmd.output().with_context(|| format!("Failed to run '{}'", cfg.tool))?;
    ensure!(
        out.status.success(),
        "'{}' exited with {}: {}",
        cfg.tool,
        out.status,
        String::from_utf8_lossy(&out.stderr).trim()
    );
    Ok(())
}

fn run(silent: bool) -> Result<()> {
    let cfg = load_config()?;
    validate_config(&cfg)?;

    let temp_filename = random_filename(&cfg.filename)?;
    let temp_path = PathBuf::from(&cfg.screenshot.temp_dir).join(&temp_filename);
    let temp_file = TempFile::new(temp_path, cfg.cleanup.delete_temp_file);

    capture_screenshot(&cfg.screenshot, temp_file.path())?;

    let response = upload_screenshot(&cfg.upload, &cfg.filename, temp_file.path())?;
    let url = extract_url(&cfg.upload, &response)?;

    copy_to_clipboard(&cfg.clipboard, &url)?;
    send_notification(&cfg.notification, temp_file.path())?;

    out!(silent, "{}", url);

    Ok(())
}

fn print_version() {
    println!(
        "zenbuser v{}\n\
         \n\
         This software is released into the public domain under The Unlicense.\n\
         The author provides this software as-is, without warranty of any kind.\n\
         The author is not responsible for any damage, data loss, or other consequences\n\
         arising from its use. Use at your own risk.",
        VERSION
    );
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();

    let version = args.iter().any(|a| a == "--version" || a == "-v");
    let silent  = args.iter().any(|a| a == "--silent"  || a == "-s");

    let unknown = args.iter().find(|a| {
        !matches!(a.as_str(), "--version" | "-v" | "--silent" | "-s")
    });

    if let Some(flag) = unknown {
        eprintln!("Error: unknown flag '{}'", flag);
        eprintln!("Usage: zenbuser [--version | -v] [--silent | -s]");
        std::process::exit(1);
    }

    if version {
        print_version();
        return;
    }

    if let Err(e) = run(silent) {
        err!(silent, "Error: {:#}", e);
        std::process::exit(1);
    }
}
