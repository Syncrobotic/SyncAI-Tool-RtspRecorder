use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing_subscriber::fmt::time::OffsetTime;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

use rtsp_shared::{config, ffmpeg, stats, uploader};

mod recorder;
mod setup;

#[derive(Parser)]
#[command(name = "rtsp-recorder")]
#[command(about = "RTSP 錄製 + 雲端上傳工具", long_about = None)]
struct Cli {
    /// 設定檔路徑
    #[arg(short, long, default_value = "config.yaml")]
    config: PathBuf,

    #[command(subcommand)]
    command: Option<Commands>,

    /// 啟動 daemon 模式
    #[arg(long)]
    daemon: bool,
}

#[derive(Subcommand)]
enum Commands {
    /// 手動錄製
    Record {
        /// 錄製時長（秒）
        #[arg(short, long, default_value = "600")]
        duration: u32,

        /// 串流名稱（逗號分隔，空白=全部）
        #[arg(short, long, default_value = "")]
        streams: String,
    },
    /// 手動上傳
    Upload,
    /// 查看狀態
    Status,
    /// 清理舊檔案
    Cleanup {
        /// 保留小時數
        #[arg(short = 'H', long)]
        hours: Option<u32>,
    },
    /// 安裝 systemd 服務
    InstallService,
    /// 檢查設定與環境
    Check,
    /// 互動式初始設定
    Setup,
    /// 解除安裝（清理服務、錄影、設定）
    Uninstall,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // Setup / Uninstall 不需要事先存在 config，使用簡單的 console 日誌
    if matches!(cli.command, Some(Commands::Setup)) {
        init_console_logging();
        return setup::run_setup_wizard(&cli.config).await;
    }
    if matches!(cli.command, Some(Commands::Uninstall)) {
        init_console_logging();
        return setup::run_uninstall(&cli.config).await;
    }

    // 載入設定（如果 config 不存在，引導使用者執行 setup）
    let config = match config::Config::load(&cli.config) {
        Ok(c) => c,
        Err(e) => {
            init_console_logging();
            if !cli.config.exists() {
                println!("❌ 找不到設定檔: {}", cli.config.display());
                println!();
                println!("首次使用請先執行初始設定：");
                println!("  rtsp-recorder setup");
                return Ok(());
            }
            return Err(e);
        }
    };

    // P0: 設定時區環境變數（確保所有時間相關操作使用正確時區）
    // POSIX TZ 格式符號相反：UTC+8 要設成 "UTC-8"
    let tz = if config.schedule.utc_offset >= 0 {
        format!("UTC-{}", config.schedule.utc_offset)
    } else {
        format!("UTC+{}", -config.schedule.utc_offset)
    };
    std::env::set_var("TZ", &tz);
    
    // 根據設定初始化日誌（支援檔案輸出）
    init_logging(&config);
    tracing::info!("已載入設定檔: {:?}", cli.config);
    tracing::info!("時區: UTC{:+}", config.schedule.utc_offset);

    // P0: 設定 GCS 憑證環境變數（程式啟動時設定一次，避免多執行緒 data race）
    // cloud-storage crate: SERVICE_ACCOUNT = 檔案路徑
    if config.gcs.credentials.exists() {
        let abs_path = std::fs::canonicalize(&config.gcs.credentials)
            .unwrap_or_else(|_| config.gcs.credentials.clone());
        std::env::set_var("SERVICE_ACCOUNT", abs_path.to_string_lossy().as_ref());
        tracing::info!("已載入 GCS 憑證: {:?}", abs_path);
    } else {
        tracing::warn!("GCS 憑證檔案不存在: {:?}", config.gcs.credentials);
    }

    // 載入或建立統計
    let stats = Arc::new(Mutex::new(
        stats::Stats::load(&config.output.dir).unwrap_or_default()
    ));

