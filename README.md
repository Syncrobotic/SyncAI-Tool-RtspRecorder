# RTSP Recorder

RTSP 監控錄製 + GCS 雲端上傳工具（Rust）

[![Build](https://github.com/Syncrobotic/SyncAI-SDK-RtspTool_Exp/actions/workflows/build.yml/badge.svg)](https://github.com/Syncrobotic/SyncAI-SDK-RtspTool_Exp/actions/workflows/build.yml)

## 功能特色

- **一鍵設定** — `rtsp-recorder setup` 互動式精靈，引導完成所有設定
- **自動下載 ffmpeg** — 首次執行自動偵測/下載，無需手動安裝
- **無縫分段錄製** — 消除分段空白，解決卡頓問題
- **斷線自動接續** — 網路中斷後自動重連，接續剩餘時間錄製
- **自動排程** — 可設定每日錄製時段（支援跨日）
- **MKV → MP4 自動轉檔** — 偵測 H.264/H.265/MJPEG 編碼，自動選擇最佳轉檔策略
- **智慧上傳** — 網路閒置時自動上傳到 Google Cloud Storage
- **本地保留** — 已上傳檔案保留指定時數，供異常回滾
- **跨平台** — 支援 Linux x64、Linux ARM64、Windows x64
- **系統服務** — Linux systemd / Windows Task Scheduler 開機自啟

---

## 快速開始

### 1. 下載

從 [Releases 頁面](https://github.com/Syncrobotic/SyncAI-SDK-RtspTool_Exp/releases) 下載對應平台的執行檔，或自行編譯：

```bash
cd rust
cargo build --release
# 產出 → target/release/rtsp-recorder
```

### 2. 初始設定

```bash
rtsp-recorder setup
```

精靈會依序引導：

| 步驟 | 內容 |
|------|------|
| 1/4 | 檢查/自動下載 ffmpeg |
| 2/4 | 互動式產生 `config.yaml` |
| 3/4 | 引導放置 GCS 憑證 JSON |
| 4/4 | 安裝系統服務（Linux systemd / Windows 排程） |

### 3. 啟動

```bash
# Daemon 模式（推薦）
rtsp-recorder --daemon

# 指定設定檔
rtsp-recorder --config /path/to/config.yaml --daemon
```

---

## 所有指令

```
rtsp-recorder [OPTIONS] [COMMAND]

Commands:
  setup            互動式初始設定（首次使用）
  check            檢查設定與環境
  record           手動錄製
  upload           手動上傳到 GCS
  status           查看運行統計
  cleanup          清理舊檔案
  install-service  安裝為系統服務
  help             顯示說明

Options:
  -c, --config <CONFIG>  設定檔路徑 [預設: config.yaml]
      --daemon           啟動 daemon 模式
  -h, --help             顯示說明
```

### 使用範例

```bash
# 首次設定
rtsp-recorder setup

# 檢查環境（ffmpeg、設定檔、GCS 憑證）
rtsp-recorder check

# 手動錄製 60 秒，指定串流
rtsp-recorder record -d 60 -s cam1,cam2

# 手動上傳所有 MP4 到 GCS
rtsp-recorder upload

# 查看統計（錄製片段數、重啟次數、轉檔/上傳次數）
rtsp-recorder status

# 清理超過 12 小時的舊檔案
rtsp-recorder cleanup -H 12

# 安裝為系統服務
sudo rtsp-recorder install-service
```

---

## 設定檔 `config.yaml`

執行 `rtsp-recorder setup` 會自動產生，也可手動建立。完整範例：

```yaml
# ──────────────── RTSP 設定 ────────────────
rtsp:
  streams:
    - name: "cam1"                                    # 串流名稱（用於檔名）
      url: "rtsp://admin:password@192.168.1.100:554/ch1"
    - name: "cam2"
      url: "rtsp://admin:password@192.168.1.100:554/ch2"
    - name: "cam3"
      url: "rtsp://10.8.60.55:8553/cam3"
  segment_duration: 600                               # 每段錄製秒數（預設 600 = 10 分鐘）

# ──────────────── 排程設定 ────────────────
schedule:
  record_start_hour: 8    # 開始錄製（24 小時制）
  record_end_hour: 1      # 結束錄製（跨日填小於 start 的值）

# ──────────────── 輸出設定 ────────────────
output:
  dir: "~/rtsp-recorder/videos"   # 影片儲存目錄

# ──────────────── GCS 上傳 ────────────────
gcs:
  bucket: "your-bucket-name"        # GCS Bucket 名稱
  prefix: ""                        # 前綴路徑（自動加日期 YYYY/MM/DD）
  credentials: "gcs-credentials.json"  # Service Account JSON 憑證

# ──────────────── 網路控制 ────────────────
network:
  interface: "auto"          # 網卡：auto 自動偵測，或指定 eth0 等
  idle_threshold_mbps: 8     # 低於此流量才上傳（避免影響店家網路）

# ──────────────── 檔案保留 ────────────────
retention:
  max_hours: 6    # 已上傳檔案保留時數（之後自動清理）

# ──────────────── 日誌設定 ────────────────
log:
  dir: "~/rtsp-recorder/videos/logs"
  rotation: "time"        # time=按天輪轉, size=按大小
  retention_days: 30      # 日誌保留天數
  to_file: true           # 是否寫入檔案
```

### 設定說明

| 區塊 | 欄位 | 說明 | 預設值 |
|------|------|------|--------|
| `rtsp` | `streams` | 串流清單（name + url） | — (必填) |
| | `segment_duration` | 每段秒數 | `600` |
| `schedule` | `record_start_hour` | 開始時間 (0-23) | `8` |
| | `record_end_hour` | 結束時間 (0-23) | `1` |
| `output` | `dir` | 影片儲存路徑 | `~/rtsp-recorder/videos` |
| `gcs` | `bucket` | GCS Bucket | — (必填) |
| | `prefix` | 物件前綴 | `""` |
| | `credentials` | SA JSON 路徑 | `gcs-credentials.json` |
| `network` | `interface` | 監控網卡 | `auto` |
| | `idle_threshold_mbps` | 閒置閾值 (Mbps) | `8` |
| `retention` | `max_hours` | 保留時數 | `6` |

---

## GCS 憑證

1. 進入 [GCP Console → IAM → Service Accounts](https://console.cloud.google.com/iam-admin/serviceaccounts)
2. 建立或選取 Service Account，授予 **Storage Object Admin** 角色
3. 建立金鑰 → 選擇 **JSON** 格式 → 下載
4. 將下載的 JSON 放到 `config.yaml` 中 `gcs.credentials` 指定的路徑

---

## 系統服務

### Linux (systemd)

```bash
# 透過 setup 精靈安裝
sudo rtsp-recorder setup

# 或手動安裝
sudo rtsp-recorder install-service

# 管理
sudo systemctl start rtsp-recorder
sudo systemctl status rtsp-recorder
sudo systemctl stop rtsp-recorder
journalctl -u rtsp-recorder -f
```

### Windows (Task Scheduler)

```powershell
# 透過 setup 精靈安裝（以系統管理員執行）
rtsp-recorder.exe setup

# 管理
schtasks /Query /TN RTSP-Recorder
schtasks /Run /TN RTSP-Recorder
schtasks /End /TN RTSP-Recorder
schtasks /Delete /TN RTSP-Recorder /F
```

### macOS

```bash
# macOS 目前不支援自動安裝服務，手動啟動：
nohup rtsp-recorder --daemon &
```

---

## 運行流程

```
啟動 (--daemon)
  │
  ├─ 檢查/下載 ffmpeg
  │
  ├─ 載入 config.yaml
  │
  ├─ 判斷是否在錄製時段
  │   ├─ 否 → 每 60 秒檢查一次
  │   └─ 是 ↓
  │
  ├─ 對每個頻道啟動 ffmpeg 錄製
  │   ├─ 按 segment_duration 分段（MKV）
  │   └─ 斷線 → 3 秒後重連，接續剩餘時間
  │
  ├─ 背景轉檔 (MKV → MP4)
  │   ├─ H.264 → copy（不重編碼）
  │   ├─ H.265 → copy + hvc1 tag
  │   └─ MJPEG → re-encode to H.264
  │
  ├─ 智慧上傳到 GCS（網路閒置時）
  │
  └─ 自動清理舊檔（依 retention.max_hours）
```

---

## 從原始碼編譯

```bash
# 需要 Rust 1.70+
cd rust
cargo build --release

# 執行檔位置
ls -la target/release/rtsp-recorder
```

## License

MIT
