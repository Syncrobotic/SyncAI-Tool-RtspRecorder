# RTSP Recorder

RTSP 監控錄製 + GCS 雲端上傳工具

[![Build](https://github.com/Syncrobotic/SyncAI-SDK-RtspTool_Exp/actions/workflows/build.yml/badge.svg)](https://github.com/Syncrobotic/SyncAI-SDK-RtspTool_Exp/actions/workflows/build.yml)

## 功能

- 多串流同時錄製，自動分段
- 斷線自動重連
- 排程錄製（支援跨日）
- MKV → MP4 自動轉檔
- 網路閒置時自動上傳 GCS
- 已上傳檔案自動清理
- 支援 Linux / Windows / macOS

---

## 快速開始

### 1. 下載

從 [Releases](https://github.com/Syncrobotic/SyncAI-SDK-RtspTool_Exp/releases) 下載，或自行編譯：

```bash
cd rust && cargo build --release
```

### 2. 設定

```bash
rtsp-recorder setup
```

依照精靈完成設定即可。

### 3. 啟動

```bash
rtsp-recorder --daemon
```

---

## 常用指令

| 指令 | 說明 |
|------|------|
| `setup` | 互動式設定精靈 |
| `check` | 檢查環境與設定 |
| `--daemon` | 啟動 daemon 模式 |
| `status` | 查看運行統計 |
| `upload` | 手動上傳 |
| `cleanup -H 12` | 清理 12 小時前的檔案 |
| `install-service` | 安裝為系統服務 |

---

## 設定檔 `config.yaml`

```yaml
rtsp:
  streams:
    - name: "cam1"
      url: "rtsp://admin:pass@192.168.1.100:554/ch1"
    - name: "cam2"
      url: "rtsp://10.8.60.55:8553/cam2"
  segment_duration: 600       # 每段秒數

schedule:
  record_start_hour: 8        # 開始時間 (24h)
  record_end_hour: 20         # 結束時間

output:
  dir: "videos"               # 影片目錄
  resolution: "1920x1080"     # 輸出解析度
  force_reencode: false       # 強制重編碼（播放兼容性）

gcs:
  bucket: "your-bucket"
  credentials: "gcs-credentials.json"

network:
  idle_threshold_mbps: 8      # 低於此流量才上傳

retention:
  max_hours: 6                # 已上傳檔案保留時數
```

> 所有路徑支援相對路徑（相對於 config.yaml 位置）

---

## GCS 憑證

1. [GCP Console](https://console.cloud.google.com/iam-admin/serviceaccounts) → Service Accounts
2. 建立帳號，授予 **Storage Object Admin**
3. 建立 JSON 金鑰並下載
4. 放到 `gcs-credentials.json`

---

## 系統服務

**Linux:**
```bash
sudo rtsp-recorder install-service
sudo systemctl start rtsp-recorder
sudo journalctl -u rtsp-recorder -f   # 查看日誌
```

**Windows:** 以管理員執行 `rtsp-recorder.exe setup`

---

## 部署結構

```
rtsp-recorder/
├── rtsp-recorder           # 執行檔
├── config.yaml             # 設定檔
├── gcs-credentials.json    # GCS 憑證
└── videos/                 # 影片輸出
    └── logs/               # 日誌
```

整個資料夾可直接複製到其他機器部署。

## License

MIT
