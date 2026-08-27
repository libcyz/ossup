//! OSS 上传助手 — a thin, reliable GUI over the official `ossutil` CLI.
//!
//! Targets **ossutil 2.x**, which differs from 1.x in ways that matter here:
//! `version` is a subcommand (not `--version`), the config file uses a
//! `[default]` profile, `--jobs` became `-j`, and v4 signing makes `region`
//! mandatory — so it is derived from the endpoint, see `region_from_endpoint`.
//!
//! Design notes
//! -------------
//! * Credentials never touch the command line by default. They are written to a
//!   0600 config file that is passed with `--config-file` and removed when the
//!   upload finishes.
//! * The checkpoint directory is stable across runs, which is what makes
//!   "close the app, reopen, press start again" resume instead of restart.
//! * `ossutil` renders progress with carriage returns, so stdout is consumed as
//!   a byte stream and split on both `\r` and `\n`.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;
use regex::Regex;
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager, State};
use tokio::io::AsyncReadExt;
use tokio::process::{Child, Command};
use tokio::sync::Mutex;
use walkdir::WalkDir;

const EVENT: &str = "upload://event";

/* ------------------------------------------------------------------ */
/* config                                                              */
/* ------------------------------------------------------------------ */

fn d_jobs() -> u32 {
    5
}
fn d_parallel() -> u32 {
    8
}
fn d_part() -> u64 {
    16
}

fn d_max_tasks() -> u32 {
    3
}

/// 一套已保存的账号凭证。多账号切换用。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Account {
    /// 用户自己起的名字，比如"生产"、"测试"
    #[serde(default)]
    pub name: String,
    pub access_key_id: String,
    pub access_key_secret: String,
    #[serde(default)]
    pub endpoint: String,
}

/// 落盘形态。密钥同样只做 base64 —— 是遮眼不是加密，和主凭证一个待遇。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct StoredAccount {
    name: String,
    access_key_id: String,
    access_key_secret_enc: String,
    endpoint: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientConfig {
    #[serde(default)]
    pub access_key_id: String,
    #[serde(default)]
    pub access_key_secret: String,
    #[serde(default)]
    pub endpoint: String,
    #[serde(default)]
    pub bucket: String,
    #[serde(default)]
    pub prefix: String,
    #[serde(default)]
    pub remember: bool,
    #[serde(default = "d_jobs")]
    pub jobs: u32,
    #[serde(default = "d_parallel")]
    pub parallel: u32,
    #[serde(default = "d_part")]
    pub part_size_mb: u64,
    #[serde(default)]
    pub ossutil_path: String,
    #[serde(default)]
    pub cli_creds: bool,
    /// 同时进行的传输任务上限。每个任务是一个独立的 ossutil 进程，
    /// 不限制的话拖十个文件夹就是十个进程一起抢带宽。
    #[serde(default = "d_max_tasks")]
    pub max_tasks: u32,
    /// 已保存的账号，用于快速切换。
    #[serde(default)]
    pub accounts: Vec<Account>,
    /// Optional allow-list shown as a dropdown in the UI.
    #[serde(default)]
    pub buckets: Vec<String>,
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            access_key_id: String::new(),
            access_key_secret: String::new(),
            // 留空，让用户自己选地域，避免默认值把数据传到错误的地域
            endpoint: String::new(),
            bucket: String::new(),
            prefix: String::new(),
            remember: false,
            jobs: d_jobs(),
            parallel: d_parallel(),
            part_size_mb: d_part(),
            ossutil_path: String::new(),
            cli_creds: false,
            max_tasks: d_max_tasks(),
            accounts: Vec::new(),
            buckets: Vec::new(),
        }
    }
}

/// On-disk shape. The secret is base64 encoded so it is not readable at a
/// glance. This is obfuscation, not encryption — see README.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct StoredConfig {
    access_key_id: String,
    access_key_secret_enc: String,
    endpoint: String,
    bucket: String,
    prefix: String,
    remember: bool,
    jobs: u32,
    parallel: u32,
    part_size_mb: u64,
    ossutil_path: String,
    cli_creds: bool,
    max_tasks: u32,
    accounts: Vec<StoredAccount>,
    buckets: Vec<String>,
}

fn config_dir(app: &AppHandle) -> Result<PathBuf, String> {
    let dir = app
        .path()
        .app_config_dir()
        .map_err(|e| format!("无法定位配置目录: {e}"))?;
    std::fs::create_dir_all(&dir).map_err(|e| format!("无法创建配置目录: {e}"))?;
    Ok(dir)
}

fn config_path(app: &AppHandle) -> Result<PathBuf, String> {
    Ok(config_dir(app)?.join("config.json"))
}