    // 執行命令
    match cli.command {
        Some(Commands::Record { duration, streams }) => {
            let ff = ffmpeg::ensure_ffmpeg().await?;
            tracing::info!("手動錄製: {}s, 串流 {:?}", duration, streams);
            recorder::record_once(&config, duration, &streams, &ff).await?;
        }
        Some(Commands::Upload) => {
            tracing::info!("手動上傳");
            uploader::upload_all(&config, Some(stats.clone())).await?;
        }
        Some(Commands::Status) => {
            let s = stats.lock().await;
            s.display();
        }
        Some(Commands::Cleanup { hours }) => {
            let h = hours.unwrap_or(config.retention.max_hours);
            cleanup_old_files(&config, h, stats.clone()).await?;
        }
        Some(Commands::InstallService) => {
            install_systemd_service(&cli.config, &config)?;
        }
        Some(Commands::Check) => {
            check_environment(&config, &cli.config).await?;
        }
        Some(Commands::Setup) => unreachable!(), // 已在上方處理
        Some(Commands::Uninstall) => unreachable!(), // 已在上方處理
        None => {
            if cli.daemon {
                let ff = ffmpeg::ensure_ffmpeg().await?;
                // 啟動前驗證 GCS bucket
                if let Err(e) = uploader::verify_bucket(&config.gcs.bucket).await {
                    tracing::warn!("{}", e);
                    tracing::warn!("GCS 上傳功能可能無法使用，但錄製仍會繼續");
                }
                tracing::info!("啟動 daemon 模式");
                recorder::run_daemon(&config, stats, &ff).await?;
            } else {
                println!("使用 --help 查看說明");
            }
        }
    }

    Ok(())
}

/// 檢查設定與環境
async fn check_environment(config: &config::Config, config_path: &PathBuf) -> Result<()> {
    println!("=== RTSP Recorder 環境檢查 ===");
    println!();

    // 1. 設定檔
    println!("📄 設定檔: {:?}", config_path);
    println!("   ✅ 格式正確，已成功載入");
    println!();

    // 2. RTSP
    println!("📡 RTSP 串流:");
    if config.rtsp.streams.is_empty() {
        println!("   ⚠️  尚未設定任何串流");
    } else {
        for (i, s) in config.rtsp.streams.iter().enumerate() {
            println!("   [{}] {} → {}", i + 1, s.name, s.url);
        }
    }
    println!("   分段長度:  {}s ({}min)", config.rtsp.segment_duration, config.rtsp.segment_duration / 60);
    println!();

    // 3. 排程
    println!("⏰ 錄製排程:");
    if config.schedule.record_start_hour < config.schedule.record_end_hour {
        println!("   {:02}:00 - {:02}:00", config.schedule.record_start_hour, config.schedule.record_end_hour);
    } else {
        println!("   {:02}:00 - 隔日 {:02}:00（跨日）", config.schedule.record_start_hour, config.schedule.record_end_hour);
    }
    println!();

    // 4. 輸出目錄
    println!("📂 輸出目錄: {:?}", config.output.dir);
    if config.output.dir.exists() {
        println!("   ✅ 目錄存在");
    } else {
        println!("   ⚠️  目錄不存在（啟動時會自動建立）");
    }
    println!("   解析度:    {}", if config.output.resolution.is_empty() || config.output.resolution == "original" { "原始大小" } else { &config.output.resolution });
    println!();

    // 5. GCS
    println!("☁️  GCS 上傳:");
    println!("   Bucket:     {}", config.gcs.bucket);
    println!("   Prefix:     {}", if config.gcs.prefix.is_empty() { "(無)" } else { &config.gcs.prefix });
    if config.gcs.credentials.exists() {
        println!("   憑證:       ✅ {:?}", config.gcs.credentials);
    } else {
        println!("   憑證:       ❌ {:?} (找不到檔案)", config.gcs.credentials);
    }
    println!();

    // 6. ffmpeg (自動偵測或下載)
    println!("🎬 ffmpeg:");
    match ffmpeg::ensure_ffmpeg().await {
        Ok(paths) => {
            if let Ok(output) = std::process::Command::new(&paths.ffmpeg).arg("-version").output() {
                let version = String::from_utf8_lossy(&output.stdout);
                let first_line = version.lines().next().unwrap_or("unknown");
                println!("   ffmpeg:  ✅ {}", first_line);
            }
            if let Ok(output) = std::process::Command::new(&paths.ffprobe).arg("-version").output() {
                let version = String::from_utf8_lossy(&output.stdout);
                let first_line = version.lines().next().unwrap_or("unknown");
                println!("   ffprobe: ✅ {}", first_line);
            }
            let display_path = if paths.ffmpeg.components().count() <= 1 {
                "系統 PATH".to_string()
            } else {
                format!("{:?}", paths.ffmpeg.parent().unwrap_or(paths.ffmpeg.as_path()))
            };
            println!("   來源: {}", display_path);
        }
        Err(e) => {
            println!("   ❌ ffmpeg 無法取得: {}", e);
        }
    }
    println!();

    // 8. 保留時間
    println!("🗑️  檔案保留: {} 小時", config.retention.max_hours);
    println!();

    // 9. systemd 狀態 (Linux)
    #[cfg(target_os = "linux")]
    {
        println!("🔧 systemd:");
        match std::process::Command::new("systemctl")
            .args(["is-active", "rtsp-recorder"])
            .output()
        {
            Ok(output) => {
                let status = String::from_utf8_lossy(&output.stdout).trim().to_string();
                match status.as_str() {
                    "active" => println!("   ✅ 服務運行中"),
                    "inactive" => println!("   ⚠️  服務已停止"),
                    _ => println!("   ℹ️  狀態: {}", status),
                }
            }
            Err(_) => println!("   ⚠️  未安裝為 systemd 服務"),
        }
        println!();
    }

    println!("=== 檢查完成 ===");
    Ok(())
}

