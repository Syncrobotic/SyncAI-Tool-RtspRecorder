use anyhow::Result;
use std::path::Path;
use std::process::Stdio;
use tokio::process::Command;

/// 偵測影片編碼格式
pub async fn detect_video_codec(file_path: &Path) -> Option<String> {
    let output = Command::new("ffprobe")
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
pub async fn convert_to_mp4(mkv_path: &Path) -> Result<Option<std::path::PathBuf>> {
    if !mkv_path.exists() {
        return Ok(None);
    }

    let mp4_path = mkv_path.with_extension("mp4");
    let codec = detect_video_codec(mkv_path).await;

    if let Some(ref c) = codec {
        tracing::info!("[轉檔] 偵測到 {} 編碼: {:?}", c.to_uppercase(), mkv_path.file_name());
    }

    let is_hevc = matches!(codec.as_deref(), Some("hevc") | Some("h265"));
    let is_mjpeg = matches!(codec.as_deref(), Some("mjpeg") | Some("mjpg"));

    // MJPEG 必須重編碼
    if !is_mjpeg {
        // 嘗試直接複製
        let mut args = vec![
            "-y",
            "-fflags", "+genpts+igndts+discardcorrupt",
            "-i", mkv_path.to_str().unwrap(),
            "-c:v", "copy",
            "-c:a", "aac",
            "-b:a", "128k",
            "-ar", "44100",
            "-movflags", "+faststart",
            "-avoid_negative_ts", "make_zero",
        ];

        if is_hevc {
            args.extend(["-tag:v", "hvc1"]);
        }

        args.push(mp4_path.to_str().unwrap());

        let status = Command::new("ffmpeg")
            .args(&args)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await?;

        if status.success() && mp4_path.exists() && mp4_path.metadata()?.len() > 1000 {
            std::fs::remove_file(mkv_path)?;
            return Ok(Some(mp4_path));
        }

        tracing::warn!("[轉檔] copy 模式失敗，重新編碼: {:?}", mkv_path.file_name());
    }

    // 重新編碼為 H.264
    let status = Command::new("ffmpeg")
        .args([
            "-y",
            "-fflags", "+genpts+igndts+discardcorrupt",
            "-i", mkv_path.to_str().unwrap(),
            "-c:v", "libx264",
            "-preset", "veryfast",
            "-crf", "20",
            "-pix_fmt", "yuv420p",
            "-c:a", "aac",
            "-b:a", "128k",
            "-ar", "44100",
            "-movflags", "+faststart",
            "-avoid_negative_ts", "make_zero",
            mp4_path.to_str().unwrap(),
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await?;

    if status.success() && mp4_path.exists() && mp4_path.metadata()?.len() > 1000 {
        std::fs::remove_file(mkv_path)?;
        return Ok(Some(mp4_path));
    }

    Ok(None)
}
