use anyhow::Result;
use chrono::{Local, Timelike};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use tokio::process::Command;
use tokio::sync::{broadcast, Mutex};

use rtsp_shared::config::{Config, StreamConfig};
use rtsp_shared::converter;
use rtsp_shared::ffmpeg::FfmpegPaths;
use rtsp_shared::stats::Stats;
use rtsp_shared::uploader;

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
    if segment_duration == 0 {
        return 1;
    }
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

/// 錄製單一串流（支援斷線接續）
async fn record_channel(
    config: &Config,
    stream: &StreamConfig,
    stats: Arc<Mutex<Stats>>,
    mut shutdown: broadcast::Receiver<()>,
    ff: &FfmpegPaths,
) -> Result<()> {
    let url = &stream.url;
    let stream_name = &stream.name;
    let output_dir = &config.output.dir;
    std::fs::create_dir_all(output_dir)?;

    // 確保輸出目錄和檔案對所有使用者可讀（解決 root 錄製 + 一般使用者上傳的權限問題）
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(output_dir, std::fs::Permissions::from_mode(0o755));
    }

    let output_pattern = output_dir
        .join(format!("{}_%Y%m%d_%H%M%S.mkv", stream_name))
        .to_string_lossy()
        .to_string();

    let segment_duration = config.rtsp.segment_duration;
    let mut is_resuming = false;

    loop {
        // P1: 檢查是否在錄製時段，不在則暫停
        if !is_recording_time(config) {
            tracing::info!("[{}] 目前不在錄製時段，暫停錄製", stream_name);
            loop {
                tokio::select! {
                    _ = tokio::time::sleep(tokio::time::Duration::from_secs(30)) => {
                        if is_recording_time(config) {
                            tracing::info!("[{}] 進入錄製時段，恢復錄製", stream_name);
                            break;
                        }
                    }
                    _ = shutdown.recv() => {
                        tracing::info!("[{}] 收到停止訊號", stream_name);
                        return Ok(());
                    }
                }
            }
        }

        // 計算使用的 segment duration（斷線接續時使用剩餘時間）
        let current_duration = if is_resuming {
            let remaining = calc_seconds_to_next_boundary(segment_duration);
            if remaining < 30 {
                // 剩餘時間太短，等待下一週期
                tracing::info!("[{}] 剩餘 {}s 太短，等待下一週期", stream_name, remaining);
                tokio::time::sleep(tokio::time::Duration::from_secs(remaining as u64 + 1)).await;
                segment_duration
            } else {
                tracing::info!("[{}] 接續剩餘 {}s", stream_name, remaining);
                remaining
            }
        } else {
            segment_duration
        };

        let args = build_ffmpeg_args(url, &output_pattern, current_duration);
        
        tracing::info!("[{}] 啟動錄製（每 {}s 分段）", stream_name, current_duration);

        // POSIX TZ 格式符號相反：UTC+8 要設成 "UTC-8"
        let tz = if config.schedule.utc_offset >= 0 {
            format!("UTC-{}", config.schedule.utc_offset)
        } else {
            format!("UTC+{}", -config.schedule.utc_offset)
        };

        let mut child = Command::new(&ff.ffmpeg)
            .args(&args)
            .env("TZ", &tz)  // 確保檔名使用正確時區
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .stdin(Stdio::null())
            .spawn()?;

        tokio::select! {
            status = child.wait() => {
                match status {
                    Ok(s) if !s.success() => {
                        let code = s.code().unwrap_or(-1);
                        tracing::warn!("[{}] ffmpeg 退出 (code={})，3 秒後重啟", stream_name, code);
                        
                        // 記錄重啟統計
                        {
                            let mut stats = stats.lock().await;
                            stats.record_ffmpeg_restart(stream_name, &format!("exit_code={}", code));
                        }
                        
                        tokio::time::sleep(tokio::time::Duration::from_secs(3)).await;
                        is_resuming = true;  // 下次啟動使用接續模式
                    }
                    Err(e) => {
                        tracing::error!("[{}] ffmpeg 錯誤: {}，3 秒後重啟", stream_name, e);
                        {
                            let mut stats = stats.lock().await;
                            stats.record_ffmpeg_restart(stream_name, &e.to_string());
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
                tracing::info!("[{}] 收到停止訊號", stream_name);
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
    ff: FfmpegPaths,
    streams: Vec<StreamConfig>,
    default_resolution: String,
    force_reencode: bool,
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
                            // 確保 MKV 檔案可被其他使用者讀取
                            #[cfg(unix)]
                            {
                                use std::os::unix::fs::PermissionsExt;
                                let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644));
                            }
                            // 從檔名取得 stream name，查找對應的解析度設定
                            let resolution = path.file_stem()
                                .and_then(|s| s.to_str())
                                .and_then(|name| name.split('_').next())
                                .and_then(|stream_name| {
                                    streams.iter()
                                        .find(|s| s.name == stream_name)
                                        .and_then(|s| s.resolution.clone())
                                })
                                .unwrap_or_else(|| default_resolution.clone());
                            // 檢查檔案是否正在寫入（修改時間 > 10 秒前）
                            if let Ok(metadata) = path.metadata() {
                                if let Ok(modified) = metadata.modified() {
                                    if modified.elapsed().map(|d| d.as_secs() > 10).unwrap_or(false) {
                                        match converter::convert_to_mp4(&path, &ff, &resolution, force_reencode).await {
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
pub async fn run_daemon(config: &Config, stats: Arc<Mutex<Stats>>, ff: &FfmpegPaths) -> Result<()> {
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
    let converter_ff = ff.clone();
    let converter_streams = config.rtsp.streams.clone();
    let converter_default_resolution = config.output.resolution.clone();
    let converter_force_reencode = config.output.force_reencode;
    tokio::spawn(async move {
        background_converter(output_dir, converter_stats, converter_shutdown, converter_ff, converter_streams, converter_default_resolution, converter_force_reencode).await;
        tracing::error!("[轉檔] 背景任務意外結束");
    });

    // P0: 啟動智慧上傳（網路閒置時自動上傳）
    let upload_config = config.clone();
    let upload_stats = stats.clone();
    let upload_shutdown = shutdown_tx.subscribe();
    tokio::spawn(async move {
        uploader::smart_upload(&upload_config, upload_stats, upload_shutdown).await;
        tracing::error!("[上傳] 背景任務意外結束");
    });

    // 啟動自動清理（每小時檢查一次）
    let cleanup_config = config.clone();
    let cleanup_stats = stats.clone();
    let cleanup_shutdown = shutdown_tx.subscribe();
    tokio::spawn(async move {
        background_cleanup(cleanup_config, cleanup_stats, cleanup_shutdown).await;
        tracing::error!("[清理] 背景任務意外結束");
    });

    // P1: 啟動定期保存統計
    let save_stats = stats.clone();
    let save_dir = config.output.dir.clone();
    let save_shutdown = shutdown_tx.subscribe();
    tokio::spawn(async move {
        background_stats_saver(save_stats, save_dir, save_shutdown).await;
        tracing::error!("[統計] 背景任務意外結束");
    });

    // 記錄啟動
    {
        let mut s = stats.lock().await;
        s.start();
    }

    // 主錄製迴圈
    let main_shutdown = shutdown_tx.subscribe();
    run_recording_loop(config, stats.clone(), ff, main_shutdown).await?;

    // 保存統計
    {
        let stats = stats.lock().await;
        if let Err(e) = stats.save(&config.output.dir) {
            tracing::error!("保存統計失敗: {}", e);
        }
    }

    Ok(())
}

/// 主錄製迴圈（在錄製時段啟動所有串流）
async fn run_recording_loop(
    config: &Config,
    stats: Arc<Mutex<Stats>>,
    ff: &FfmpegPaths,
    mut shutdown: broadcast::Receiver<()>,
) -> Result<()> {
    loop {
        if !is_recording_time(config) {
            tokio::select! {
                _ = tokio::time::sleep(tokio::time::Duration::from_secs(60)) => {
                    tracing::info!("目前不在錄製時間，等待中...");
                    continue;
                }
                _ = shutdown.recv() => {
                    tracing::info!("收到停止訊號，結束主迴圈");
                    return Ok(());
                }
            }
        }

        tracing::info!("開始錄製 {} 個串流", config.rtsp.streams.len());

        // 為每個串流建立一個新的 broadcast channel 來管理子任務
        let (stream_shutdown_tx, _) = broadcast::channel::<()>(16);

        let mut handles = vec![];
        for stream in &config.rtsp.streams {
            let config = config.clone();
            let stream = stream.clone();
            let rx = stream_shutdown_tx.subscribe();
            let channel_stats = stats.clone();
            let ch_ff = ff.clone();
            handles.push(tokio::spawn(async move {
                if let Err(e) = record_channel(&config, &stream, channel_stats, rx, &ch_ff).await {
                    tracing::error!("[{}] 錯誤: {}", stream.name, e);
                }
            }));
        }

        // 等待 shutdown 或所有錄製完成
        let all_done = async {
            for handle in handles {
                let _ = handle.await;
            }
        };
        tokio::pin!(all_done);

        tokio::select! {
            _ = shutdown.recv() => {
                tracing::info!("收到停止訊號，停止所有串流");
                let _ = stream_shutdown_tx.send(());
                return Ok(());
            }
            _ = &mut all_done => {
                // 所有串流都結束了（不太可能除非全部出錯），重新啟動
                tracing::warn!("所有串流意外結束，5 秒後重新啟動");
                tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
            }
        }
    }
}

/// 背景自動清理任務
async fn background_cleanup(
    config: Config,
    stats: Arc<Mutex<Stats>>,
    mut shutdown: broadcast::Receiver<()>,
) {
    // 每小時檢查一次
    let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(3600));
    
    loop {
        tokio::select! {
            _ = interval.tick() => {
                // 1. 清理舊影片檔案
                let threshold = std::time::SystemTime::now()
                    - std::time::Duration::from_secs(config.retention.max_hours as u64 * 3600);
                let mut deleted = 0u64;

                if let Ok(entries) = std::fs::read_dir(&config.output.dir) {
                    for entry in entries.flatten() {
                        let path = entry.path();
                        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
                        if !["mp4", "mkv"].contains(&ext) {
                            continue;
                        }
                        if let Ok(meta) = path.metadata() {
                            if let Ok(modified) = meta.modified() {
                                if modified < threshold {
                                    if std::fs::remove_file(&path).is_ok() {
                                        tracing::info!("[清理] 已刪除: {:?}", path.file_name());
                                        deleted += 1;
                                    }
                                }
                            }
                        }
                    }
                }

                if deleted > 0 {
                    let mut s = stats.lock().await;
                    s.record_cleanup(deleted);
                    tracing::info!("[清理] 本次刪除 {} 個舊影片檔案", deleted);
                }

                // 2. 清理舊日誌檔案
                if config.log.to_file && config.log.retention_days > 0 {
                    let log_threshold = std::time::SystemTime::now()
                        - std::time::Duration::from_secs(config.log.retention_days as u64 * 86400);
                    let mut log_deleted = 0u64;

                    if let Ok(entries) = std::fs::read_dir(&config.log.dir) {
                        for entry in entries.flatten() {
                            let path = entry.path();
                            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                            // 只清理 rtsp-recorder 的日誌檔案
                            if !name.starts_with("rtsp-recorder") || !name.ends_with(".log") {
                                continue;
                            }
                            if let Ok(meta) = path.metadata() {
                                if let Ok(modified) = meta.modified() {
                                    if modified < log_threshold {
                                        if std::fs::remove_file(&path).is_ok() {
                                            tracing::info!("[清理] 已刪除舊日誌: {:?}", path.file_name());
                                            log_deleted += 1;
                                        }
                                    }
                                }
                            }
                        }
                    }

                    if log_deleted > 0 {
                        tracing::info!("[清理] 本次刪除 {} 個舊日誌檔案", log_deleted);
                    }
                }
            }
            _ = shutdown.recv() => {
                tracing::info!("[清理] 停止自動清理");
                break;
            }
        }
    }
}

/// P1: 定期保存統計（每 5 分鐘）
async fn background_stats_saver(
    stats: Arc<Mutex<Stats>>,
    output_dir: PathBuf,
    mut shutdown: broadcast::Receiver<()>,
) {
    let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(300));
    
    loop {
        tokio::select! {
            _ = interval.tick() => {
                let s = stats.lock().await;
                if let Err(e) = s.save(&output_dir) {
                    tracing::error!("[統計] 保存失敗: {}", e);
                }
            }
            _ = shutdown.recv() => {
                // 結束前最後保存一次
                let s = stats.lock().await;
                let _ = s.save(&output_dir);
                break;
            }
        }
    }
}

/// 手動錄製一次
pub async fn record_once(config: &Config, duration: u32, streams_filter: &str, ff: &FfmpegPaths) -> Result<()> {
    let streams: Vec<&StreamConfig> = if streams_filter.is_empty() {
        // 空字串 = 全部串流
        config.rtsp.streams.iter().collect()
    } else {
        // 依名稱篩選
        let names: Vec<&str> = streams_filter.split(',').map(|s| s.trim()).collect();
        config.rtsp.streams.iter()
            .filter(|s| names.contains(&s.name.as_str()))
            .collect()
    };

    if streams.is_empty() {
        println!("❌ 找不到符合的串流。可用串流:");
        for s in &config.rtsp.streams {
            println!("   - {}", s.name);
        }
        return Ok(());
    }

    tracing::info!("手動錄製 {}s, {} 個串流", duration, streams.len());

    let output_dir = &config.output.dir;
    std::fs::create_dir_all(output_dir)?;

    let mut handles = vec![];
    for stream in streams {
        let url = stream.url.clone();
        let name = stream.name.clone();
        let output_file = output_dir
            .join(format!("{}_{}.mkv", name, Local::now().format("%Y%m%d_%H%M%S")))
            .to_string_lossy()
            .to_string();
        let ff = ff.clone();
        let dur = duration;

        handles.push(tokio::spawn(async move {
            tracing::info!("[{}] 開始錄製 {}s → {}", name, dur, output_file);

            let status = Command::new(&ff.ffmpeg)
                .args([
                    "-y",
                    "-fflags", "+genpts+discardcorrupt+nobuffer+igndts",
                    "-rtsp_transport", "tcp",
                    "-rtsp_flags", "prefer_tcp",
                    "-use_wallclock_as_timestamps", "1",
                    "-timeout", "10000000",
                    "-i", &url,
                    "-t", &dur.to_string(),
                    "-c:v", "copy",
                    "-c:a", "aac",
                    "-map", "0",
                    &output_file,
                ])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .await;

            match status {
                Ok(s) if s.success() => tracing::info!("[{}] 錄製完成: {}", name, output_file),
                Ok(s) => tracing::error!("[{}] ffmpeg 退出 code={}", name, s.code().unwrap_or(-1)),
                Err(e) => tracing::error!("[{}] ffmpeg 錯誤: {}", name, e),
            }
        }));
    }

    for handle in handles {
        let _ = handle.await;
    }

    tracing::info!("手動錄製完成");
    Ok(())
}
