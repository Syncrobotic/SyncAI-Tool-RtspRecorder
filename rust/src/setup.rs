use anyhow::Result;
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};

/// 互動式設定精靈 — 引導使用者完成首次設定
pub async fn run_setup_wizard(config_path: &Path) -> Result<()> {
    println!("╔══════════════════════════════════════╗");
    println!("║   RTSP Recorder — 初始設定精靈       ║");
    println!("╚══════════════════════════════════════╝");
    println!();

    // ── Step 1: ffmpeg ──
    println!("【1/4】檢查 ffmpeg ...");
    let ff = crate::ffmpeg::ensure_ffmpeg().await?;
    if let Ok(output) = std::process::Command::new(&ff.ffmpeg).arg("-version").output() {
        let ver = String::from_utf8_lossy(&output.stdout);
        let first = ver.lines().next().unwrap_or("unknown");
        println!("   ✅ {}", first);
    }
    println!();

    // ── Step 2: config.yaml ──
    println!("【2/4】設定檔 ...");
    if config_path.exists() {
        println!("   ✅ {} 已存在", config_path.display());
        let modify = ask_yn("   是否重新設定？", false)?;
        if modify {
            generate_config(config_path)?;
        }
    } else {
        println!("   ⚠️  找不到 {}", config_path.display());
        generate_config(config_path)?;
    }
    println!();

    // ── Step 3: GCS 憑證 ──
    println!("【3/4】GCS 憑證 ...");
    // 從剛建立/既有的 config 中讀取憑證路徑
    let config = crate::config::Config::load(config_path)?;
    if config.gcs.credentials.exists() {
        println!("   ✅ {} 已存在", config.gcs.credentials.display());
    } else {
        println!("   ⚠️  找不到 {}", config.gcs.credentials.display());
        println!();
        println!("   請將 GCS Service Account JSON 憑證放到以下路徑：");
        println!("   → {}", config.gcs.credentials.display());
        println!();
        println!("   取得方式：");
        println!("   1. GCP Console → IAM → Service Accounts");
        println!("   2. 建立金鑰 → JSON 格式");
        println!("   3. 下載後放到上述路徑");
        println!();

        // 建立範例檔案
        let example_path = config.gcs.credentials.with_extension("example.json");
        if !example_path.exists() {
            let example = serde_json::json!({
                "type": "service_account",
                "project_id": "your-project-id",
                "private_key_id": "your-private-key-id",
                "private_key": "-----BEGIN PRIVATE KEY-----\nYOUR_PRIVATE_KEY_HERE\n-----END PRIVATE KEY-----\n",
                "client_email": "your-sa@your-project-id.iam.gserviceaccount.com",
                "client_id": "123456789",
                "auth_uri": "https://accounts.google.com/o/oauth2/auth",
                "token_uri": "https://oauth2.googleapis.com/token",
                "auth_provider_x509_cert_url": "https://www.googleapis.com/oauth2/v1/certs",
                "client_x509_cert_url": "https://www.googleapis.com/robot/v1/metadata/x509/your-sa",
                "universe_domain": "googleapis.com"
            });
            std::fs::write(&example_path, serde_json::to_string_pretty(&example)?)?;
            println!("   已建立範例檔: {}", example_path.display());
        }

        println!("   放好後按 Enter 繼續（或直接按 Enter 跳過）...");
        let _ = read_line();

        if config.gcs.credentials.exists() {
            println!("   ✅ 憑證已就位");
        } else {
            println!("   ⏭️  先跳過，之後可手動放置");
        }
    }
    println!();

    // ── Step 4: 系統服務 ──
    println!("【4/4】系統服務 ...");
    setup_system_service(config_path)?;
    println!();

    // ── 完成 ──
    println!("╔══════════════════════════════════════╗");
    println!("║         ✅ 設定完成！                 ║");
    println!("╚══════════════════════════════════════╝");
    println!();
    println!("接下來你可以：");
    println!("  檢查環境:  rtsp-recorder check");
    println!("  啟動錄製:  rtsp-recorder --daemon");
    println!("  手動錄製:  rtsp-recorder record -d 60");
    println!("  查看狀態:  rtsp-recorder status");

    Ok(())
}