#[tauri::command]
fn load_config(app: AppHandle) -> Result<ClientConfig, String> {
    let path = config_path(&app)?;
    if !path.exists() {
        return Ok(ClientConfig::default());
    }
    let raw = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
    let stored: StoredConfig = serde_json::from_str(&raw).unwrap_or_default();

    Ok(ClientConfig {
        access_key_id: stored.access_key_id,
        access_key_secret: decode_secret(&stored.access_key_secret_enc),
        endpoint: stored.endpoint,
        bucket: stored.bucket,
        prefix: stored.prefix,
        remember: stored.remember,
        jobs: if stored.jobs == 0 {
            d_jobs()
        } else {
            stored.jobs
        },
        parallel: if stored.parallel == 0 {
            d_parallel()
        } else {
            stored.parallel
        },
        part_size_mb: if stored.part_size_mb == 0 {
            d_part()
        } else {
            stored.part_size_mb
        },
        ossutil_path: stored.ossutil_path,
        cli_creds: stored.cli_creds,
        max_tasks: if stored.max_tasks == 0 {
            d_max_tasks()
        } else {
            stored.max_tasks
        },
        accounts: stored
            .accounts
            .into_iter()
            .map(|a| Account {
                name: a.name,
                access_key_id: a.access_key_id,
                access_key_secret: decode_secret(&a.access_key_secret_enc),
                endpoint: a.endpoint,
            })
            .collect(),
        buckets: stored.buckets,
    })
}

/// base64 解不开就当空字符串 —— 配置被手改过也不该让整个应用起不来
fn decode_secret(enc: &str) -> String {
    B64.decode(enc.as_bytes())
        .ok()
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .unwrap_or_default()
}

#[tauri::command]
fn save_config(app: AppHandle, cfg: ClientConfig) -> Result<(), String> {
    let path = config_path(&app)?;
    let stored = StoredConfig {
        access_key_id: cfg.access_key_id,
        access_key_secret_enc: B64.encode(cfg.access_key_secret.as_bytes()),
        endpoint: cfg.endpoint,
        bucket: cfg.bucket,
        prefix: cfg.prefix,
        remember: cfg.remember,
        jobs: cfg.jobs,
        parallel: cfg.parallel,
        part_size_mb: cfg.part_size_mb,
        ossutil_path: cfg.ossutil_path,
        cli_creds: cfg.cli_creds,
        max_tasks: cfg.max_tasks,
        accounts: cfg
            .accounts
            .into_iter()
            .map(|a| StoredAccount {
                name: a.name,
                access_key_id: a.access_key_id,
                access_key_secret_enc: B64.encode(a.access_key_secret.as_bytes()),
                endpoint: a.endpoint,
            })
            .collect(),
        buckets: cfg.buckets,
    };
    let json = serde_json::to_string_pretty(&stored).map_err(|e| e.to_string())?;
    std::fs::write(&path, json).map_err(|e| e.to_string())?;
    restrict_permissions(&path);
    Ok(())
}

#[cfg(unix)]
fn restrict_permissions(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}

#[cfg(not(unix))]
fn restrict_permissions(_path: &Path) {}

/* ------------------------------------------------------------------ */
/* ossutil discovery                                                   */
/* ------------------------------------------------------------------ */

fn binary_name() -> &'static str {
    if cfg!(windows) {
        "ossutil.exe"
    } else {
        "ossutil"
    }
}

/// ossutil 直接嵌进 exe，单文件下载即用；首次运行解到缓存目录。
#[cfg(windows)]
const EMBEDDED_OSSUTIL: &[u8] = include_bytes!("../binaries/ossutil.exe");

/// 解出内嵌的 ossutil。长度不一致视为旧版本，覆盖重写。
#[cfg(windows)]
fn extract_embedded(app: &AppHandle) -> Option<PathBuf> {
    let dir = app.path().app_cache_dir().ok()?;
    std::fs::create_dir_all(&dir).ok()?;
    let path = dir.join(binary_name());
    let fresh = std::fs::metadata(&path)
        .map(|m| m.len() == EMBEDDED_OSSUTIL.len() as u64)
        .unwrap_or(false);
    // ponytail: 只比长度，不算哈希——同长度不同内容的 ossutil 不会出现在发布流程里
    if !fresh {
        std::fs::write(&path, EMBEDDED_OSSUTIL).ok()?;
    }
    Some(path)
}

#[cfg(not(windows))]
fn extract_embedded(_app: &AppHandle) -> Option<PathBuf> {
    None
}

