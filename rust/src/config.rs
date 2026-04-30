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
}

#[derive(Debug, Clone, Deserialize)]
pub struct ScheduleConfig {
    #[serde(default = "default_start_hour")]
    pub record_start_hour: u32,
    #[serde(default = "default_end_hour")]
    pub record_end_hour: u32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct OutputConfig {
    #[serde(default = "default_output_dir")]
    pub dir: PathBuf,
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
fn default_output_dir() -> PathBuf { dirs::home_dir().unwrap().join("rtsp-recorder/videos") }
fn default_credentials() -> PathBuf { PathBuf::from("gcs-credentials.json") }
fn default_interface() -> String { "auto".to_string() }
fn default_threshold() -> u32 { 8 }
fn default_max_hours() -> u32 { 6 }
fn default_log_dir() -> PathBuf { dirs::home_dir().unwrap().join("rtsp-recorder/videos/logs") }
fn default_rotation() -> String { "time".to_string() }
fn default_retention_days() -> u32 { 30 }
fn default_to_file() -> bool { true }

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let content = std::fs::read_to_string(path)?;
        let config: Config = serde_yaml::from_str(&content)?;
        Ok(config)
    }
}
