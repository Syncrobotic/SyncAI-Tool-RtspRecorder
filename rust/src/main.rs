use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;

mod config;
mod converter;
mod recorder;
mod stats;
mod uploader;

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

        /// 頻道範圍，例如 1-10
        #[arg(short, long, default_value = "1-10")]
        channels: String,
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
}

#[tokio::main]
async fn main() -> Result<()> {
    // 初始化日誌
    tracing_subscriber::fmt()
        .with_env_filter("rtsp_recorder=info")
        .init();

    let cli = Cli::parse();

    // 載入設定
    let config = config::Config::load(&cli.config)?;
    tracing::info!("已載入設定檔: {:?}", cli.config);

    // 載入或建立統計
    let stats = Arc::new(Mutex::new(
        stats::Stats::load(&config.output.dir).unwrap_or_default()
    ));

    // 執行命令
    match cli.command {
        Some(Commands::Record { duration, channels }) => {
            tracing::info!("手動錄製: {}s, 頻道 {}", duration, channels);
            recorder::record_once(&config, duration, &channels).await?;
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
            install_systemd_service(&cli.config)?;
        }
        None => {
            if cli.daemon {
                tracing::info!("啟動 daemon 模式");
                recorder::run_daemon(&config, stats).await?;
            } else {
                println!("使用 --help 查看說明");
            }
        }
    }

    Ok(())
}

/// 清理舊檔案
async fn cleanup_old_files(
    config: &config::Config,
    hours: u32,
    stats: Arc<Mutex<stats::Stats>>,
) -> Result<()> {
    use std::time::{Duration, SystemTime};

    let output_dir = &config.output.dir;
    if !output_dir.exists() {
        println!("輸出目錄不存在");
        return Ok(());
    }

    let threshold = SystemTime::now() - Duration::from_secs(hours as u64 * 3600);
    let mut deleted = 0;

    for entry in std::fs::read_dir(output_dir)? {
        let entry = entry?;
        let path = entry.path();

        // 只處理影片檔案
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        if !["mp4", "mkv"].contains(&ext) {
            continue;
        }

        if let Ok(metadata) = path.metadata() {
            if let Ok(modified) = metadata.modified() {
                if modified < threshold {
                    if let Err(e) = std::fs::remove_file(&path) {
                        tracing::error!("刪除失敗: {:?} - {}", path.file_name(), e);
                    } else {
                        tracing::info!("已刪除: {:?}", path.file_name());
                        deleted += 1;
                    }
                }
            }
        }
    }

    {
        let mut s = stats.lock().await;
        s.record_cleanup(deleted);
        s.save(output_dir)?;
    }

    println!("清理完成: 刪除 {} 個檔案（保留 {} 小時內）", deleted, hours);
    Ok(())
}

/// 安裝 systemd 服務
fn install_systemd_service(config_path: &PathBuf) -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        let exe_path = std::env::current_exe()?;
        let config_abs = std::fs::canonicalize(config_path)?;
        let user = std::env::var("USER").unwrap_or_else(|_| "root".to_string());

        let service_content = format!(
            r#"[Unit]
Description=RTSP Recorder - 監控錄製服務
After=network.target

[Service]
Type=simple
User={user}
ExecStart={exe} --config {config} --daemon
Restart=always
RestartSec=10
StandardOutput=journal
StandardError=journal

[Install]
WantedBy=multi-user.target
"#,
            user = user,
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