/// Resolution order: user override -> bundled resource -> embedded copy -> system PATH.
fn resolve_ossutil(app: &AppHandle, custom: &str) -> PathBuf {
    let custom = custom.trim();
    if !custom.is_empty() {
        return PathBuf::from(custom);
    }
    if let Ok(dir) = app.path().resource_dir() {
        let bundled = dir.join("binaries").join(binary_name());
        if bundled.exists() {
            return bundled;
        }
    }
    if let Some(path) = extract_embedded(app) {
        return path;
    }
    PathBuf::from(binary_name())
}

fn base_command(app: &AppHandle, custom: &str) -> Command {
    let mut cmd = Command::new(resolve_ossutil(app, custom));
    #[cfg(windows)]
    {
        // CREATE_NO_WINDOW — keeps a console from flashing on every call.
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000);
    }
    cmd
}

#[tauri::command]
async fn check_ossutil(app: AppHandle, ossutil_path: String) -> Result<String, String> {
    // 2.x 是 `ossutil version`；1.x 只认 `--version`，两个都试一下。
    let mut output = base_command(&app, &ossutil_path)
        .arg("version")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(|e| format!("无法启动 ossutil: {e}"))?;

    if !output.status.success() {
        output = base_command(&app, &ossutil_path)
            .arg("--version")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .await
            .map_err(|e| format!("无法启动 ossutil: {e}"))?;
    }

    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let line = text
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("ossutil")
        .trim()
        .to_string();
    // 2.x 的 `version` 只吐一个裸版本号（"2.3.0"），补上名字界面上才看得懂。
    if line.to_lowercase().contains("ossutil") {
        Ok(line)
    } else {
        Ok(format!("ossutil {line}"))
    }
}

/* ------------------------------------------------------------------ */
/* requests + events                                                   */
/* ------------------------------------------------------------------ */

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UploadRequest {
    pub local_path: String,
    pub bucket: String,
    #[serde(default)]
    pub prefix: String,
    pub access_key_id: String,
    pub access_key_secret: String,
    pub endpoint: String,
    #[serde(default = "d_jobs")]
    pub jobs: u32,
    #[serde(default = "d_parallel")]
    pub parallel: u32,
    #[serde(default = "d_part")]
    pub part_size_mb: u64,
    #[serde(default)]
    pub ossutil_path: String,
    #[serde(default)]
    pub cli_creds: bool,
}

impl UploadRequest {
    fn target(&self) -> String {
        let prefix = self.prefix.trim().trim_matches('/');
        if prefix.is_empty() {
            format!("oss://{}/", self.bucket.trim())
        } else {
            format!("oss://{}/{}/", self.bucket.trim(), prefix)
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct UploadEvent {
    kind: &'static str,
    /// 哪个任务发出来的。多任务并发时前端靠它把进度分派到对应的行上，
    /// 不带这个所有任务的进度会糊成一团。
    #[serde(skip_serializing_if = "Option::is_none")]
    task_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    line: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    percent: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    speed: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    total_num: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    ok_num: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    code: Option<i32>,
}

impl UploadEvent {
    fn log(line: String) -> Self {
        Self {
            kind: "log",
            task_id: None,
            line: Some(line),
            percent: None,
            speed: None,
            total_num: None,
            ok_num: None,
            code: None,
        }
    }

    fn for_task(mut self, task_id: &str) -> Self {
        self.task_id = Some(task_id.to_string());
        self
    }
}

/* ------------------------------------------------------------------ */
/* progress parsing                                                    */
/* ------------------------------------------------------------------ */

fn re(pattern: &'static str, cell: &'static OnceLock<Regex>) -> &'static Regex {
    cell.get_or_init(|| Regex::new(pattern).expect("valid regex"))
}

fn cap_f64(text: &str, pattern: &'static str, cell: &'static OnceLock<Regex>) -> Option<f64> {
    re(pattern, cell)
        .captures(text)
        .and_then(|c| c.get(1))
        .and_then(|m| m.as_str().replace(',', "").parse().ok())
}

fn cap_u64(text: &str, pattern: &'static str, cell: &'static OnceLock<Regex>) -> Option<u64> {
    cap_f64(text, pattern, cell).map(|v| v as u64)
}

fn cap_str(text: &str, pattern: &'static str, cell: &'static OnceLock<Regex>) -> Option<String> {
    re(pattern, cell)
        .captures(text)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str().trim().to_string())
}

/// `ossutil` progress lines differ between 1.x and 2.x, so every field is
/// parsed independently and treated as optional.
///
/// 1.x: `Total num: 12, size: ... OK num: 3 ... speed: 1.2 MB/s`
/// 2.x: `Total 2 files, 12 B, 1 dirs, Upload done:(1 objects) failed:(0 objects)`
///
/// 2.x 压根不打百分比，所以百分比按文件数自己算。
fn parse_line(line: &str) -> UploadEvent {
    static PERCENT: OnceLock<Regex> = OnceLock::new();
    static SPEED: OnceLock<Regex> = OnceLock::new();
    static TOTAL_1X: OnceLock<Regex> = OnceLock::new();
    static OK_1X: OnceLock<Regex> = OnceLock::new();
    static TOTAL_2X: OnceLock<Regex> = OnceLock::new();
    static OK_2X: OnceLock<Regex> = OnceLock::new();

    // 1.x 写成 "speed: 1.2 MB/s"，2.x 直接是 "1.2 MiB/s"，所以前缀可有可无。
    let speed = cap_str(line, r"(?i)([\d.]+\s*[KMGT]?i?B/s)", &SPEED);
    let total_num = cap_u64(line, r"(?i)total\s+num:\s*([\d,]+)", &TOTAL_1X)
        .or_else(|| cap_u64(line, r"(?i)total\s+([\d,]+)\s+files?\b", &TOTAL_2X));
    // 只认 "done:("，别把 "failed:(3 objects)" 当成传好了
    let ok_num = cap_u64(line, r"(?i)(?:ok|dealt)\s+num:\s*([\d,]+)", &OK_1X)
        .or_else(|| cap_u64(line, r"(?i)\bdone:\(\s*([\d,]+)\s+objects?", &OK_2X));

    let percent = cap_f64(line, r"(\d+(?:\.\d+)?)\s*%", &PERCENT).or(match (ok_num, total_num) {
        (Some(ok), Some(total)) if total > 0 => Some(ok as f64 * 100.0 / total as f64),
        _ => None,
    });

    let is_progress = percent.is_some() || speed.is_some() || total_num.is_some();

    UploadEvent {
        kind: if is_progress { "progress" } else { "log" },
        task_id: None,
        line: Some(line.to_string()),
        percent,
        speed,
        total_num,
        ok_num,
        code: None,
    }
}

/* ------------------------------------------------------------------ */
/* stream pump                                                         */
/* ------------------------------------------------------------------ */

/// Reads a child stream byte by byte and flushes a segment on `\n` **or**
/// `\r`. Line-oriented readers hang forever on `ossutil`'s progress output
/// because it repaints the same line with carriage returns.
async fn pump<R>(mut reader: R, app: AppHandle, task_id: String)
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    let mut chunk = [0u8; 4096];
    let mut acc: Vec<u8> = Vec::with_capacity(256);

