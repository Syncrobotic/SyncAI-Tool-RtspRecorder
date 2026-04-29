use anyhow::Result;
use chrono::{Local, Timelike};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use tokio::process::Command;
use tokio::sync::{broadcast, Mutex};

use crate::config::Config;
use crate::converter;
use crate::stats::Stats;

/// 檢查是否為錄製時間
fn is_recording_time(config: &Config) -> bool {
    let hour = Local::now().hour();
    let start = config.schedule.record_start_hour;
    let end = config.schedule.record_end_hour;

    if start < end {
        hour >= start && hour < end
    } else {
        // 跨日情況，例如 08:00 - 01:00
        hour >= start || hour < end
    }
}

/// 計算到下一個分段邊界的秒數
fn calc_seconds_to_next_boundary(segment_duration: u32) -> u32 {
    let now = Local::now();
    let seconds_since_midnight = now.num_seconds_from_midnight();
    let elapsed_in_segment = seconds_since_midnight % segment_duration;
    segment_duration - elapsed_in_segment
}

/// 建構 ffmpeg 命令
fn build_ffmpeg_args(url: &str, output_pattern: &str, segment_duration: u32) -> Vec<String> {
    vec![
        "-y".to_string(),
        "-fflags".to_string(), "+genpts+discardcorrupt+nobuffer+igndts".to_string(),
        "-rtsp_transport".to_string(), "tcp".to_string(),
        "-rtsp_flags".to_string(), "prefer_tcp".to_string(),
        "-use_wallclock_as_timestamps".to_string(), "1".to_string(),
        "-timeout".to_string(), "10000000".to_string(),
        "-buffer_size".to_string(), "8388608".to_string(),
        "-max_delay".to_string(), "500000".to_string(),
        "-reorder_queue_size".to_string(), "5000".to_string(),
        "-i".to_string(), url.to_string(),
        "-start_at_zero".to_string(),
        "-c:v".to_string(), "copy".to_string(),
        "-c:a".to_string(), "aac".to_string(),
        "-af".to_string(), "aresample=async=1".to_string(),
        "-map".to_string(), "0".to_string(),
        "-f".to_string(), "segment".to_string(),
        "-segment_time".to_string(), segment_duration.to_string(),
        "-segment_format".to_string(), "matroska".to_string(),
        "-reset_timestamps".to_string(), "1".to_string(),
        "-segment_atclocktime".to_string(), "1".to_string(),
        "-strftime".to_string(), "1".to_string(),
        "-avoid_negative_ts".to_string(), "make_zero".to_string(),
        "-max_muxing_queue_size".to_string(), "1024".to_string(),
        output_pattern.to_string(),
    ]
}

/// 錄製單一頻道（支援斷線接續）
async fn record_channel(
    config: &Config,
    channel: u32,
    stats: Arc<Mutex<Stats>>,
    mut shutdown: broadcast::Receiver<()>,
) -> Result<()> {
    let url = format!("{}/chID={}&streamType=main&linkType=tcp", config.rtsp.base_url, channel);
    let output_dir = &config.output.dir;
    std::fs::create_dir_all(output_dir)?;

    let output_pattern = output_dir
        .join(format!("ch{}_%Y%m%d_%H%M%S.mkv", channel))
        .to_string_lossy()
        .to_string();

    let segment_duration = config.rtsp.segment_duration;
    let mut is_resuming = false;

    loop {
        // 計算使用的 segment duration（斷線接續時使用剩餘時間）
        let current_duration = if is_resuming {
            let remaining = calc_seconds_to_next_boundary(segment_duration);
            if remaining < 30 {
                // 剩餘時間太短，等待下一週期
                tracing::info!("[CH{}] 剩餘 {}s 太短，等待下一週期", channel, remaining);
                tokio::time::sleep(tokio::time::Duration::from_secs(remaining as u64 + 1)).await;
                is_resuming = false;
                segment_duration
            } else {
                tracing::info!("[CH{}] 接續剩餘 {}s", channel, remaining);
                remaining
            }
        } else {
            segment_duration
        };

        let args = build_ffmpeg_args(&url, &output_pattern, current_duration);
        
        tracing::info!("[CH{}] 啟動錄製（每 {}s 分段）", channel, current_duration);

        let mut child = Command::new("ffmpeg")
            .args(&args)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .stdin(Stdio::piped())
            .spawn()?;

        tokio::select! {
            status = child.wait() => {
                match status {
                    Ok(s) if !s.success() => {
                        let code = s.code().unwrap_or(-1);
                        tracing::warn!("[CH{}] ffmpeg 退出 (code={})，3 秒後重啟", channel, code);
                        
                        // 記錄重啟統計
                        {
                            let mut stats = stats.lock().await;
                            stats.record_ffmpeg_restart(channel, &format!("exit_code={}", code));
                        }
                        
                        tokio::time::sleep(tokio::time::Duration::from_secs(3)).await;
                        is_resuming = true;  // 下次啟動使用接續模式
                    }
                    Err(e) => {
                        tracing::error!("[CH{}] ffmpeg 錯誤: {}，3 秒後重啟", channel, e);
                        {
                            let mut stats = stats.lock().await;
                            stats.record_ffmpeg_restart(channel, &e.to_string());
                        }
                        tokio::time::sleep(tokio::time::Duration::from_secs(3)).await;
                        is_resuming = true;
                    }
                    Ok(_) => {
                        // 正常完成一個片段
                        {
                            let mut stats = stats.lock().await;
                            stats.record_segment();
                        }
                        is_resuming = false;
                    }
                }
            }
            _ = shutdown.recv() => {
                tracing::info!("[CH{}] 收到停止訊號", channel);
                child.kill().await?;
                break;
            }
        }
    }

    Ok(())
}

