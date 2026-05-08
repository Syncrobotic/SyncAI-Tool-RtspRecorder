use anyhow::Result;
use std::path::Path;
use std::process::Stdio;
use tokio::process::Command;

use crate::ffmpeg::FfmpegPaths;

/// 偵測影片編碼格式
pub async fn detect_video_codec(file_path: &Path, ff: &FfmpegPaths) -> Option<String> {
    let output = Command::new(&ff.ffprobe)
        .args([
            "-v", "error",
            "-select_streams", "v:0",
            "-show_entries", "stream=codec_name",
            "-of", "default=noprint_wrappers=1:nokey=1",
        ])
        .arg(file_path)
        .output()
        .await
        .ok()?;

    if output.status.success() {
        let codec = String::from_utf8_lossy(&output.stdout)
            .trim()
            .to_lowercase();
        Some(codec)
    } else {
        None
    }
}

/// 轉檔 MKV -> MP4
/// resolution: 例如 "1920x1080"，空字串或 "original" 表示不縮放
/// force_reencode: 強制重新編碼（確保最大播放兼容性）
pub async fn convert_to_mp4(mkv_path: &Path, ff: &FfmpegPaths, resolution: &str, force_reencode: bool) -> Result<Option<std::path::PathBuf>> {
    if !mkv_path.exists() {
        return Ok(None);
    }

    let mp4_path = mkv_path.with_extension("mp4");
    let codec = detect_video_codec(mkv_path, ff).await;

    if let Some(ref c) = codec {
        tracing::info!("[轉檔] 偵測到 {} 編碼: {:?}", c.to_uppercase(), mkv_path.file_name());
    }

    let is_hevc = matches!(codec.as_deref(), Some("hevc") | Some("h265"));
    let is_mjpeg = matches!(codec.as_deref(), Some("mjpeg") | Some("mjpg"));

    let mkv_str = mkv_path.to_str().ok_or_else(|| anyhow::anyhow!("路徑含無效 UTF-8: {:?}", mkv_path))?;
    let mp4_str = mp4_path.to_str().ok_or_else(|| anyhow::anyhow!("路徑含無效 UTF-8: {:?}", mp4_path))?;

    // H.264 可以直接複製；HEVC/MJPEG/force_reencode 需要重新編碼為 H.264 以確保播放相容性
    if !is_mjpeg && !is_hevc && !force_reencode {
        // 嘗試直接複製（僅限 H.264 等通用編碼）
        let args = vec![
            "-y",
            "-fflags", "+genpts+igndts+discardcorrupt",
            "-i", mkv_str,
            "-c:v", "copy",
            "-c:a", "aac",
            "-b:a", "128k",
            "-ar", "44100",
            "-movflags", "+faststart",
            "-brand", "mp42",  // 增加 Mac/Linux 播放器兼容性
            "-avoid_negative_ts", "make_zero",
            mp4_str,
        ];

        let status = Command::new(&ff.ffmpeg)
            .args(&args)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await?;

        if status.success() && mp4_path.exists() && mp4_path.metadata()?.len() > 1000 {
            fix_file_permissions(&mp4_path);
            std::fs::remove_file(mkv_path)?;
            return Ok(Some(mp4_path));
        }

        tracing::warn!("[轉檔] copy 模式失敗，重新編碼: {:?}", mkv_path.file_name());
    }

    // 重新編碼為 H.264
    let need_scale = !resolution.is_empty() && resolution != "original";
    let scale_filter = if need_scale {
        // 解析 "1920x1080" → "scale=1920:1080"
        let dims = resolution.replace('x', ":");
        format!("scale={}", dims)
    } else {
        String::new()
    };

    let mut args = vec![
        "-y".to_string(),
        "-fflags".to_string(), "+genpts+igndts+discardcorrupt".to_string(),
        "-i".to_string(), mkv_str.to_string(),
        "-c:v".to_string(), "libx264".to_string(),
        "-preset".to_string(), "veryfast".to_string(),
        "-crf".to_string(), "20".to_string(),
        "-pix_fmt".to_string(), "yuv420p".to_string(),
    ];

    if need_scale {
        args.extend(["-vf".to_string(), scale_filter]);
    }

    args.extend([
        "-c:a".to_string(), "aac".to_string(),
        "-b:a".to_string(), "128k".to_string(),
        "-ar".to_string(), "44100".to_string(),
        "-movflags".to_string(), "+faststart".to_string(),
        "-brand".to_string(), "mp42".to_string(),  // 增加 Mac/Linux 播放器兼容性
        "-avoid_negative_ts".to_string(), "make_zero".to_string(),
        mp4_str.to_string(),
    ]);

    let status = Command::new(&ff.ffmpeg)
        .args(&args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await?;

    if status.success() && mp4_path.exists() && mp4_path.metadata()?.len() > 1000 {
        fix_file_permissions(&mp4_path);
        std::fs::remove_file(mkv_path)?;
        return Ok(Some(mp4_path));
    }

    // 清理失敗的殘留 MP4
    let _ = std::fs::remove_file(&mp4_path);
    Ok(None)
}

/// 設定檔案權限為 0644（解決 root 錄製 + 一般使用者上傳的問題）
#[cfg(unix)]
fn fix_file_permissions(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o644));
}

#[cfg(not(unix))]
fn fix_file_permissions(_path: &Path) {}
