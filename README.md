# RTSP Recorder

RTSP 錄製 + GCS 上傳整合工具

提供 **Python** 和 **Rust** 兩個版本，功能相同，選擇適合的使用。

## 功能

- **無縫分段錄製**：消除分段空白，解決卡頓問題
- **斷線自動接續**：網路中斷後自動重連，接續剩餘時間錄製
- **自動排程**：每日 08:00 - 01:00 自動錄製
- **智慧上傳**：網路閒置時自動上傳到 Google Cloud Storage
- **多格式支援**：自動偵測 H.264/H.265/MJPEG 並正確轉檔
- **本地保留**：保留已上傳檔案 6 小時供異常回滾
- **持久化日誌**：日誌按天輪轉，保留 30 天

## 專案結構

```
rtsp-recorder/
├── python/                   # Python 版本
│   ├── rtsp_recorder.py
│   ├── requirements.txt
│   ├── config.yaml
│   └── gcs-credentials.json
│
├── rust/                     # Rust 版本
│   ├── Cargo.toml
│   ├── src/
│   ├── config.yaml
│   └── gcs-credentials.json
│
├── docker/                   # Docker 部署
│   ├── Dockerfile.python
│   ├── Dockerfile.rust
│   └── docker-compose.yml
│
├── config.example.yaml       # 設定檔範例
└── gcs-credentials.example.json
```

---

## Python 版本

### 系統需求

- Python 3.8+
- ffmpeg

### 安裝

```bash
cd python
pip install -r requirements.txt
```

### 設定

```bash
# 複製並編輯設定檔
cp ../config.example.yaml config.yaml
cp ../gcs-credentials.example.json gcs-credentials.json
# 編輯 config.yaml 填入 RTSP URL 和 GCS 設定
```

### 執行

```bash
# Daemon 模式（推薦）
python rtsp_recorder.py --daemon

# 手動錄製
python rtsp_recorder.py record -d 600 -c 1-10

# 查看狀態
python rtsp_recorder.py status

# 安裝為系統服務
sudo python rtsp_recorder.py install-service
```

---

## Rust 版本

### 系統需求

- Rust 1.70+
- ffmpeg

### 編譯

```bash
cd rust
cargo build --release
# 執行檔在 target/release/rtsp-recorder
```

### 設定

```bash
# 複製並編輯設定檔
cp ../config.example.yaml config.yaml
cp ../gcs-credentials.example.json gcs-credentials.json
```

### 執行

```bash
# Daemon 模式
./target/release/rtsp-recorder --daemon

# 或指定設定檔
./target/release/rtsp-recorder --config /path/to/config.yaml --daemon

# 手動錄製
./target/release/rtsp-recorder record -d 600 -c 1-10

# 查看狀態
./target/release/rtsp-recorder status
```

---

## Docker 部署

### Python 版本

```bash
cd docker

# 建置
docker build -f Dockerfile.python -t rtsp-recorder:python ..

# 執行
docker run -d \
  -v $(pwd)/../python/config.yaml:/app/config.yaml:ro \
  -v $(pwd)/../python/gcs-credentials.json:/app/gcs-credentials.json:ro \
  -v $(pwd)/videos:/app/videos \
  --name rtsp-recorder \
  rtsp-recorder:python
```

### Rust 版本

```bash
cd docker

# 建置
docker build -f Dockerfile.rust -t rtsp-recorder:rust ..

# 執行
docker run -d \
  -v $(pwd)/../rust/config.yaml:/app/config/config.yaml:ro \
  -v $(pwd)/../rust/gcs-credentials.json:/app/config/gcs-credentials.json:ro \
  -v $(pwd)/videos:/app/videos \
  --name rtsp-recorder \
  rtsp-recorder:rust
```

### Docker Compose

```bash
cd docker
docker-compose up -d rtsp-recorder-python  # 或 rtsp-recorder-rust
```

---

## 設定檔說明

```yaml
rtsp:
  base_url: "rtsp://admin:password@192.168.1.100:554/Streaming/channels"
  channels: [1, 2, 3, 4, 5, 6, 7, 8, 9, 10]
  segment_duration: 600  # 10 分鐘

schedule:
  record_start_hour: 8   # 08:00 開始
  record_end_hour: 1     # 01:00 結束（隔日）

output:
  dir: "~/rtsp-recorder/videos"

gcs:
  bucket: "your-bucket-name"
  prefix: "recordings"
  credentials: "gcs-credentials.json"

retention:
  max_hours: 6
```

## License

MIT