/// 互動式產生 config.yaml
fn generate_config(config_path: &Path) -> Result<()> {
    println!();
    println!("   --- 建立設定檔 ---");

    // RTSP
    let base_url = ask_with_default(
        "   RTSP Base URL",
        "rtsp://admin:password@192.168.1.100:554",
    )?;

    let channels_str = ask_with_default("   錄製頻道 (如: 1,2,3 或 1-10)", "1-10")?;
    let channels = parse_channels(&channels_str);

    let segment = ask_with_default("   分段長度 (秒)", "600")?;
    let segment_duration: u32 = segment.parse().unwrap_or(600);

    // 排程
    let start_hour = ask_with_default("   錄製開始時間 (24h)", "8")?;
    let start_h: u32 = start_hour.parse().unwrap_or(8);
    let end_hour = ask_with_default("   錄製結束時間 (24h)", "1")?;
    let end_h: u32 = end_hour.parse().unwrap_or(1);

    // 輸出
    let default_dir = dirs::home_dir()
        .map(|h| h.join("rtsp-recorder/videos").display().to_string())
        .unwrap_or_else(|| "./videos".to_string());
    let output_dir = ask_with_default("   輸出目錄", &default_dir)?;

    // GCS
    let bucket = ask_with_default("   GCS Bucket 名稱", "my-rtsp-recordings")?;
    let prefix = ask_with_default("   GCS 前綴 (可留空)", "")?;
    let cred_path = ask_with_default("   GCS 憑證路徑", "gcs-credentials.json")?;

    // 保留
    let retention = ask_with_default("   本地檔案保留時數", "6")?;
    let retention_h: u32 = retention.parse().unwrap_or(6);

    // 產生 YAML
    let yaml = format!(
        r#"# RTSP Recorder 設定檔 — 由 setup 精靈自動產生

rtsp:
  base_url: "{base_url}"
  channels: {channels}
  segment_duration: {segment_duration}

schedule:
  record_start_hour: {start_h}
  record_end_hour: {end_h}

output:
  dir: "{output_dir}"

gcs:
  bucket: "{bucket}"
  prefix: "{prefix}"
  credentials: "{cred_path}"

network:
  interface: "auto"
  idle_threshold_mbps: 8

retention:
  max_hours: {retention_h}

log:
  dir: "{output_dir}/logs"
  rotation: "time"
  retention_days: 30
  to_file: true
"#,
        base_url = base_url,
        channels = format_channels_yaml(&channels),
        segment_duration = segment_duration,
        start_h = start_h,
        end_h = end_h,
        output_dir = output_dir,
        bucket = bucket,
        prefix = prefix,
        cred_path = cred_path,
        retention_h = retention_h,
    );

    std::fs::write(config_path, &yaml)?;
    println!();
    println!("   ✅ 已寫入 {}", config_path.display());
    Ok(())
}

/// 設定系統服務（依平台）
fn setup_system_service(config_path: &Path) -> Result<()> {
    if cfg!(target_os = "linux") {
        setup_linux_service(config_path)?;
    } else if cfg!(target_os = "windows") {
        setup_windows_service(config_path)?;
    } else {
        println!("   ℹ️  macOS 不支援自動安裝服務");
        println!("   手動執行：");
        println!("     rtsp-recorder --daemon");
        println!("   或使用 nohup：");
        println!("     nohup rtsp-recorder --config {} --daemon &",
            config_path.display());
    }
    Ok(())
}