/// 清理舊檔案
async fn cleanup_old_files(
    config: &config::Config,
    hours: u32,
    stats: Arc<Mutex<stats::Stats>>,
) -> Result<()> {
    use std::io::{self, BufRead, Write};
    use std::time::{Duration, SystemTime};

    let output_dir = &config.output.dir;
    if !output_dir.exists() {
        println!("輸出目錄不存在");
        return Ok(());
    }

    let threshold = SystemTime::now() - Duration::from_secs(hours as u64 * 3600);

    // 先掃描要刪除的檔案
    let mut to_delete: Vec<std::path::PathBuf> = Vec::new();
    for entry in std::fs::read_dir(output_dir)? {
        let entry = entry?;
        let path = entry.path();

        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        if !["mp4", "mkv"].contains(&ext) {
            continue;
        }

        if let Ok(metadata) = entry.metadata() {
            if let Ok(modified) = metadata.modified() {
                if modified < threshold {
                    to_delete.push(path);
                }
            }
        }
    }

    if to_delete.is_empty() {
        println!("沒有超過 {} 小時的舊檔案", hours);
        return Ok(());
    }

    // 列出並提醒
    println!("⚠️  以下 {} 個檔案將被刪除（超過 {} 小時）：", to_delete.len(), hours);
    println!();
    for (i, path) in to_delete.iter().enumerate() {
        if i < 10 {
            println!("   {:?}", path.file_name().unwrap_or_default());
        } else if i == 10 {
            println!("   ... 還有 {} 個", to_delete.len() - 10);
            break;
        }
    }
    println!();
    println!("⚠️  請確認這些檔案已上傳到 GCS，刪除後無法復原！");
    print!("確定要刪除嗎？[y/N]: ");
    io::stdout().flush()?;

    let mut input = String::new();
    io::stdin().lock().read_line(&mut input)?;
    if !input.trim().to_lowercase().starts_with('y') {
        println!("已取消");
        return Ok(());
    }

    // 執行刪除
    let mut deleted = 0;
    for path in &to_delete {
        if let Err(e) = std::fs::remove_file(path) {
            tracing::error!("刪除失敗: {:?} - {}", path.file_name(), e);
        } else {
            tracing::info!("已刪除: {:?}", path.file_name());
            deleted += 1;
        }
    }

    {
        let mut s = stats.lock().await;
        s.record_cleanup(deleted);
        s.save(output_dir)?;
    }

    println!("清理完成: 刪除 {} 個檔案", deleted);
    Ok(())
}

