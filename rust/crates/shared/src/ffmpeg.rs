use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};

/// ffmpeg/ffprobe 二進制路徑
#[derive(Debug, Clone)]
pub struct FfmpegPaths {
    pub ffmpeg: PathBuf,
    pub ffprobe: PathBuf,
}

/// 取得 ffmpeg 路徑（自動偵測或下載）
pub async fn ensure_ffmpeg() -> Result<FfmpegPaths> {
    // 1. 檢查程式旁邊的 ffmpeg 目錄
    if let Some(paths) = find_local_ffmpeg() {
        tracing::info!("使用本地 ffmpeg: {:?}", paths.ffmpeg);
        return Ok(paths);
    }

    // 2. 檢查系統 PATH
    if let Some(paths) = find_system_ffmpeg() {
        tracing::info!("使用系統 ffmpeg: {:?}", paths.ffmpeg);
        return Ok(paths);
    }

    // 3. 自動下載
    tracing::info!("系統未偵測到 ffmpeg，開始自動下載...");
    let paths = download_ffmpeg().await?;
    tracing::info!("ffmpeg 下載完成: {:?}", paths.ffmpeg);
    Ok(paths)
}

/// 取得 ffmpeg 所在的本地目錄（程式旁邊）
fn ffmpeg_local_dir() -> PathBuf {
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|p| p.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."));
    exe_dir.join("ffmpeg-bin")
}

/// 在程式旁邊尋找 ffmpeg
fn find_local_ffmpeg() -> Option<FfmpegPaths> {
    let dir = ffmpeg_local_dir();
    let (ffmpeg, ffprobe) = binary_names();
    let ffmpeg_path = dir.join(&ffmpeg);
    let ffprobe_path = dir.join(&ffprobe);

    if ffmpeg_path.exists() && ffprobe_path.exists() {
        Some(FfmpegPaths {
            ffmpeg: ffmpeg_path,
            ffprobe: ffprobe_path,
        })
    } else {
        None
    }
}