    loop {
        match reader.read(&mut chunk).await {
            Ok(0) => break,
            Ok(n) => {
                for &byte in &chunk[..n] {
                    if byte == b'\n' || byte == b'\r' {
                        flush(&mut acc, &app, &task_id);
                    } else {
                        acc.push(byte);
                    }
                }
            }
            Err(_) => break,
        }
    }
    flush(&mut acc, &app, &task_id);
}

fn flush(acc: &mut Vec<u8>, app: &AppHandle, task_id: &str) {
    if acc.is_empty() {
        return;
    }
    let text = String::from_utf8_lossy(acc).trim_end().to_string();
    acc.clear();
    if text.is_empty() {
        return;
    }
    let _ = app.emit(EVENT, parse_line(&text).for_task(task_id));
}

/* ------------------------------------------------------------------ */
/* credential file                                                     */
/* ------------------------------------------------------------------ */

/// ossutil 2.x 默认用 v4 签名，v4 强制要求 region，而界面上只让填 endpoint，
/// 所以从 endpoint 反推。推不出来（自定义域名、传输加速域名）就返回 None，
/// 调用方会退回 v1 签名。
fn region_from_endpoint(endpoint: &str) -> Option<String> {
    static REGION: OnceLock<Regex> = OnceLock::new();
    re(
        r"(?i)^oss-([a-z]{2}-[a-z0-9-]+?)(?:-internal)?\.aliyuncs\.com$",
        &REGION,
    )
    .captures(endpoint.trim())
    .map(|c| c[1].to_lowercase())
}