/// Linux: 安裝 systemd 服務
fn setup_linux_service(config_path: &Path) -> Result<()> {
    let install = ask_yn("   是否安裝為 systemd 服務（開機自動啟動）？", true)?;
    if !install {
        println!("   ⏭️  跳過，之後可執行: rtsp-recorder install-service");
        return Ok(());
    }

    let exe_path = std::env::current_exe()?;
    let config_abs = std::fs::canonicalize(config_path)
        .unwrap_or_else(|_| config_path.to_path_buf());
    let user = std::env::var("USER").unwrap_or_else(|_| "root".to_string());

    let service = format!(
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

    match std::fs::write(service_path, &service) {
        Ok(_) => {
            println!("   ✅ 已建立 {}", service_path);
            let _ = std::process::Command::new("systemctl")
                .args(["daemon-reload"])
                .status();
            let _ = std::process::Command::new("systemctl")
                .args(["enable", "rtsp-recorder"])
                .status();
            println!("   ✅ 已設定開機自動啟動");
            println!();
            println!("   管理指令：");
            println!("     sudo systemctl start rtsp-recorder");
            println!("     sudo systemctl status rtsp-recorder");
            println!("     sudo systemctl stop rtsp-recorder");
            println!("     journalctl -u rtsp-recorder -f");
        }
        Err(_) => {
            // 沒有權限，產生在當前目錄讓使用者手動安裝
            let local_service = PathBuf::from("rtsp-recorder.service");
            std::fs::write(&local_service, &service)?;
            println!("   ⚠️  需要 root 權限，已將服務檔案儲存到：");
            println!("   → {}", local_service.display());
            println!();
            println!("   請執行：");
            println!("     sudo cp rtsp-recorder.service {}", service_path);
            println!("     sudo systemctl daemon-reload");
            println!("     sudo systemctl enable --now rtsp-recorder");
        }
    }
    Ok(())
}

/// Windows: 設定排程任務或 NSSM 服務
fn setup_windows_service(config_path: &Path) -> Result<()> {
    println!("   Windows 不支援 systemd，可選擇以下方式：");
    println!("   [1] 使用「工作排程器」（Task Scheduler）開機啟動");
    println!("   [2] 跳過，手動執行");
    println!();

    let choice = ask_with_default("   請選擇 (1/2)", "1")?;

    if choice == "1" {
        let exe_path = std::env::current_exe()?;
        let config_abs = std::fs::canonicalize(config_path)
            .unwrap_or_else(|_| config_path.to_path_buf());

        // 使用 schtasks 建立排程任務
        let task_cmd = format!(
            r#""{}" --config "{}" --daemon"#,
            exe_path.display(),
            config_abs.display(),
        );

        let status = std::process::Command::new("schtasks")
            .args([
                "/Create",
                "/TN", "RTSP-Recorder",
                "/SC", "ONSTART",
                "/TR", &task_cmd,
                "/RL", "HIGHEST",
                "/F",
            ])
            .status();

        match status {
            Ok(s) if s.success() => {
                println!("   ✅ 已建立排程任務 'RTSP-Recorder'");
                println!();
                println!("   管理指令：");
                println!("     schtasks /Query /TN RTSP-Recorder");
                println!("     schtasks /Run /TN RTSP-Recorder");
                println!("     schtasks /End /TN RTSP-Recorder");
                println!("     schtasks /Delete /TN RTSP-Recorder /F");
            }
            _ => {
                println!("   ⚠️  建立排程任務失敗（可能需要系統管理員權限）");
                println!("   請以系統管理員身份開啟終端機後重新執行 setup");
                println!();
                println!("   或手動執行：");
                println!("     {} --config {} --daemon",
                    exe_path.display(), config_abs.display());
            }
        }
    } else {
        println!("   ⏭️  跳過");
        println!("   手動執行：rtsp-recorder --config {} --daemon",
            config_path.display());
    }
    Ok(())
}

// ─────────────── 互動工具 ───────────────

fn read_line() -> String {
    let mut line = String::new();
    io::stdin().lock().read_line(&mut line).unwrap_or(0);
    line.trim().to_string()
}

fn ask_with_default(prompt: &str, default: &str) -> Result<String> {
    if default.is_empty() {
        print!("{}: ", prompt);
    } else {
        print!("{} [{}]: ", prompt, default);
    }
    io::stdout().flush()?;
    let input = read_line();
    if input.is_empty() {
        Ok(default.to_string())
    } else {
        Ok(input)
    }
}

fn ask_yn(prompt: &str, default_yes: bool) -> Result<bool> {
    let hint = if default_yes { "Y/n" } else { "y/N" };
    print!("{} [{}]: ", prompt, hint);
    io::stdout().flush()?;
    let input = read_line().to_lowercase();
    if input.is_empty() {
        Ok(default_yes)
    } else {
        Ok(input.starts_with('y'))
    }
}

/// 解析 "1-10" 或 "1,2,3,5" 格式
fn parse_channels(s: &str) -> Vec<u32> {
    let mut result = Vec::new();
    for part in s.split(',') {
        let part = part.trim();
        if let Some((start, end)) = part.split_once('-') {
            if let (Ok(s), Ok(e)) = (start.trim().parse::<u32>(), end.trim().parse::<u32>()) {
                for ch in s..=e {
                    result.push(ch);
                }
            }
        } else if let Ok(n) = part.parse::<u32>() {
            result.push(n);
        }
    }
    if result.is_empty() {
        result = (1..=10).collect();
    }
    result
}

fn format_channels_yaml(channels: &[u32]) -> String {
    let nums: Vec<String> = channels.iter().map(|n| n.to_string()).collect();
    format!("[{}]", nums.join(", "))
}
