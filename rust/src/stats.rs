use anyhow::Result;
use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Stats {
    /// 啟動時間
    pub start_time: Option<DateTime<Local>>,
    /// 上次更新時間
    pub last_update: Option<DateTime<Local>>,
    
    /// 錄製統計
    pub segments_created: u64,
    pub ffmpeg_restarts: u64,
    pub ffmpeg_errors: HashMap<u32, Vec<FfmpegError>>,
    
    /// 轉檔統計
    pub conversions_ok: u64,
    pub conversions_fail: u64,
    
    /// 上傳統計
    pub uploads_ok: u64,
    pub uploads_fail: u64,
    pub bytes_uploaded: u64,
    
    /// 清理統計
    pub files_cleaned: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FfmpegError {
    pub time: DateTime<Local>,
    pub reason: String,
}

impl Default for Stats {
    fn default() -> Self {
        Self::new()
    }
}

impl Stats {
    pub fn new() -> Self {
        Self {
            start_time: None,
            last_update: None,
            segments_created: 0,
            ffmpeg_restarts: 0,
            ffmpeg_errors: HashMap::new(),
            conversions_ok: 0,
            conversions_fail: 0,
            uploads_ok: 0,
            uploads_fail: 0,
            bytes_uploaded: 0,
            files_cleaned: 0,
        }
    }

    /// 標記啟動時間
    pub fn start(&mut self) {
        self.start_time = Some(Local::now());
        self.last_update = Some(Local::now());
    }

    /// 記錄新片段
    pub fn record_segment(&mut self) {
        self.segments_created += 1;
        self.last_update = Some(Local::now());
    }

    /// 記錄 ffmpeg 重啟
    pub fn record_ffmpeg_restart(&mut self, channel: u32, reason: &str) {
        self.ffmpeg_restarts += 1;
        let errors = self.ffmpeg_errors.entry(channel).or_insert_with(Vec::new);
        errors.push(FfmpegError {
            time: Local::now(),
            reason: reason.to_string(),
        });
        // 只保留每頻道最近 5 個錯誤
        if errors.len() > 5 {
            errors.remove(0);
        }
        self.last_update = Some(Local::now());
    }

    /// 記錄轉檔結果
    pub fn record_conversion(&mut self, success: bool) {
        if success {
            self.conversions_ok += 1;
        } else {
            self.conversions_fail += 1;
        }
        self.last_update = Some(Local::now());
    }

    /// 記錄上傳結果
    pub fn record_upload(&mut self, success: bool, size: u64) {
        if success {
            self.uploads_ok += 1;
            self.bytes_uploaded += size;
        } else {
            self.uploads_fail += 1;
        }
        self.last_update = Some(Local::now());
    }

    /// 記錄清理數量
    pub fn record_cleanup(&mut self, count: u64) {
        self.files_cleaned += count;
        self.last_update = Some(Local::now());
    }

    /// 取得運行時間字串
    pub fn get_uptime(&self) -> String {
        match self.start_time {
            Some(start) => {
                let duration = Local::now().signed_duration_since(start);
                let hours = duration.num_hours();
                let minutes = duration.num_minutes() % 60;
                let seconds = duration.num_seconds() % 60;
                format!("{}h {}m {}s", hours, minutes, seconds)
            }
            None => "未啟動".to_string(),
        }
    }

    /// 保存統計到檔案
    pub fn save(&self, output_dir: &Path) -> Result<()> {
        let stats_path = output_dir.join(".stats.json");
        let content = serde_json::to_string_pretty(self)?;
        std::fs::write(stats_path, content)?;
        Ok(())
    }

    /// 從檔案載入統計
    pub fn load(output_dir: &Path) -> Result<Self> {
        let stats_path = output_dir.join(".stats.json");
        if stats_path.exists() {
            let content = std::fs::read_to_string(stats_path)?;
            let stats: Stats = serde_json::from_str(&content)?;
            Ok(stats)
        } else {
            Ok(Self::new())
        }
    }

    /// 顯示統計摘要
    pub fn display(&self) {
        println!("=== RTSP Recorder 統計 ===");
        println!("運行時間: {}", self.get_uptime());
        println!();
        println!("📹 錄製:");
        println!("  片段數: {}", self.segments_created);
        println!("  ffmpeg 重啟: {}", self.ffmpeg_restarts);
        if !self.ffmpeg_errors.is_empty() {
            let channels: Vec<_> = self.ffmpeg_errors.keys().collect();
            println!("  有錯誤的頻道: {:?}", channels);
        }
        println!();
        println!("🔄 轉檔:");
        println!("  成功: {}", self.conversions_ok);
        println!("  失敗: {}", self.conversions_fail);
        println!();
        println!("☁️  上傳:");
        println!("  成功: {}", self.uploads_ok);
        println!("  失敗: {}", self.uploads_fail);
        println!("  總量: {:.2} MB", self.bytes_uploaded as f64 / 1024.0 / 1024.0);
        println!();
        println!("🗑️  清理:");
        println!("  已刪除檔案: {}", self.files_cleaned);
    }
}