/// 每次调用给一个独一无二的凭证文件名。
///
/// 以前所有操作共用 `session.ossutilconfig` 一个固定路径，并且用完就删 ——
/// 并发之后这是致命的：一次列目录跑完会把正在传输的任务的凭证文件删掉，
/// 两个同时启动的任务还会互相覆盖对方的内容。
fn cred_tag() -> String {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    format!(
        "{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    )
}

fn write_cred_file(app: &AppHandle, ak: &str, sk: &str, endpoint: &str) -> Result<PathBuf, String> {
    let path = config_dir(app)?.join(format!("session-{}.ossutilconfig", cred_tag()));
    let endpoint = endpoint.trim();
    let mut body = format!(
        "[default]\nlanguage=CH\naccessKeyID={}\naccessKeySecret={}\n",
        ak.trim(),
        sk
    );
    if !endpoint.is_empty() {
        body.push_str(&format!("endpoint={endpoint}\n"));
    }
    if let Some(region) = region_from_endpoint(endpoint) {
        body.push_str(&format!("region={region}\n"));
    }
    std::fs::write(&path, body).map_err(|e| format!("无法写入临时凭证文件: {e}"))?;
    restrict_permissions(&path);
    Ok(path)
}

/// 凭证 + 签名版本，`cp` 和 `ls` 都要带。
fn add_auth_args(cmd: &mut Command, cred_file: &Path, endpoint: &str) {
    cmd.arg("--config-file").arg(cred_file);
    if region_from_endpoint(endpoint).is_none() {
        cmd.arg("--sign-version").arg("v1");
    }
}

/* ------------------------------------------------------------------ */
/* OSS REST API passthrough                                            */
/* ------------------------------------------------------------------ */

/// 登录态所需的最小信息。上传/校验用的 `UploadRequest` 是它的超集。
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Auth {
    pub access_key_id: String,
    pub access_key_secret: String,
    pub endpoint: String,
    #[serde(default)]
    pub ossutil_path: String,
}

/// `ossutil api <op> [args] --output-format json` 的透传。
///
/// ossutil 的 `api` 子命令覆盖了 OSS 全部 REST API，所以浏览、删除、改名、ACL
/// 这些都不需要各写一个 command——换个 `op` 就行。
/// ponytail: 前端能调任意 op。这是本地桌面应用，webview 里跑的就是本仓库的
/// 代码，不是信任边界；真要收紧就在这里加一张 op 白名单。
#[tauri::command]
async fn oss_api(
    app: AppHandle,
    auth: Auth,
    op: String,
    args: Vec<String>,
) -> Result<serde_json::Value, String> {
    let cred_file = write_cred_file(
        &app,
        &auth.access_key_id,
        &auth.access_key_secret,
        &auth.endpoint,
    )?;

    let mut cmd = base_command(&app, &auth.ossutil_path);
    cmd.arg("api").arg(&op);
    add_auth_args(&mut cmd, &cred_file, &auth.endpoint);
    cmd.args(&args).arg("--output-format").arg("json");

    let output = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::null())
        .output()
        .await
        .map_err(|e| {
            let _ = std::fs::remove_file(&cred_file);
            format!("无法启动 ossutil: {e}")
        })?;

    // 这个文件里是明文 AK/SK，用完立刻删掉，别让它留在磁盘上。
    // Windows 上 restrict_permissions 是空实现，更不能留。
    let _ = std::fs::remove_file(&cred_file);

    let stdout = String::from_utf8_lossy(&output.stdout);
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        // ossutil 的服务端错误是多行的（Error Code / Message / Request Id），
        // 只取第一行会把最有用的 InvalidAccessKeyId 丢掉，所以整段透出，
        // 只滤掉每次都跟在末尾的 "0.147220(s) elapsed"。
        let msg = stderr
            .lines()
            .chain(stdout.lines())
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.ends_with("(s) elapsed"))
            .collect::<Vec<_>>()
            .join("\n");
        return Err(if msg.is_empty() {
            format!("ossutil {op} 执行失败")
        } else {
            msg
        });
    }

    parse_api_output(&stdout)
}

/// ossutil 在 JSON 后面还会追加一行 `0.976553(s) elapsed`，直接 `from_str` 会报
/// trailing characters。用 StreamDeserializer 只吃第一个值，后面的原样丢掉。
/// 顺带覆盖了 delete-object 这类成功时不吐 body 的 op —— 没有值就是 Null。
fn parse_api_output(stdout: &str) -> Result<serde_json::Value, String> {
    let mut stream = serde_json::Deserializer::from_str(stdout).into_iter::<serde_json::Value>();
    match stream.next() {
        Some(Ok(value)) => Ok(value),
        Some(Err(e)) => Err(format!("解析 ossutil 输出失败: {e}\n{stdout}")),
        None => Ok(serde_json::Value::Null),
    }
}