/// 背景轉檔任務
async fn background_converter(
    output_dir: PathBuf,
    stats: Arc<Mutex<Stats>>,
    mut shutdown: broadcast::Receiver<()>,
) {
    let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(30));
    
    loop {
        tokio::select! {
            _ = interval.tick() => {
                // 掃描 MKV 檔案並轉檔
                if let Ok(entries) = std::fs::read_dir(&output_dir) {
                    for entry in entries.flatten() {
                        let path = entry.path();
                        if path.extension().and_then(|e| e.to_str()) == Some("mkv") {
                            // 檢查檔案是否正在寫入（修改時間 > 10 秒前）
                            if let Ok(metadata) = path.metadata() {
                                if let Ok(modified) = metadata.modified() {
                                    if modified.elapsed().map(|d| d.as_secs() > 10).unwrap_or(false) {
                                        match converter::convert_to_mp4(&path).await {
                                            Ok(Some(_)) => {
                                                let mut stats = stats.lock().await;
                                                stats.record_conversion(true);
                                            }
                                            Ok(None) | Err(_) => {
                                                let mut stats = stats.lock().await;
                                                stats.record_conversion(false);
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
            _ = shutdown.recv() => {
                tracing::info!("[轉檔] 停止背景轉檔");
                break;
            }
        }
    }
}

/// 執行 daemon 模式
pub async fn run_daemon(config: &Config, stats: Arc<Mutex<Stats>>) -> Result<()> {
    let (shutdown_tx, _) = broadcast::channel::<()>(16);

    // 設定 Ctrl+C 處理
    let shutdown_tx_clone = shutdown_tx.clone();
    tokio::spawn(async move {
        tokio::signal::ctrl_c().await.ok();
        tracing::info!("收到停止訊號，正在關閉...");
        let _ = shutdown_tx_clone.send(());
    });

    // 啟動背景轉檔
    let converter_stats = stats.clone();
    let converter_shutdown = shutdown_tx.subscribe();
    let output_dir = config.output.dir.clone();
    tokio::spawn(async move {
        background_converter(output_dir, converter_stats, converter_shutdown).await;
    });

    // 記錄啟動
    {
        let mut s = stats.lock().await;
        s.start();
    }

    loop {
        if !is_recording_time(config) {
            tracing::info!("目前不在錄製時間，等待中...");
            tokio::time::sleep(tokio::time::Duration::from_secs(60)).await;
            continue;
        }

        tracing::info!("開始錄製 {} 個頻道", config.rtsp.channels.len());

        let mut handles = vec![];
        for &ch in &config.rtsp.channels {
            let config = config.clone();
            let rx = shutdown_tx.subscribe();
            let channel_stats = stats.clone();
            handles.push(tokio::spawn(async move {
                if let Err(e) = record_channel(&config, ch, channel_stats, rx).await {
                    tracing::error!("[CH{}] 錯誤: {}", ch, e);
                }
            }));
        }

        // 等待所有錄製任務完成
        for handle in handles {
            handle.await?;
        }

        break;
    }

    // 保存統計
    {
        let stats = stats.lock().await;
        if let Err(e) = stats.save(&config.output.dir) {
            tracing::error!("保存統計失敗: {}", e);
        }
    }

    Ok(())
}

/// 手動錄製一次
pub async fn record_once(config: &Config, duration: u32, channels: &str) -> Result<()> {
    tracing::info!("手動錄製 {}s, 頻道: {}", duration, channels);
    // TODO: 解析頻道範圍並錄製
    Ok(())
}