/// 安裝 systemd 服務
fn install_systemd_service(config_path: &PathBuf, config: &config::Config) -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        let exe_path = std::env::current_exe()?;
        let config_abs = std::fs::canonicalize(config_path)?;
        
        // 使用 SUDO_USER 取得原本的使用者，避免 sudo 執行時變成 root
        let user = std::env::var("SUDO_USER")
            .or_else(|_| std::env::var("USER"))
            .unwrap_or_else(|_| "root".to_string());

        // 取得 GCS 認證檔案的絕對路徑
        let gcs_credentials = std::fs::canonicalize(&config.gcs.credentials)
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| config.gcs.credentials.display().to_string());

        let service_content = format!(
            r#"[Unit]
Description=RTSP Recorder - 監控錄製服務
After=network.target

[Service]
Type=simple
User={user}
Environment="GOOGLE_APPLICATION_CREDENTIALS={gcs_credentials}"
ExecStart={exe} --config {config} --daemon
Restart=always
RestartSec=10
StandardOutput=journal
StandardError=journal

[Install]
WantedBy=multi-user.target
"#,
            user = user,
            gcs_credentials = gcs_credentials,
            exe = exe_path.display(),
            config = config_abs.display(),
        );

        let service_path = "/etc/systemd/system/rtsp-recorder.service";
        
        println!("將建立 systemd 服務: {}", service_path);
        println!();
        println!("服務內容:");
        println!("{}", service_content);
        println!();

        // 檢查是否有 root 權限（透過嘗試寫入來判斷）
        match std::fs::write(service_path, &service_content) {
            Ok(_) => {
                println!("✅ 服務檔案已建立: {}", service_path);

                // 重新載入 systemd
                std::process::Command::new("systemctl")
                    .args(["daemon-reload"])
                    .status()?;

                println!();
                println!("使用以下命令管理服務:");
                println!("  sudo systemctl enable rtsp-recorder  # 開機自動啟動");
                println!("  sudo systemctl start rtsp-recorder   # 立即啟動");
                println!("  sudo systemctl status rtsp-recorder  # 查看狀態");
                println!("  sudo systemctl stop rtsp-recorder    # 停止");
                println!("  journalctl -u rtsp-recorder -f       # 查看日誌");
            }
            Err(_) => {
                println!("需要 root 權限，請執行:");
                println!();
                println!("sudo {} --config {} install-service", exe_path.display(), config_path.display());
            }
        }
    }

    #[cfg(not(target_os = "linux"))]
    {
        let _ = config;
        println!("systemd 服務僅支援 Linux 系統");
        println!();
        println!("macOS 建議使用 launchd，或直接執行:");
        println!("  nohup {} --config {} --daemon &", 
            std::env::current_exe()?.display(), 
            config_path.display()
        );
    }

    Ok(())
}

/// 取得本地時間 offset（預設 UTC+8）
fn get_local_time_offset() -> time::UtcOffset {
    // 嘗試取得系統本地時間 offset，失敗時預設 UTC+8
    time::UtcOffset::current_local_offset().unwrap_or(time::UtcOffset::from_hms(8, 0, 0).unwrap())
}

/// 初始化 console 日誌（用於 setup/uninstall 等不需要 config 的命令）
fn init_console_logging() {
    let timer = OffsetTime::new(
        get_local_time_offset(),
        time::macros::format_description!("[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:3]"),
    );
    tracing_subscriber::fmt()
        .with_timer(timer)
        .with_env_filter("rtsp_recorder=info,rtsp_shared=info")
        .init();
}

/// 根據設定初始化日誌（支援檔案輸出）
fn init_logging(config: &config::Config) {
    let env_filter = tracing_subscriber::EnvFilter::new("rtsp_recorder=info,rtsp_shared=info");
    let timer = OffsetTime::new(
        get_local_time_offset(),
        time::macros::format_description!("[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:3]"),
    );
    let fmt_layer = tracing_subscriber::fmt::layer().with_timer(timer.clone());

    if config.log.to_file {
        // 建立日誌目錄
        if let Err(e) = std::fs::create_dir_all(&config.log.dir) {
            eprintln!("無法建立日誌目錄 {:?}: {}", config.log.dir, e);
            // 退回到只用 console
            tracing_subscriber::registry()
                .with(env_filter)
                .with(fmt_layer)
                .init();
            return;
        }

        // 根據 rotation 設定建立 file appender
        let file_appender = match config.log.rotation.as_str() {
            "time" | "daily" => tracing_appender::rolling::daily(&config.log.dir, "rtsp-recorder.log"),
            "hourly" => tracing_appender::rolling::hourly(&config.log.dir, "rtsp-recorder.log"),
            _ => tracing_appender::rolling::daily(&config.log.dir, "rtsp-recorder.log"),
        };

        let (non_blocking, _guard) = tracing_appender::non_blocking(file_appender);
        
        // 保持 guard 存活（使用 Box::leak 避免 guard 被 drop）
        Box::leak(Box::new(_guard));

        let file_layer = tracing_subscriber::fmt::layer()
            .with_timer(timer)
            .with_writer(non_blocking)
            .with_ansi(false);  // 檔案不需要 ANSI 顏色碼

        tracing_subscriber::registry()
            .with(env_filter)
            .with(fmt_layer)
            .with(file_layer)
            .init();

        tracing::info!("日誌檔案: {:?}", config.log.dir);
    } else {
        tracing_subscriber::registry()
            .with(env_filter)
            .with(fmt_layer)
            .init();
    }
}