/// 任意 ossutil 子命令的透传，返回 stdout。
///
/// `oss_api` 走的是 `ossutil api <REST-API>`；`cp` / `rm` / `mkdir` / `presign`
/// 这些是顶层子命令，不在 api 底下，所以单开一个入口。区别只有一处：认证参数
/// 插在子命令名之后、其余参数之前 —— `cp src dst` 的位置参数顺序不能被打乱。
///
/// ponytail: 前端能跑任意子命令。这是本地桌面应用，webview 里跑的就是本仓库的
/// 代码，不是信任边界；真要收紧就在这里加一张子命令白名单。
#[tauri::command]
async fn oss_run(app: AppHandle, auth: Auth, args: Vec<String>) -> Result<String, String> {
    let Some((sub, rest)) = args.split_first() else {
        return Err("没有指定 ossutil 子命令".into());
    };

    let cred_file = write_cred_file(
        &app,
        &auth.access_key_id,
        &auth.access_key_secret,
        &auth.endpoint,
    )?;

    let mut cmd = base_command(&app, &auth.ossutil_path);
    cmd.arg(sub);
    add_auth_args(&mut cmd, &cred_file, &auth.endpoint);
    cmd.args(rest);

    let output = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::null())
        .output()
        .await
        .map_err(|e| {
            let _ = std::fs::remove_file(&cred_file);
            format!("无法启动 ossutil: {e}")
        })?;

    let _ = std::fs::remove_file(&cred_file);

    let stdout = String::from_utf8_lossy(&output.stdout);
    if output.status.success() {
        return Ok(stdout.trim().to_string());
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    let msg = stderr
        .lines()
        .chain(stdout.lines())
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.ends_with("(s) elapsed"))
        .collect::<Vec<_>>()
        .join("\n");
    Err(if msg.is_empty() {
        format!("ossutil {sub} 执行失败")
    } else {
        msg
    })
}

/* ------------------------------------------------------------------ */
/* upload                                                              */
/* ------------------------------------------------------------------ */

/// 并发中的传输任务表，按前端给的 task_id 索引。
///
/// 以前是单个 `Option<Child>`，第二个任务直接被拒。改成 HashMap 之后
/// 上传和下载可以同时跑、各自独立取消。
#[derive(Default)]
pub struct AppState {
    children: Arc<Mutex<std::collections::HashMap<String, Child>>>,
}

#[tauri::command]
async fn start_upload(
    app: AppHandle,
    state: State<'_, AppState>,
    req: UploadRequest,
    task_id: String,
) -> Result<(), String> {
    let source = PathBuf::from(&req.local_path);
    if !source.exists() {
        return Err(format!("本地路径不存在: {}", req.local_path));
    }
    // 单个文件不能带 -r，ossutil 会直接拒绝：
    // "xxx is a not directory, can not work with --recursive option"
    let recursive = source.is_dir();
    // `ossutil cp -r 本地目录 oss://bucket/前缀/` 只把目录里的内容铺到前缀下，
    // 目录本身不会出现在 OSS 上。补一层同名前缀，选中的文件夹才不会散开。
    let target = match (recursive, source.file_name().and_then(|n| n.to_str())) {
        (true, Some(name)) => format!("{}{}/", req.target(), name),
        // 拿不到名字的只有盘符根这类路径，维持原样直接铺开
        _ => req.target(),
    };
    spawn_transfer(
        app,
        state,
        req,
        source.display().to_string(),
        target,
        recursive,
        task_id,
    )
    .await
}

/// 下载就是把 `cp` 的两端调个个儿：断点续传、进度解析、取消，全都原样复用。
///
/// `recursive` 由前端按列表里的类型给：目录（OSS 上的 key 前缀）才要 `-r`，
/// 单个对象带上会被 ossutil 拒绝。`oss://` 路径没法在后端 stat，只能这样传。
#[tauri::command]
async fn start_download(
    app: AppHandle,
    state: State<'_, AppState>,
    req: UploadRequest,
    source: String,
    target: String,
    recursive: bool,
    task_id: String,
) -> Result<(), String> {
    spawn_transfer(app, state, req, source, target, recursive, task_id).await
}

