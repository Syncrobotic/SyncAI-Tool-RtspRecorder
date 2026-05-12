use anyhow::Result;
use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub rtsp: RtspConfig,
    pub schedule: ScheduleConfig,
    pub output: OutputConfig,
    pub gcs: GcsConfig,
    pub network: NetworkConfig,
    pub retention: RetentionConfig,
    pub log: LogConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RtspConfig {
    /// 串流清單（新格式）
    #[serde(default)]
    pub streams: Vec<StreamConfig>,
    #[serde(default = "default_segment_duration")]
    pub segment_duration: u32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct StreamConfig {
    pub name: String,
    pub url: String,
    /// 此串流的輸出解析度（可選，留空使用全域設定）
    #[serde(default)]
    pub resolution: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ScheduleConfig {
    #[serde(default = "default_start_hour")]
    pub record_start_hour: u32,
    #[serde(default = "default_end_hour")]
    pub record_end_hour: u32,
    /// UTC 時區偏移（例如 8 代表 UTC+8，-5 代表 UTC-5）
    #[serde(default = "default_utc_offset")]
    pub utc_offset: i32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct OutputConfig {
    #[serde(default = "default_output_dir")]
    pub dir: PathBuf,
    /// 輸出解析度，例如 "1920x1080"。留空或 "original" 表示不縮放
    #[serde(default = "default_resolution")]
    pub resolution: String,
    /// 強制重新編碼（確保最大播放兼容性，但會增加 CPU 使用）
    #[serde(default)]
    pub force_reencode: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GcsConfig {
    pub bucket: String,
    #[serde(default)]
    pub prefix: String,
    #[serde(default = "default_credentials")]
    pub credentials: PathBuf,
}

#[derive(Debug, Clone, Deserialize)]
pub struct NetworkConfig {
    #[serde(default = "default_interface")]
    pub interface: String,
    #[serde(default = "default_threshold")]
    pub idle_threshold_mbps: u32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RetentionConfig {
    #[serde(default = "default_max_hours")]
    pub max_hours: u32,
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct LogConfig {
    #[serde(default = "default_log_dir")]
    pub dir: PathBuf,
    #[serde(default = "default_rotation")]
    pub rotation: String,
    #[serde(default = "default_retention_days")]
    pub retention_days: u32,
    #[serde(default = "default_to_file")]
    pub to_file: bool,
}

// 預設值函數
fn default_segment_duration() -> u32 { 600 }
fn default_start_hour() -> u32 { 8 }
fn default_end_hour() -> u32 { 1 }
fn default_utc_offset() -> i32 { 0 }  // 預設 UTC+0
fn default_output_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("rtsp-recorder/videos")
}
fn default_resolution() -> String { "1920x1080".to_string() }
fn default_credentials() -> PathBuf { PathBuf::from("gcs-credentials.json") }
fn default_interface() -> String { "auto".to_string() }
fn default_threshold() -> u32 { 8 }
fn default_max_hours() -> u32 { 6 }
fn default_log_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("rtsp-recorder/videos/logs")
}
fn default_rotation() -> String { "time".to_string() }
fn default_retention_days() -> u32 { 30 }
fn default_to_file() -> bool { true }

/// 展開路徑中的 `~` 為使用者家目錄
fn expand_tilde(path: &Path) -> PathBuf {
    if let Ok(stripped) = path.strip_prefix("~") {
        if let Some(home) = dirs::home_dir() {
            return home.join(stripped);
        }
    }
    path.to_path_buf()
}

/// 解析路徑：先展開 ~，再將相對路徑轉為相對於 base_dir 的絕對路徑
fn resolve_path(path: &Path, base_dir: &Path) -> PathBuf {
    let expanded = expand_tilde(path);
    if expanded.is_absolute() {
        expanded
    } else {
        base_dir.join(expanded)
    }
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let content = std::fs::read_to_string(path)?;
        let mut config: Config = serde_yaml::from_str(&content)?;

        // 取得 config 檔案所在目錄，作為相對路徑的基準
        let base_dir = path.parent()
            .map(|p| if p.as_os_str().is_empty() { Path::new(".") } else { p })
            .unwrap_or(Path::new("."));
        let base_dir = std::fs::canonicalize(base_dir).unwrap_or_else(|_| base_dir.to_path_buf());

        // P1: 解析所有路徑（展開 ~ 並將相對路徑轉為絕對路徑）
        config.output.dir = resolve_path(&config.output.dir, &base_dir);
        config.gcs.credentials = resolve_path(&config.gcs.credentials, &base_dir);
        config.log.dir = resolve_path(&config.log.dir, &base_dir);

        Ok(config)
    }
}
