use anyhow::Result;
use chrono::Local;
use std::path::Path;
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::config::Config;
use crate::stats::Stats;

/// 檢查網路是否閒置
#[cfg(target_os = "linux")]
async fn is_network_idle(interface: &str, threshold_mbps: u32) -> bool {
    // 讀取 /sys/class/net/{interface}/statistics/rx_bytes
    let interface_name = if interface == "auto" {
        // 自動偵測：找第一個非 lo 的介面
        if let Ok(entries) = std::fs::read_dir("/sys/class/net") {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if name != "lo" {
                    return check_bandwidth(&name, threshold_mbps).await;
                }
            }
        }
        return true; // 找不到介面，假設閒置
    } else {
        interface.to_string()
    };

    check_bandwidth(&interface_name, threshold_mbps).await
}

#[cfg(target_os = "linux")]
async fn check_bandwidth(interface: &str, threshold_mbps: u32) -> bool {
    let path = format!("/sys/class/net/{}/statistics/rx_bytes", interface);
    
    // 讀取第一次
    let bytes1: u64 = match std::fs::read_to_string(&path) {
        Ok(s) => s.trim().parse().unwrap_or(0),
        Err(e) => {
            tracing::warn!("[網路] 無法讀取 {}: {}", path, e);
            return true; // 讀取失敗時允許上傳
        }
    };
    
    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
    
    // 讀取第二次
    let bytes2: u64 = match std::fs::read_to_string(&path) {
        Ok(s) => s.trim().parse().unwrap_or(0),
        Err(_) => return true,
    };
    
    let bytes_per_sec = bytes2.saturating_sub(bytes1);
    let mbps = (bytes_per_sec * 8) as f64 / 1_000_000.0;
    
    mbps < threshold_mbps as f64
}

#[cfg(not(target_os = "linux"))]
async fn is_network_idle(_interface: &str, _threshold_mbps: u32) -> bool {
    // macOS/Windows：暫時總是回傳 true（允許上傳）
    true
}

/// 驗證 GCS bucket 是否可存取
pub async fn verify_bucket(bucket: &str) -> Result<()> {
    tracing::info!("[GCS] 驗證 bucket: {}", bucket);
    cloud_storage::Client::default()
        .bucket()
        .read(bucket)
        .await
        .map_err(|e| anyhow::anyhow!("GCS bucket '{}' 無法存取: {}", bucket, e))?;
    tracing::info!("[GCS] ✅ bucket '{}' 驗證成功", bucket);
    Ok(())
}

/// 上傳單一檔案到 GCS
pub async fn upload_file(config: &Config, file_path: &Path, stats: Option<Arc<Mutex<Stats>>>) -> Result<()> {
    let bucket = &config.gcs.bucket;
    let prefix = &config.gcs.prefix;
    
    let file_name = file_path.file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown");

    // 按 channel/年/月/日 建立子資料夾: prefix/channel/YYYY/MM/DD/filename.mp4
    let channel = file_name.split('_').next().unwrap_or("unknown");
    let now = Local::now();
    let date_path = now.format("%Y/%m/%d").to_string();
    let object_name = if prefix.is_empty() {
        format!("{}/{}/{}", channel, date_path, file_name)
    } else {
        format!("{}/{}/{}/{}", prefix, channel, date_path, file_name)
    };

    let file_size = file_path.metadata().map(|m| m.len()).unwrap_or(0);

    tracing::info!("[上傳] {} -> gs://{}/{}", file_name, bucket, object_name);

    // 讀取檔案內容
    let file_bytes = tokio::fs::read(file_path).await?;

    // 使用 cloud-storage crate 上傳
    let result = cloud_storage::Client::default()
        .object()
        .create(bucket, file_bytes, &object_name, "application/octet-stream")
        .await;

    match result {
        Ok(_) => {
            tracing::info!("[上傳] 完成: {}", file_name);
            // 上傳成功後刪除本地檔案
            std::fs::remove_file(file_path)?;
            
            if let Some(stats) = stats {
                let mut s = stats.lock().await;
                s.record_upload(true, file_size);
            }
            Ok(())
        }
        Err(e) => {
            if let Some(stats) = stats {
                let mut s = stats.lock().await;
                s.record_upload(false, 0);
            }
            anyhow::bail!("上傳失敗: {} - {}", file_name, e)
        }
    }
}

/// 智慧上傳：網路閒置時上傳
pub async fn smart_upload(
    config: &Config,
    stats: Arc<Mutex<Stats>>,
    mut shutdown: tokio::sync::broadcast::Receiver<()>,
) {
    let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(60));
    
    loop {
        tokio::select! {
            _ = interval.tick() => {
                // 檢查網路是否閒置
                if !is_network_idle(&config.network.interface, config.network.idle_threshold_mbps).await {
                    tracing::debug!("[上傳] 網路忙碌，跳過");
                    continue;
                }

                // 掃描並上傳 MP4 檔案
                match std::fs::read_dir(&config.output.dir) {
                    Ok(entries) => {
                        for entry in entries.flatten() {
                            let path = entry.path();
                            if path.extension().and_then(|e| e.to_str()) == Some("mp4") {
                                // 再次檢查網路
                                if !is_network_idle(&config.network.interface, config.network.idle_threshold_mbps).await {
                                    tracing::debug!("[上傳] 網路變忙碌，暫停");
                                    break;
                                }
                                
                                if let Err(e) = upload_file(config, &path, Some(stats.clone())).await {
                                    tracing::error!("[上傳] 失敗: {:?} - {}", path.file_name(), e);
                                }
                            }
                        }
                    }
                    Err(e) => {
                        tracing::warn!("[上傳] 無法掃描目錄: {}", e);
                    }
                }
            }
            _ = shutdown.recv() => {
                tracing::info!("[上傳] 停止智慧上傳");
                break;
            }
        }
    }
}

/// 上傳所有待上傳的檔案
pub async fn upload_all(config: &Config, stats: Option<Arc<Mutex<Stats>>>) -> Result<()> {
    let output_dir = &config.output.dir;

    if !output_dir.exists() {
        tracing::info!("輸出目錄不存在，無檔案可上傳");
        return Ok(());
    }

    let mut uploaded = 0;
    let mut failed = 0;

    for entry in std::fs::read_dir(output_dir)? {
        let entry = entry?;
        let path = entry.path();

        // 只上傳 mp4 檔案
        if path.extension().and_then(|e| e.to_str()) == Some("mp4") {
            match upload_file(config, &path, stats.clone()).await {
                Ok(_) => uploaded += 1,
                Err(e) => {
                    tracing::error!("[上傳] 失敗: {:?} - {}", path.file_name(), e);
                    failed += 1;
                }
            }
        }
    }

    tracing::info!("[上傳] 完成: ✅{} ❌{}", uploaded, failed);
    Ok(())
}