/// `ossutil cp` 的公共流水线：凭证文件 -> spawn -> 边读边发进度 -> 退出后清理。
async fn spawn_transfer(
    app: AppHandle,
    state: State<'_, AppState>,
    req: UploadRequest,
    source: String,
    target: String,
    recursive: bool,
    task_id: String,
) -> Result<(), String> {
    if state.children.lock().await.contains_key(&task_id) {
        return Err(format!("任务 {task_id} 已在运行"));
    }

    let cfg_dir = config_dir(&app)?;
    let checkpoint = cfg_dir.join("checkpoints");
    std::fs::create_dir_all(&checkpoint).map_err(|e| e.to_string())?;
    let cred_file = write_cred_file(&app, &req.access_key_id, &req.access_key_secret, &req.endpoint)?;

    let part_bytes = req.part_size_mb.max(1) * 1024 * 1024;

    let mut cmd = base_command(&app, &req.ossutil_path);
    cmd.arg("cp");
    if recursive {
        cmd.arg("-r"); // 只有目录才递归；单个文件带 -r 会被 ossutil 拒绝
    }
    cmd.arg("-u") // skip objects that are already up to date
        .arg("-f"); // never prompt
    add_auth_args(&mut cmd, &cred_file, &req.endpoint);
    cmd.arg("--checkpoint-dir")
        .arg(&checkpoint)
        // 2.x 的错误报告默认写到当前工作目录的 ossutil_output/，挪进配置目录
        .arg("--output-dir")
        .arg(cfg_dir.join("output"))
        .arg("-j") // 2.x 把 --jobs 改成了 -j
        .arg(req.jobs.max(1).to_string())
        .arg("--parallel")
        .arg(req.parallel.max(1).to_string())
        .arg("--part-size")
        .arg(part_bytes.to_string())
        .arg(&source)
        .arg(&target);

    if req.cli_creds {
        cmd.arg("-i")
            .arg(req.access_key_id.trim())
            .arg("-k")
            .arg(&req.access_key_secret)
            .arg("-e")
            .arg(req.endpoint.trim());
    }

    cmd.stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::null())
        .kill_on_drop(true);

    let _ = app.emit(
        EVENT,
        UploadEvent::log(format!(
            "[start] {} -> {}  (jobs={}, parallel={}, part-size={}MB)",
            source,
            target,
            req.jobs,
            req.parallel,
            req.part_size_mb
        )),
    );

    let mut child = cmd.spawn().map_err(|e| {
        let _ = std::fs::remove_file(&cred_file);
        format!("无法启动 ossutil: {e}")
    })?;

    if let Some(stdout) = child.stdout.take() {
        tokio::spawn(pump(stdout, app.clone(), task_id.clone()));
    }
    if let Some(stderr) = child.stderr.take() {
        tokio::spawn(pump(stderr, app.clone(), task_id.clone()));
    }

    let table = state.children.clone();
    table.lock().await.insert(task_id.clone(), child);

    let id = task_id.clone();
    // 轮询而不是 await wait()：await 会一直占着锁，取消命令就拿不到了。
    tokio::spawn(async move {
        let code = loop {
            tokio::time::sleep(Duration::from_millis(250)).await;
            let mut guard = table.lock().await;
            // 只动自己那一项，别碰别的任务
            let Some(child) = guard.get_mut(&id) else {
                break -1;
            };
            match child.try_wait() {
                Ok(Some(status)) => {
                    guard.remove(&id);
                    break status.code().unwrap_or(-1);
                }
                Ok(None) => continue,
                Err(_) => {
                    guard.remove(&id);
                    break -1;
                }
            }
        };

        let _ = std::fs::remove_file(&cred_file);
        let _ = app.emit(
            EVENT,
            UploadEvent {
                kind: "finished",
                task_id: Some(task_id.clone()),
                line: Some(format!("[exit] ossutil 退出码 {code}")),
                percent: None,
                speed: None,
                total_num: None,
                ok_num: None,
                code: Some(code),
            },
        );
    });

    Ok(())
}

/// 取消指定任务；`task_id` 为空则全部取消。
///
/// 只发 kill，不从表里删 —— 让轮询那边统一收尾，才能保证 finished 事件
/// 一定会发出去（前端靠它把这一行从列表里摘掉）。
#[tauri::command]
async fn cancel_transfer(
    state: State<'_, AppState>,
    task_id: Option<String>,
) -> Result<(), String> {
    let mut guard = state.children.lock().await;
    match task_id {
        Some(id) => match guard.get_mut(&id) {
            Some(child) => child.start_kill().map_err(|e| e.to_string()),
            None => Err("这个任务已经不在运行了".into()),
        },
        None => {
            if guard.is_empty() {
                return Err("没有正在运行的传输任务".into());
            }
            for child in guard.values_mut() {
                let _ = child.start_kill();
            }
            Ok(())
        }
    }
}

/* ------------------------------------------------------------------ */
/* verification                                                        */
/* ------------------------------------------------------------------ */

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VerifyResult {
    local_count: u64,
    local_size: u64,
    local_size_human: String,
    remote_count: u64,
}

fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.2} {}", UNITS[unit])
    }
}

fn scan_local(root: &Path) -> (u64, u64) {
    let mut count = 0;
    let mut size = 0;
    for entry in WalkDir::new(root).into_iter().filter_map(Result::ok) {
        if entry.file_type().is_file() {
            count += 1;
            if let Ok(meta) = entry.metadata() {
                size += meta.len();
            }
        }
    }
    (count, size)
}