/// 在系統 PATH 中尋找 ffmpeg
fn find_system_ffmpeg() -> Option<FfmpegPaths> {
    let ffmpeg_ok = std::process::Command::new("ffmpeg")
        .arg("-version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok();

    let ffprobe_ok = std::process::Command::new("ffprobe")
        .arg("-version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok();

    if ffmpeg_ok && ffprobe_ok {
        Some(FfmpegPaths {
            ffmpeg: PathBuf::from("ffmpeg"),
            ffprobe: PathBuf::from("ffprobe"),
        })
    } else {
        None
    }
}

/// 根據平台決定下載 URL 和二進制名稱
fn download_info() -> Result<(&'static str, &'static str)> {
    // 使用 BtbN 的 FFmpeg 靜態建構
    // https://github.com/BtbN/FFmpeg-Builds/releases
    let (url, archive) = if cfg!(target_os = "windows") && cfg!(target_arch = "x86_64") {
        (
            "https://github.com/BtbN/FFmpeg-Builds/releases/download/latest/ffmpeg-master-latest-win64-gpl.zip",
            "zip",
        )
    } else if cfg!(target_os = "linux") && cfg!(target_arch = "x86_64") {
        (
            "https://github.com/BtbN/FFmpeg-Builds/releases/download/latest/ffmpeg-master-latest-linux64-gpl.tar.xz",
            "tar.xz",
        )
    } else if cfg!(target_os = "linux") && cfg!(target_arch = "aarch64") {
        (
            "https://github.com/BtbN/FFmpeg-Builds/releases/download/latest/ffmpeg-master-latest-linuxarm64-gpl.tar.xz",
            "tar.xz",
        )
    } else {
        bail!(
            "不支援自動下載 ffmpeg 於此平台 ({}/{})\n請手動安裝: https://ffmpeg.org/download.html",
            std::env::consts::OS,
            std::env::consts::ARCH
        );
    };

    Ok((url, archive))
}

/// 二進制檔名
fn binary_names() -> (String, String) {
    if cfg!(target_os = "windows") {
        ("ffmpeg.exe".to_string(), "ffprobe.exe".to_string())
    } else {
        ("ffmpeg".to_string(), "ffprobe".to_string())
    }
}

/// 下載並解壓 ffmpeg
async fn download_ffmpeg() -> Result<FfmpegPaths> {
    let (url, archive_type) = download_info()?;
    let dest_dir = ffmpeg_local_dir();
    std::fs::create_dir_all(&dest_dir)?;

    let tmp_dir = dest_dir.join("_tmp_download");
    std::fs::create_dir_all(&tmp_dir)?;

    let archive_name = if archive_type == "zip" {
        "ffmpeg.zip"
    } else {
        "ffmpeg.tar.xz"
    };
    let archive_path = tmp_dir.join(archive_name);

    // 下載
    println!("📥 下載 ffmpeg...");
    println!("   來源: {}", url);

    download_file(url, &archive_path).await?;

    let file_size = std::fs::metadata(&archive_path)?.len();
    println!(
        "   下載完成 ({:.1} MB)",
        file_size as f64 / 1024.0 / 1024.0
    );

    // 解壓
    println!("📦 解壓縮...");
    let extract_result = extract_ffmpeg(&archive_path, archive_type, &tmp_dir, &dest_dir);

    // 無論成功失敗都清理暫存目錄
    let _ = std::fs::remove_dir_all(&tmp_dir);

    extract_result?;

    // 驗證
    let paths = find_local_ffmpeg().context("下載後找不到 ffmpeg 二進制檔")?;

    // 確認可執行
    let output = std::process::Command::new(&paths.ffmpeg)
        .arg("-version")
        .output()
        .context("ffmpeg 無法執行")?;

    let version = String::from_utf8_lossy(&output.stdout);
    let first_line = version.lines().next().unwrap_or("unknown");
    println!("✅ {}", first_line);

    Ok(paths)
}

/// 使用系統工具下載檔案
async fn download_file(url: &str, dest: &Path) -> Result<()> {
    // 優先用 curl（大部分系統都有）
    let status = tokio::process::Command::new("curl")
        .args(["-L", "-f", "-o"])
        .arg(dest)
        .arg(url)
        .arg("--progress-bar")
        .status()
        .await;

    match status {
        Ok(s) if s.success() => return Ok(()),
        _ => {}
    }

    // fallback: wget
    let status = tokio::process::Command::new("wget")
        .args(["-q", "--show-progress", "-O"])
        .arg(dest)
        .arg(url)
        .status()
        .await;

    match status {
        Ok(s) if s.success() => Ok(()),
        _ => bail!("下載失敗，請確認網路連線，或手動安裝 ffmpeg"),
    }
}

/// 解壓並複製 ffmpeg/ffprobe 到目標目錄
fn extract_ffmpeg(
    archive_path: &Path,
    archive_type: &str,
    tmp_dir: &Path,
    dest_dir: &Path,
) -> Result<()> {
    let (ffmpeg_name, ffprobe_name) = binary_names();

    if archive_type == "zip" {
        // Windows: 用 PowerShell 解壓
        let status = std::process::Command::new("powershell")
            .args([
                "-NoProfile",
                "-Command",
                &format!(
                    "Expand-Archive -Path '{}' -DestinationPath '{}' -Force",
                    archive_path.display(),
                    tmp_dir.display()
                ),
            ])
            .status()
            .context("解壓縮失敗")?;

        if !status.success() {
            bail!("zip 解壓縮失敗");
        }
    } else {
        // Linux: tar xf
        let status = std::process::Command::new("tar")
            .args(["xf"])
            .arg(archive_path)
            .arg("-C")
            .arg(tmp_dir)
            .status()
            .context("解壓縮失敗")?;

        if !status.success() {
            bail!("tar 解壓縮失敗");
        }
    }

    // 在解壓目錄中尋找 ffmpeg 和 ffprobe
    let ffmpeg_src = find_file_recursive(tmp_dir, &ffmpeg_name)
        .context("解壓後找不到 ffmpeg")?;
    let ffprobe_src = find_file_recursive(tmp_dir, &ffprobe_name)
        .context("解壓後找不到 ffprobe")?;

    // 複製到目標目錄
    std::fs::copy(&ffmpeg_src, dest_dir.join(&ffmpeg_name))?;
    std::fs::copy(&ffprobe_src, dest_dir.join(&ffprobe_name))?;

    // Linux: 加執行權限
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o755);
        std::fs::set_permissions(dest_dir.join(&ffmpeg_name), perms.clone())?;
        std::fs::set_permissions(dest_dir.join(&ffprobe_name), perms)?;
    }

    Ok(())
}

/// 遞迴搜尋檔案
fn find_file_recursive(dir: &Path, name: &str) -> Option<PathBuf> {
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file() && path.file_name().map(|n| n == name).unwrap_or(false) {
                return Some(path);
            }
            if path.is_dir() {
                if let Some(found) = find_file_recursive(&path, name) {
                    return Some(found);
                }
            }
        }
    }
    None
}