#[tauri::command]
async fn verify_upload(app: AppHandle, req: UploadRequest) -> Result<VerifyResult, String> {
    let source = PathBuf::from(&req.local_path);
    let (local_count, local_size) = tokio::task::spawn_blocking(move || scan_local(&source))
        .await
        .map_err(|e| e.to_string())?;

    let cred_file = write_cred_file(&app, &req.access_key_id, &req.access_key_secret, &req.endpoint)?;
    let mut cmd = base_command(&app, &req.ossutil_path);
    cmd.arg("ls").arg("--short-format"); // 2.x 去掉了 -s 短写法
    add_auth_args(&mut cmd, &cred_file, &req.endpoint);
    let output = cmd
        .arg(req.target())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::null())
        .output()
        .await;
    let _ = std::fs::remove_file(&cred_file);

    let output = output.map_err(|e| format!("校验失败: {e}"))?;
    let text = String::from_utf8_lossy(&output.stdout).to_string();

    static OBJ: OnceLock<Regex> = OnceLock::new();
    let remote_count = cap_u64(&text, r"(?i)object\s+number\s+is:\s*([\d,]+)", &OBJ)
        .unwrap_or_else(|| text.lines().filter(|l| l.starts_with("oss://")).count() as u64);

    Ok(VerifyResult {
        local_count,
        local_size,
        local_size_human: human_size(local_size),
        remote_count,
    })
}

/* ------------------------------------------------------------------ */
/* entry point                                                         */
/* ------------------------------------------------------------------ */

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .manage(AppState::default())
        .invoke_handler(tauri::generate_handler![
            load_config,
            save_config,
            check_ossutil,
            oss_api,
            oss_run,
            start_upload,
            start_download,
            cancel_transfer,
            verify_upload,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

#[cfg(test)]
mod tests {
    use super::{parse_api_output, parse_line, region_from_endpoint};

    /// ossutil 真实输出的形状：JSON 之后跟一个空行 + 计时行。
    #[test]
    fn api_output_ignores_trailing_elapsed_line() {
        let raw = "{\"Buckets\":{\"Bucket\":[]},\"IsTruncated\":\"true\"}\n\n0.976553(s) elapsed\n";
        let value = parse_api_output(raw).expect("尾部计时行不该让解析失败");
        assert_eq!(value["IsTruncated"], "true");
    }

    #[test]
    fn api_output_empty_is_null() {
        assert!(parse_api_output("").unwrap().is_null());
    }

    #[test]
    fn api_output_broken_json_still_errors() {
        assert!(parse_api_output("{not json").is_err());
    }


    #[test]
    fn progress_parsing() {
        // 2.x 实测输出
        let e =
            parse_line("Total 8 files, 12 B, 1 dirs, Upload done:(2 objects) failed:(0 objects)");
        assert_eq!(e.kind, "progress");
        assert_eq!(e.total_num, Some(8));
        assert_eq!(e.ok_num, Some(2));
        assert_eq!(e.percent, Some(25.0));

        // failed:( 不能被当成 done:(
        let e = parse_line(
            "Total 4 files, 12 B, 1 dirs, Upload done:(0 objects) failed:(4 objects, 12 B)",
        );
        assert_eq!(e.ok_num, Some(0));
        assert_eq!(e.percent, Some(0.0));

        // 1.x 输出，百分比和速度用它自己打的
        let e = parse_line("Total num: 10, size: 100. OK num: 5. 50% speed: 1.2 MB/s");
        assert_eq!(
            (e.total_num, e.ok_num, e.percent),
            (Some(10), Some(5), Some(50.0))
        );
        assert_eq!(e.speed.as_deref(), Some("1.2 MB/s"));

        // 2.x 裸速度，没有 "speed:" 前缀
        assert_eq!(
            parse_line("copying 3.5 MiB/s").speed.as_deref(),
            Some("3.5 MiB/s")
        );

        // 普通日志行不该被当成进度
        assert_eq!(parse_line("Error: NoSuchBucket").kind, "log");
    }

    #[test]
    fn region_derivation() {
        let r = |e| region_from_endpoint(e);
        assert_eq!(
            r("oss-cn-hangzhou.aliyuncs.com").as_deref(),
            Some("cn-hangzhou")
        );
        assert_eq!(
            r("oss-cn-hangzhou-internal.aliyuncs.com").as_deref(),
            Some("cn-hangzhou")
        );
        assert_eq!(
            r("oss-ap-southeast-1.aliyuncs.com").as_deref(),
            Some("ap-southeast-1")
        );
        assert_eq!(
            r(" oss-us-west-1.aliyuncs.com ").as_deref(),
            Some("us-west-1")
        );
        // 推不出 region 的：传输加速、自定义域名、空值 —— 调用方退回 v1 签名
        assert_eq!(r("oss-accelerate.aliyuncs.com"), None);
        assert_eq!(r("cdn.example.com"), None);
        assert_eq!(r(""), None);
    }
}
