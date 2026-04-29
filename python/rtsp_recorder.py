#!/usr/bin/env python3
"""
RTSP 錄製 + 雲端上傳整合腳本

功能：
  - 排程錄製：每日 08:00 - 01:00 自動錄製
  - 閒置上傳：01:00 - 08:00 上傳錄製檔案到 GCS
  - 智慧上傳：錄製期間偵測網路閒置時也會上傳

使用方式:
  # 啟動自動排程模式（推薦）
  python rtsp_recorder.py --daemon

  # 手動錄製（單次）
  python rtsp_recorder.py record -d 600 -c 1-10

  # 手動上傳
  python rtsp_recorder.py upload

  # 設定 systemd 服務
  python rtsp_recorder.py install-service
"""

import argparse
import json
import logging
import logging.handlers
import os
import signal
import subprocess
import sys
import threading
import time
from concurrent.futures import ThreadPoolExecutor, as_completed
from datetime import datetime, timedelta
from pathlib import Path
from typing import Optional

try:
    import yaml
    HAS_YAML = True
except ImportError:
    HAS_YAML = False

# ============================================================================
# 預設設定（會被 config.yaml 覆蓋）
# ============================================================================

CONFIG = {
    # RTSP 設定
    "rtsp_base_url": "",  # 必填，從 config.yaml 載入
    "channels": list(range(1, 11)),  # 1-10
    "segment_duration": 600,  # 10 分鐘一段
    
    # 錄製時間（每日）
    "record_start_hour": 8,   # 08:00 開始
    "record_end_hour": 1,     # 01:00 結束（隔日）
    
    # 輸出設定
    "output_dir": Path.home() / "rtsp-recorder" / "videos",
    
    # GCS 設定
    "gcs_bucket": "",  # 必填，從 config.yaml 載入
    "gcs_prefix": "",  # 會自動加上日期
    "gcs_credentials": Path(__file__).parent / "gcs-credentials.json",
    
    # 網路流量設定
    "network_interface": "auto",  # 網路介面：auto=自動偵測 或指定如 eth0, enp0s3
    "idle_bandwidth_threshold_mbps": 8,  # 低於此值視為閒置
    
    # 本地保留設定
    "max_retention_hours": 6,  # 最多保留 6 小時歷史紀錄（異常回滾用）
    
    # 日誌設定
    "log_dir": Path.home() / "rtsp-recorder" / "videos" / "logs",
    "log_rotation": "time",     # 輪轉方式："time"=按天 或 "size"=按大小
    "log_retention_days": 30,   # 保留天數（log_rotation="time" 時生效）
    "log_max_size_mb": 10,      # 單檔最大 MB（log_rotation="size" 時生效）
    "log_backup_count": 7,      # 保留數量（log_rotation="size" 時生效）
    "log_to_file": True,        # 是否寫入檔案
}

# 狀態
_stop_requested = False
_upload_lock = threading.Lock()
_is_uploading = False
_is_converting = False  # 防止多線程同時轉檔
_file_logger = None  # 檔案日誌器

# ============================================================================
# 統計系統
# ============================================================================

class Stats:
    """統計數據追蹤"""
    
    def __init__(self):
        self.start_time = None
        self.lock = threading.Lock()
        
        # 錄製統計
        self.recording_sessions = 0  # 錄製啟動次數
        self.segments_created = 0    # 產生的片段數
        self.ffmpeg_restarts = 0     # ffmpeg 重啟次數
        self.ffmpeg_errors = {}      # {頻道: [錯誤訊息]}
        
        # 轉檔統計
        self.conversions_ok = 0
        self.conversions_fail = 0
        
        # 上傳統計
        self.uploads_ok = 0
        self.uploads_fail = 0
        self.upload_errors = []      # 最近 10 個錯誤
        self.bytes_uploaded = 0
        
        # 清理統計
        self.files_cleaned = 0
        
        # 上次報告時間
        self.last_report_time = None
    
    def start(self):
        """標記開始時間"""
        self.start_time = datetime.now()
        self.last_report_time = datetime.now()
        self.save()  # 保存初始狀態
    
    def record_segment(self):
        """記錄新片段產生"""
        with self.lock:
            self.segments_created += 1
            # 每 10 個片段保存一次
            if self.segments_created % 10 == 0:
                self.save()
    
    def record_ffmpeg_restart(self, ch_id: int, reason: str):
        """記錄 ffmpeg 重啟"""
        with self.lock:
            self.ffmpeg_restarts += 1
            if ch_id not in self.ffmpeg_errors:
                self.ffmpeg_errors[ch_id] = []
            self.ffmpeg_errors[ch_id].append({
                "time": datetime.now().isoformat(),
                "reason": reason
            })
            # 只保留每頻道最近 5 個錯誤
            self.ffmpeg_errors[ch_id] = self.ffmpeg_errors[ch_id][-5:]
            self.save()  # 錯誤立即保存
    
    def record_conversion(self, success: bool):
        """記錄轉檔結果"""
        with self.lock:
            if success:
                self.conversions_ok += 1
            else:
                self.conversions_fail += 1
    
    def record_upload(self, success: bool, size: int = 0, error: str = None):
        """記錄上傳結果"""
        with self.lock:
            if success:
                self.uploads_ok += 1
                self.bytes_uploaded += size
            else:
                self.uploads_fail += 1
                if error:
                    self.upload_errors.append({
                        "time": datetime.now().isoformat(),
                        "error": error
                    })
                    self.upload_errors = self.upload_errors[-10:]
            self.save()  # 上傳後保存
    
    def record_cleanup(self, count: int):
        """記錄清理數量"""
        with self.lock:
            self.files_cleaned += count
            self.save()
    
    def get_uptime(self) -> str:
        """取得運行時間"""
        if not self.start_time:
            return "未啟動"
        delta = datetime.now() - self.start_time
        hours, remainder = divmod(int(delta.total_seconds()), 3600)
        minutes, seconds = divmod(remainder, 60)
        return f"{hours}h {minutes}m {seconds}s"
    
    def get_summary(self) -> dict:
        """取得統計摘要"""
        with self.lock:
            return {
                "start_time": self.start_time.isoformat() if self.start_time else None,
                "uptime": self.get_uptime(),
                "recording": {
                    "segments": self.segments_created,
                    "ffmpeg_restarts": self.ffmpeg_restarts,
                },
                "conversion": {
                    "success": self.conversions_ok,
                    "failed": self.conversions_fail,
                },
                "upload": {
                    "success": self.uploads_ok,
                    "failed": self.uploads_fail,
                    "bytes": self.bytes_uploaded,
                    "mb": round(self.bytes_uploaded / 1024 / 1024, 2),
                },
                "cleanup": {
                    "files_deleted": self.files_cleaned,
                },
            }
    
    def should_report(self) -> bool:
        """檢查是否該輸出定期報告（每小時）"""
        if not self.last_report_time:
            return False
        return (datetime.now() - self.last_report_time).total_seconds() >= 3600
    
    def log_report(self):
        """輸出統計報告到日誌"""
        summary = self.get_summary()
        self.last_report_time = datetime.now()
        
        log("=" * 50, "INFO")
        log(f"📊 統計報告 | 運行 {summary['uptime']}", "INFO")
        log(f"  錄製: {summary['recording']['segments']} 片段, {summary['recording']['ffmpeg_restarts']} 次重啟", "INFO")
        log(f"  轉檔: ✅{summary['conversion']['success']} ❌{summary['conversion']['failed']}", "INFO")
        log(f"  上傳: ✅{summary['upload']['success']} ❌{summary['upload']['failed']} ({summary['upload']['mb']} MB)", "INFO")
        log(f"  清理: {summary['cleanup']['files_deleted']} 檔案", "INFO")
        
        # 輸出最近的錯誤
        if self.ffmpeg_errors:
            channels_with_errors = list(self.ffmpeg_errors.keys())
            log(f"  ⚠️ 有錯誤的頻道: {channels_with_errors}", "WARN")
        
        if self.upload_errors:
            last_error = self.upload_errors[-1]
            log(f"  ⚠️ 最近上傳錯誤: {last_error['error'][:50]}", "WARN")
        
        log("=" * 50, "INFO")
        
        # 保存統計到檔案
        self.save()
    
    def get_stats_file_path(self) -> Path:
        """取得統計檔案路徑"""
        return CONFIG["output_dir"] / ".stats.json"
    
    def save(self):
        """保存統計到 JSON 檔案"""
        try:
            data = {
                "start_time": self.start_time.isoformat() if self.start_time else None,
                "last_update": datetime.now().isoformat(),
                "recording": {
                    "sessions": self.recording_sessions,
                    "segments": self.segments_created,
                    "ffmpeg_restarts": self.ffmpeg_restarts,
                    "ffmpeg_errors": {str(k): v for k, v in self.ffmpeg_errors.items()},
                },
                "conversion": {
                    "success": self.conversions_ok,
                    "failed": self.conversions_fail,
                },
                "upload": {
                    "success": self.uploads_ok,
                    "failed": self.uploads_fail,
                    "bytes": self.bytes_uploaded,
                    "errors": self.upload_errors,
                },
                "cleanup": {
                    "files_deleted": self.files_cleaned,
                },
            }
            path = self.get_stats_file_path()
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(json.dumps(data, indent=2, ensure_ascii=False))
        except Exception:
            pass  # 保存失敗不影響主要功能
    
    @classmethod
    def load_from_file(cls) -> Optional[dict]:
        """從檔案載入統計（用於 status 命令）"""
        try:
            path = CONFIG["output_dir"] / ".stats.json"
            if path.exists():
                return json.loads(path.read_text())
        except Exception:
            pass
        return None


# 全域統計實例
_stats = Stats()


def load_config():
    """
    載入 YAML 設定檔（config.yaml）
    
    優先順序：
    1. config.yaml（如果存在）
    2. CONFIG 預設值
    3. CLI 參數可覆蓋（在各 cmd_* 函數中處理）
    """
    config_path = Path(__file__).parent / "config.yaml"
    
    if not config_path.exists():
        return  # 使用預設值
    
    if not HAS_YAML:
        print("⚠️ 找到 config.yaml 但未安裝 PyYAML，使用預設設定")
        print("  安裝方式: pip install pyyaml")
        return
    
    try:
        with open(config_path, encoding="utf-8") as f:
            cfg = yaml.safe_load(f)
        
        if not cfg:
            return
        
        # RTSP 設定
        if "rtsp" in cfg:
            if "base_url" in cfg["rtsp"]:
                CONFIG["rtsp_base_url"] = cfg["rtsp"]["base_url"]
            if "channels" in cfg["rtsp"]:
                CONFIG["channels"] = cfg["rtsp"]["channels"]
            if "segment_duration" in cfg["rtsp"]:
                CONFIG["segment_duration"] = cfg["rtsp"]["segment_duration"]
        
        # 排程設定
        if "schedule" in cfg:
            if "record_start_hour" in cfg["schedule"]:
                CONFIG["record_start_hour"] = cfg["schedule"]["record_start_hour"]
            if "record_end_hour" in cfg["schedule"]:
                CONFIG["record_end_hour"] = cfg["schedule"]["record_end_hour"]
        
        # 輸出設定
        if "output" in cfg:
            if "dir" in cfg["output"]:
                path = cfg["output"]["dir"]
                CONFIG["output_dir"] = Path(path).expanduser()
        
        # GCS 設定
        if "gcs" in cfg:
            if "bucket" in cfg["gcs"]:
                CONFIG["gcs_bucket"] = cfg["gcs"]["bucket"]
            if "prefix" in cfg["gcs"]:
                CONFIG["gcs_prefix"] = cfg["gcs"]["prefix"] or ""
            if "credentials" in cfg["gcs"]:
                creds = cfg["gcs"]["credentials"]
                if creds:
                    creds_path = Path(creds)
                    if not creds_path.is_absolute():
                        creds_path = Path(__file__).parent / creds_path
                    CONFIG["gcs_credentials"] = creds_path
        
        # 網路設定
        if "network" in cfg:
            if "interface" in cfg["network"]:
                CONFIG["network_interface"] = cfg["network"]["interface"]
            if "idle_threshold_mbps" in cfg["network"]:
                CONFIG["idle_bandwidth_threshold_mbps"] = cfg["network"]["idle_threshold_mbps"]
        
        # 保留設定
        if "retention" in cfg:
            if "max_hours" in cfg["retention"]:
                CONFIG["max_retention_hours"] = cfg["retention"]["max_hours"]
        
        # 日誌設定
        if "log" in cfg:
            if "dir" in cfg["log"]:
                path = cfg["log"]["dir"]
                CONFIG["log_dir"] = Path(path).expanduser()
            if "rotation" in cfg["log"]:
                CONFIG["log_rotation"] = cfg["log"]["rotation"]
            if "retention_days" in cfg["log"]:
                CONFIG["log_retention_days"] = cfg["log"]["retention_days"]
            if "max_size_mb" in cfg["log"]:
                CONFIG["log_max_size_mb"] = cfg["log"]["max_size_mb"]
            if "backup_count" in cfg["log"]:
                CONFIG["log_backup_count"] = cfg["log"]["backup_count"]
            if "to_file" in cfg["log"]:
                CONFIG["log_to_file"] = cfg["log"]["to_file"]
        
        print(f"✅ 已載入設定檔: {config_path}")
        
    except Exception as e:
        print(f"⚠️ 載入設定檔失敗: {e}")
        print("  使用預設設定")


def validate_config(require_rtsp=False, require_gcs=False):
    """驗證必填設定"""
    errors = []
    
    if require_rtsp and not CONFIG.get("rtsp_base_url"):
        errors.append("❗ rtsp.base_url 未設定")
    
    if require_gcs and not CONFIG.get("gcs_bucket"):
        errors.append("❗ gcs.bucket 未設定")
    
    if errors:
        print("\n❌ 設定檔錯誤:")
        for err in errors:
            print(f"  {err}")
        print("\n請編輯 config.yaml 並填入必要設定")
        print(f"  設定檔位置: {Path(__file__).parent / 'config.yaml'}")
        sys.exit(1)


# 載入設定檔
load_config()
_file_logger = None  # 檔案日誌器


# ============================================================================
# 工具函數
# ============================================================================

def setup_file_logger():
    """初始化檔案日誌器（支援按大小或按時間輪轉）"""
    global _file_logger
    
    if not CONFIG.get("log_to_file", True):
        return
    
    log_dir = CONFIG["log_dir"]
    log_dir.mkdir(parents=True, exist_ok=True)
    
    log_file = log_dir / "rtsp_recorder.log"
    
    _file_logger = logging.getLogger("rtsp_recorder")
    _file_logger.setLevel(logging.DEBUG)
    
    # 避免重複添加 handler
    if _file_logger.handlers:
        return
    
    rotation_mode = CONFIG.get("log_rotation", "time")
    
    if rotation_mode == "time":
        # 按天輪轉，保留指定天數
        retention_days = CONFIG.get("log_retention_days", 30)
        file_handler = logging.handlers.TimedRotatingFileHandler(
            log_file,
            when="midnight",           # 每天午夜輪轉
            interval=1,
            backupCount=retention_days,  # 保留天數
            encoding="utf-8",
        )
        # 設定輪轉後的檔名後綴（如 rtsp_recorder.log.2026-04-23）
        file_handler.suffix = "%Y-%m-%d"
    else:
        # 按大小輪轉
        max_bytes = CONFIG.get("log_max_size_mb", 10) * 1024 * 1024
        backup_count = CONFIG.get("log_backup_count", 7)
        file_handler = logging.handlers.RotatingFileHandler(
            log_file,
            maxBytes=max_bytes,
            backupCount=backup_count,
            encoding="utf-8",
        )
    
    file_handler.setFormatter(logging.Formatter(
        "%(asctime)s [%(levelname)s] %(message)s",
        datefmt="%Y-%m-%d %H:%M:%S"
    ))
    _file_logger.addHandler(file_handler)
    
    # 記錄啟動事件
    mode_desc = f"按天輪轉，保留 {CONFIG.get('log_retention_days', 30)} 天" if rotation_mode == "time" else f"按大小輪轉，單檔 {CONFIG.get('log_max_size_mb', 10)}MB"
    _file_logger.info("=" * 60)
    _file_logger.info(f"日誌系統啟動（{mode_desc}）")


def log(msg: str, level: str = "INFO"):
    """統一日誌格式（同時輸出到終端和檔案）"""
    timestamp = datetime.now().strftime("%Y-%m-%d %H:%M:%S")
    prefix = {"INFO": "ℹ️", "WARN": "⚠️", "ERROR": "❌", "OK": "✅", "REC": "🔴", "UP": "📤"}.get(level, "")
    
    # 終端輸出（帶 emoji）
    print(f"[{timestamp}] {prefix} {msg}")
    
    # 檔案輸出（純文字，方便 grep）
    if _file_logger:
        log_level = {
            "INFO": logging.INFO,
            "WARN": logging.WARNING,
            "ERROR": logging.ERROR,
            "OK": logging.INFO,
            "REC": logging.INFO,
            "UP": logging.INFO,
        }.get(level, logging.INFO)
        
        # 加入標籤方便過濾
        tag = f"[{level}]" if level not in ("INFO",) else ""
        _file_logger.log(log_level, f"{tag} {msg}".strip())


def is_recording_time() -> bool:
    """檢查現在是否為錄製時間 (08:00 - 01:00)"""
    now = datetime.now()
    hour = now.hour
    start = CONFIG["record_start_hour"]
    end = CONFIG["record_end_hour"]
    
    # 跨午夜的情況：08:00 - 23:59 或 00:00 - 01:00
    if start > end:
        return hour >= start or hour < end
    else:
        return start <= hour < end


def is_upload_time() -> bool:
    """檢查現在是否為上傳時間 (01:00 - 08:00)"""
    return not is_recording_time()


def get_network_usage_mbps(interface: str = "auto") -> float:
    """
    取得目前網路使用量 (Mbps)
    
    interface: 
      - "auto" = 自動計算所有實體網卡的總流量（排除 lo, docker, veth）
      - 指定名稱 = 只計算該網卡
    """
    try:
        # 讀取 /proc/net/dev
        with open("/proc/net/dev") as f:
            lines = f.readlines()
        
        # 要排除的虛擬介面
        excluded = ("lo", "docker", "veth", "br-", "virbr")
        
        def parse_interface(lines, interface):
            """解析指定介面或所有實體介面的流量"""
            total_rx = 0
            total_tx = 0
            
            for line in lines[2:]:  # 跳過標題行
                if ":" not in line:
                    continue
                iface_name = line.split(":")[0].strip()
                
                # 自動模式：排除虛擬介面
                if interface == "auto":
                    if any(iface_name.startswith(ex) for ex in excluded):
                        continue
                else:
                    # 指定介面模式
                    if iface_name != interface:
                        continue
                
                parts = line.split()
                # 找到 ":" 後的數據
                data_start = line.index(":") + 1
                data = line[data_start:].split()
                if len(data) >= 9:
                    total_rx += int(data[0])
                    total_tx += int(data[8])
            
            return total_rx, total_tx
        
        rx_bytes, tx_bytes = parse_interface(lines, interface)
        
        if rx_bytes == 0 and tx_bytes == 0:
            return 0.0
        
        time.sleep(1)
        
        with open("/proc/net/dev") as f:
            lines = f.readlines()
        
        rx_bytes_new, tx_bytes_new = parse_interface(lines, interface)
        
        # 計算每秒流量 (Mbps)
        rx_mbps = (rx_bytes_new - rx_bytes) * 8 / 1_000_000
        tx_mbps = (tx_bytes_new - tx_bytes) * 8 / 1_000_000
        
        return rx_mbps + tx_mbps
    except Exception:
        return 0.0


def is_network_idle() -> bool:
    """檢查網路是否閒置"""
    usage = get_network_usage_mbps(CONFIG["network_interface"])
    return usage < CONFIG["idle_bandwidth_threshold_mbps"]


def get_pending_files() -> list[Path]:
    """取得待上傳的 mp4 檔案（按修改時間排序）"""
    output_dir = CONFIG["output_dir"]
    if not output_dir.exists():
        return []
    
    # 取得已上傳清單
    uploaded = load_uploaded_records()
    uploaded_names = set(uploaded.keys())
    
    files = [f for f in output_dir.glob("*.mp4") if f.name not in uploaded_names]
    # 按修改時間排序（舊的優先）
    files.sort(key=lambda f: f.stat().st_mtime)
    return files


def get_uploaded_records_path() -> Path:
    """取得已上傳記錄檔路徑"""
    return CONFIG["output_dir"] / ".uploaded.json"


def load_uploaded_records() -> dict:
    """載入已上傳記錄 {filename: {url, uploaded_at}}"""
    path = get_uploaded_records_path()
    if path.exists():
        try:
            return json.loads(path.read_text())
        except Exception:
            return {}
    return {}


def save_uploaded_record(filename: str, url: str):
    """記錄檔案已上傳成功"""
    records = load_uploaded_records()
    records[filename] = {
        "url": url,
        "uploaded_at": datetime.now().isoformat(),
    }
    path = get_uploaded_records_path()
    path.write_text(json.dumps(records, indent=2, ensure_ascii=False))


def cleanup_old_files() -> int:
    """清理已上傳且超過保留時間的檔案，返回刪除數量"""
    output_dir = CONFIG["output_dir"]
    if not output_dir.exists():
        return 0
    
    max_age = timedelta(hours=CONFIG["max_retention_hours"])
    cutoff = datetime.now() - max_age
    deleted = 0
    
    # 載入已上傳記錄
    uploaded = load_uploaded_records()
    updated_records = dict(uploaded)
    
    # 只清理「已上傳」且「超過保留時間」的檔案
    for pattern in ("*.mp4", "*.mkv"):
        for f in output_dir.glob(pattern):
            try:
                # 只有已上傳的檔案才考慮刪除
                if f.name not in uploaded:
                    continue
                
                mtime = datetime.fromtimestamp(f.stat().st_mtime)
                if mtime < cutoff:
                    f.unlink()
                    deleted += 1
                    # 從記錄中移除
                    updated_records.pop(f.name, None)
                    log(f"🗑️ 清理已上傳檔案: {f.name}", "INFO")
            except Exception as e:
                log(f"清理失敗 {f.name}: {e}", "ERROR")
    
    # 清理孤立的 log 檔案（超時即刪）
    for f in output_dir.glob("*.log"):
        try:
            mtime = datetime.fromtimestamp(f.stat().st_mtime)
            if mtime < cutoff:
                f.unlink()
        except Exception:
            pass
    
    # 更新記錄檔
    if updated_records != uploaded:
        get_uploaded_records_path().write_text(
            json.dumps(updated_records, indent=2, ensure_ascii=False)
        )
    
    return deleted


# ============================================================================
# RTSP 錄製
# ============================================================================

def _calc_seconds_to_next_boundary(segment_duration: int) -> int:
    """
    計算距離下一個分段邊界的秒數
    
    例如 segment_duration=600 (10分鐘)：
    - 10:03:20 → 距離 10:10:00 還有 400 秒
    - 10:08:45 → 距離 10:10:00 還有 75 秒
    """
    now = datetime.now()
    # 當天 00:00:00
    midnight = now.replace(hour=0, minute=0, second=0, microsecond=0)
    # 從午夜起的秒數
    seconds_since_midnight = int((now - midnight).total_seconds())
    # 當前片段內已過的秒數
    elapsed_in_segment = seconds_since_midnight % segment_duration
    # 距離下一邊界的秒數
    remaining = segment_duration - elapsed_in_segment
    return remaining


def _build_ffmpeg_cmd(url: str, output_pattern: str, segment_duration: int) -> list:
    """建構 ffmpeg 錄製命令"""
    return [
        "ffmpeg", "-y",
        # === 輸入參數（關鍵優化）===
        "-fflags", "+genpts+discardcorrupt+nobuffer+igndts",
        "-rtsp_transport", "tcp",
        "-rtsp_flags", "prefer_tcp",
        "-use_wallclock_as_timestamps", "1",  # 使用系統時鐘作為時間戳
        "-timeout", "10000000",
        "-buffer_size", "8388608",  # 8MB 緩衝區
        "-max_delay", "500000",
        "-reorder_queue_size", "5000",  # 增加重排序佇列
        "-i", url,
        
        # === 時間戳強制重置（關鍵！）===
        "-start_at_zero",        # 強制從 0 開始
        
        # === 輸出參數（無縫分段）===
        "-c:v", "copy",
        "-c:a", "aac",  # 重新編碼音訊以修正時間戳
        "-af", "aresample=async=1",  # 音訊重採樣修正同步
        "-map", "0",
        
        # 分段設定
        "-f", "segment",
        "-segment_time", str(segment_duration),
        "-segment_format", "matroska",
        "-reset_timestamps", "1",  # 每段時間戳從 0 開始
        "-segment_atclocktime", "1",  # 對齊時鐘整點
        "-strftime", "1",  # 使用時間格式命名
        
        # 時間戳處理
        "-avoid_negative_ts", "make_zero",
        "-max_muxing_queue_size", "1024",
        
        output_pattern,
    ]


def record_channel_seamless(ch_id: int, segment_duration: int, output_dir: Path, base_url: str, stop_event: threading.Event):
    """
    無縫分段錄製（支援斷線後接續剩餘時間）
    
    使用 ffmpeg segment 功能，單一連線持續錄製，自動分段
    優點：
    1. 消除分段之間的空白（無需重新連線）
    2. 減少卡頓（持續連線 + 緩衝區）
    3. 即時開始（不需等待 I-frame 同步）
    4. 斷線重連後接續剩餘時間，不遺失後續影片
    """
    url = f"{base_url}/chID={ch_id}&streamType=main&linkType=tcp"
    
    # 輸出檔名模板：ch1_20260423_143000.mkv
    output_pattern = str(output_dir / f"ch{ch_id}_%Y%m%d_%H%M%S.mkv")
    log_file = output_dir / f"ch{ch_id}_seamless.log"

    # 正常模式：使用完整 segment_duration
    cmd = _build_ffmpeg_cmd(url, output_pattern, segment_duration)

    log(f"[CH{ch_id}] 啟動無縫錄製（每 {segment_duration}s 自動分段）", "REC")
    log_fh = open(log_file, "w")
    proc = subprocess.Popen(
        cmd,
        stdout=subprocess.DEVNULL,
        stderr=log_fh,
        stdin=subprocess.PIPE,  # 用於發送 'q' 優雅退出
    )
    
    # 追蹤是否處於「接續剩餘時間」模式
    is_resuming = False
    resume_end_time = None
    
    # 監控並等待停止信號
    while not stop_event.is_set():
        if proc.poll() is not None:
            # ffmpeg 意外退出，嘗試重啟
            exit_code = proc.returncode
            log(f"[CH{ch_id}] ffmpeg 意外退出 (code={exit_code})，3 秒後重啟...", "WARN")
            _stats.record_ffmpeg_restart(ch_id, f"exit_code={exit_code}")
            time.sleep(3)
            
            if not stop_event.is_set():
                # 計算到下一個整點邊界的剩餘時間
                remaining_seconds = _calc_seconds_to_next_boundary(segment_duration)
                
                # 如果剩餘時間太短（< 30秒），直接等到下一個完整週期
                if remaining_seconds < 30:
                    log(f"[CH{ch_id}] 剩餘 {remaining_seconds}s 太短，等待下一週期", "INFO")
                    time.sleep(remaining_seconds + 1)
                    remaining_seconds = segment_duration
                    is_resuming = False
                else:
                    is_resuming = True
                    resume_end_time = datetime.now() + timedelta(seconds=remaining_seconds)
                    log(f"[CH{ch_id}] 接續剩餘 {remaining_seconds}s（至 {resume_end_time.strftime('%H:%M:%S')}）", "INFO")
                
                # 使用剩餘時間作為 segment_duration
                cmd_resume = _build_ffmpeg_cmd(url, output_pattern, remaining_seconds if is_resuming else segment_duration)
                
                log_fh = open(log_file, "a")
                proc = subprocess.Popen(
                    cmd_resume,
                    stdout=subprocess.DEVNULL,
                    stderr=log_fh,
                    stdin=subprocess.PIPE,
                )
        
        # 檢查是否需要從「接續模式」切換回「正常模式」
        if is_resuming and resume_end_time and datetime.now() >= resume_end_time:
            # 已過整點邊界，重啟為正常模式
            if proc.poll() is None:
                try:
                    proc.stdin.write(b"q")
                    proc.stdin.flush()
                    proc.wait(timeout=5)
                except Exception:
                    proc.kill()
                    proc.wait()
            
            log(f"[CH{ch_id}] 接續完成，恢復正常 {segment_duration}s 週期", "INFO")
            is_resuming = False
            resume_end_time = None
            
            cmd = _build_ffmpeg_cmd(url, output_pattern, segment_duration)
            log_fh = open(log_file, "a")
            proc = subprocess.Popen(
                cmd,
                stdout=subprocess.DEVNULL,
                stderr=log_fh,
                stdin=subprocess.PIPE,
            )
        
        time.sleep(1)
    
    # 優雅停止
    if proc.poll() is None:
        try:
            proc.stdin.write(b"q")
            proc.stdin.flush()
            proc.wait(timeout=10)
        except Exception:
            proc.kill()
            proc.wait()
    
    log_fh.close()
    log(f"[CH{ch_id}] 無縫錄製已停止", "OK")


def _detect_video_codec(file_path: Path) -> Optional[str]:
    """偵測影片的視訊編碼格式"""
    cmd = [
        "ffprobe", "-v", "error",
        "-select_streams", "v:0",
        "-show_entries", "stream=codec_name",
        "-of", "default=noprint_wrappers=1:nokey=1",
        str(file_path)
    ]
    try:
        result = subprocess.run(cmd, capture_output=True, text=True, timeout=10)
        if result.returncode == 0:
            codec = result.stdout.strip().lower()
            return codec  # 例如: h264, hevc, h265
    except Exception:
        pass
    return None


def convert_to_mp4(mkv_path: Path) -> Optional[Path]:
    """
    轉檔 mkv -> mp4（通用型，自動偵測多種編碼格式）
    
    支援格式：
    - H.264 (AVC)：直接複製（最快）
    - H.265 (HEVC)：直接複製 + hvc1 tag（QuickTime 相容）
    - MJPEG：重新編碼為 H.264（MP4 不支援 MJPEG）
    - MPEG-4 Part 2：直接複製或重編碼
    - 其他：嘗試複製，失敗則重編碼
    """
    if not mkv_path.exists():
        return None
    
    mp4_path = mkv_path.with_suffix(".mp4")
    
    # 偵測編碼格式
    codec = _detect_video_codec(mkv_path)
    
    if codec:
        log(f"[轉檔] 偵測到 {codec.upper()} 編碼: {mkv_path.name}", "INFO")
    
    # 判斷編碼類型
    is_hevc = codec in ("hevc", "h265")
    is_mjpeg = codec in ("mjpeg", "mjpg")
    is_mpeg4 = codec in ("mpeg4", "msmpeg4v3", "msmpeg4v2")
    needs_reencode = is_mjpeg  # MJPEG 必須重編碼
    
    if not needs_reencode:
        # 方案 1：直接複製串流（最快）
        cmd = [
            "ffmpeg", "-y",
            "-fflags", "+genpts+igndts+discardcorrupt",
            "-i", str(mkv_path),
            "-c:v", "copy",
            "-c:a", "aac",
            "-b:a", "128k",
            "-ar", "44100",
            "-movflags", "+faststart",
            "-avoid_negative_ts", "make_zero",
        ]
        
        # H.265 使用 hvc1 tag（QuickTime/Safari 相容）
        if is_hevc:
            cmd.extend(["-tag:v", "hvc1"])
        
        cmd.append(str(mp4_path))
        
        result = subprocess.run(cmd, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
        if result.returncode == 0 and mp4_path.exists() and mp4_path.stat().st_size > 1000:
            mkv_path.unlink()
            return mp4_path
        
        log(f"[轉檔] copy 模式失敗，重新編碼為 H.264: {mkv_path.name}", "WARN")
    else:
        log(f"[轉檔] {codec.upper()} 需重新編碼為 H.264: {mkv_path.name}", "INFO")
    
    # 方案 2：重新編碼為 H.264（保證所有播放器相容）
    cmd_reencode = [
        "ffmpeg", "-y",
        "-fflags", "+genpts+igndts+discardcorrupt",
        "-i", str(mkv_path),
        "-c:v", "libx264",
        "-preset", "veryfast",
        "-crf", "20",
        "-profile:v", "high",
        "-level", "4.1",
        "-pix_fmt", "yuv420p",
        "-c:a", "aac",
        "-b:a", "128k",
        "-ar", "44100",
        "-movflags", "+faststart",
        "-avoid_negative_ts", "make_zero",
        str(mp4_path),
    ]
    result = subprocess.run(cmd_reencode, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    if result.returncode == 0 and mp4_path.exists() and mp4_path.stat().st_size > 1000:
        mkv_path.unlink()
        return mp4_path
    return None


def convert_batch_background(tasks: list, round_num: int):
    """背景執行緒：轉檔一批錄製結果（自動偵測多種編碼格式）"""
    converting = []      # (ch, proc, mkv, mp4, log_file, codec, needs_reencode)
    reencode_queue = []  # 需要重編碼的任務
    
    for ch, returncode, output_file, log_file in tasks:
        if returncode == 0:
            mp4_path = output_file.with_suffix(".mp4")
            
            # 偵測編碼格式
            codec = _detect_video_codec(output_file)
            is_hevc = codec in ("hevc", "h265")
            is_mjpeg = codec in ("mjpeg", "mjpg")
            needs_reencode = is_mjpeg  # MJPEG 必須重編碼
            
            if needs_reencode:
                # MJPEG 等格式直接加入重編碼佇列
                reencode_queue.append((ch, output_file, mp4_path, log_file, codec))
            else:
                # H.264/H.265 等先嘗試 copy
                cmd = [
                    "ffmpeg", "-y",
                    "-fflags", "+genpts+igndts+discardcorrupt",
                    "-i", str(output_file),
                    "-c:v", "copy",
                    "-c:a", "aac",
                    "-b:a", "128k",
                    "-ar", "44100",
                    "-movflags", "+faststart",
                    "-avoid_negative_ts", "make_zero",
                ]
                
                if is_hevc:
                    cmd.extend(["-tag:v", "hvc1"])
                
                cmd.append(str(mp4_path))
                
                proc = subprocess.Popen(cmd, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
                converting.append((ch, proc, output_file, mp4_path, log_file, codec))
        else:
            log(f"[R{round_num}][CH{ch}] 錄製失敗", "ERROR")
            log_file.unlink(missing_ok=True)

    ok = 0
    fail = 0
    
    # 處理 copy 模式的結果
    for ch, proc, mkv_path, mp4_path, log_file, codec in converting:
        proc.wait()
        if proc.returncode == 0 and mp4_path.exists() and mp4_path.stat().st_size > 1000:
            mkv_path.unlink()
            ok += 1
            log_file.unlink(missing_ok=True)
        else:
            # copy 失敗，加入重編碼佇列
            reencode_queue.append((ch, mkv_path, mp4_path, log_file, codec))
    
    # 處理需要重編碼的任務
    for ch, mkv_path, mp4_path, log_file, codec in reencode_queue:
        log(f"[R{round_num}][CH{ch}] {codec.upper() if codec else '未知'} 重編碼中...", "INFO")
        cmd_reencode = [
            "ffmpeg", "-y",
            "-fflags", "+genpts+igndts+discardcorrupt",
            "-i", str(mkv_path),
            "-c:v", "libx264",
            "-preset", "veryfast",
            "-crf", "20",
            "-pix_fmt", "yuv420p",
            "-c:a", "aac",
            "-b:a", "128k",
            "-ar", "44100",
            "-movflags", "+faststart",
            "-avoid_negative_ts", "make_zero",
            str(mp4_path),
        ]
        result = subprocess.run(cmd_reencode, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        if result.returncode == 0 and mp4_path.exists() and mp4_path.stat().st_size > 1000:
            mkv_path.unlink()
            ok += 1
        else:
            fail += 1
        log_file.unlink(missing_ok=True)

    log(f"第 {round_num} 輪轉檔完成（✅{ok} ❌{fail}）", "OK")


def run_recording_loop():
    """錄製主迴圈"""
    global _stop_requested
    
    channels = CONFIG["channels"]
    segment_sec = CONFIG["segment_duration"]
    output_dir = CONFIG["output_dir"]
    base_url = CONFIG["rtsp_base_url"]
    
    output_dir.mkdir(parents=True, exist_ok=True)
    
    convert_threads: list[threading.Thread] = []
    round_num = 0

    while not _stop_requested:
        # 檢查是否為錄製時間
        if not is_recording_time():
            log("目前不在錄製時間，等待中...", "INFO")
            time.sleep(60)
            continue
        
        round_num += 1
        round_start = datetime.now()
        log(f"第 {round_num} 輪錄製開始 | {round_start.strftime('%H:%M:%S')}", "REC")

        # 同時啟動所有頻道
        running = []
        for ch in channels:
            proc, output_file, log_file, log_fh = record_channel(ch, segment_sec, output_dir, base_url)
            running.append((ch, proc, output_file, log_file, log_fh))

        # 等待錄製完成，同時檢查是否有空閒可以上傳
        deadline = segment_sec + 60
        check_interval = 30  # 每 30 秒檢查一次網路
        waited = 0
        
        while waited < deadline:
            # 檢查是否所有錄製都完成
            all_done = all(proc.poll() is not None for _, proc, _, _, _ in running)
            if all_done:
                break
            
            # 檢查網路是否閒置，嘗試背景上傳
            if is_network_idle() and not _is_uploading:
                threading.Thread(target=try_background_upload, daemon=True).start()
            
            time.sleep(min(check_interval, deadline - waited))
            waited += check_interval

        # 收集結果
        tasks = []
        for ch, proc, output_file, log_file, log_fh in running:
            if proc.poll() is None:
                log(f"[CH{ch}] 逾時 {deadline}s，強制終止", "WARN")
                proc.kill()
            proc.wait()
            log_fh.close()
            tasks.append((ch, proc.returncode, output_file, log_file))

        elapsed = (datetime.now() - round_start).total_seconds()
        log(f"第 {round_num} 輪錄製完成（耗時 {elapsed:.0f}s）", "OK")

        # 非同步轉檔
        t = threading.Thread(
            target=convert_batch_background,
            args=(tasks, round_num),
            daemon=True,
        )
        t.start()
        convert_threads.append(t)

        # 清理已完成的執行緒
        convert_threads = [t for t in convert_threads if t.is_alive()]

        if _stop_requested:
            break

    # 等待所有背景轉檔完成
    pending = [t for t in convert_threads if t.is_alive()]
    if pending:
        log(f"等待 {len(pending)} 個背景轉檔完成...", "INFO")
        for t in pending:
            t.join(timeout=120)


def run_seamless_recording_loop():
    """
    無縫錄製主迴圈（推薦）
    
    每個頻道一個持續運行的 ffmpeg 進程，自動分段
    優點：
    - 無分段空白
    - 減少連線延遲
    - 更穩定的時間戳
    """
    global _stop_requested
    
    channels = CONFIG["channels"]
    segment_sec = CONFIG["segment_duration"]
    output_dir = CONFIG["output_dir"]
    base_url = CONFIG["rtsp_base_url"]
    
    output_dir.mkdir(parents=True, exist_ok=True)
    
    # 各頻道的停止事件
    stop_events = {ch: threading.Event() for ch in channels}
    recording_threads: list[threading.Thread] = []
    
    log(f"🚀 啟動無縫錄製模式 | {len(channels)} 頻道 | 每段 {segment_sec}s", "REC")
    
    while not _stop_requested:
        # 檢查是否為錄製時間
        if not is_recording_time():
            # 停止所有錄製
            if recording_threads:
                log("離開錄製時間，停止所有頻道...", "INFO")
                for ch in channels:
                    stop_events[ch].set()
                for t in recording_threads:
                    t.join(timeout=30)
                recording_threads.clear()
                # 重置停止事件
                for ch in channels:
                    stop_events[ch].clear()
                log("所有頻道已停止", "OK")
            
            time.sleep(60)
            continue
        
        # 確保所有頻道都在錄製
        if not recording_threads:
            log("進入錄製時間，啟動所有頻道...", "INFO")
            for ch in channels:
                t = threading.Thread(
                    target=record_channel_seamless,
                    args=(ch, segment_sec, output_dir, base_url, stop_events[ch]),
                    daemon=True,
                    name=f"record-ch{ch}",
                )
                t.start()
                recording_threads.append(t)
            log(f"已啟動 {len(channels)} 個頻道的無縫錄製", "OK")
        
        # 定期執行背景任務
        for _ in range(60):  # 每分鐘檢查一次
            if _stop_requested:
                break
            
            # 檢查網路是否閒置，嘗試背景上傳
            if is_network_idle() and not _is_uploading:
                threading.Thread(target=try_background_upload, daemon=True).start()
            
            # 背景轉檔已完成的 mkv 檔案
            threading.Thread(target=convert_completed_mkv_files, daemon=True).start()
            
            time.sleep(1)
    
    # 停止所有錄製
    log("收到停止信號，正在停止所有頻道...", "INFO")
    for ch in channels:
        stop_events[ch].set()
    for t in recording_threads:
        t.join(timeout=30)
    log("所有錄製已停止", "OK")


def convert_completed_mkv_files():
    """背景轉檔已完成的 mkv 檔案（非正在錄製的）"""
    global _is_converting
    
    # 防止多線程同時轉檔
    if _is_converting:
        return
    _is_converting = True
    
    try:
        output_dir = CONFIG["output_dir"]
        if not output_dir.exists():
            return
    
        # 找出已完成的 mkv 檔案（檔案大小穩定超過 10 秒）
        for mkv in output_dir.glob("*.mkv"):
            try:
                # 跳過太新的檔案（可能還在錄製）
                age = time.time() - mkv.stat().st_mtime
                if age < 30:  # 至少 30 秒沒更新才轉檔
                    continue
                
                # 跳過已有對應 mp4 的檔案
                mp4 = mkv.with_suffix(".mp4")
                if mp4.exists():
                    continue
                
                # 再次檢查檔案是否存在（防止競爭）
                if not mkv.exists():
                    continue
                
                # 轉檔
                log(f"背景轉檔: {mkv.name}", "INFO")
                result = convert_to_mp4(mkv)
                if result:
                    log(f"轉檔完成: {result.name}", "OK")
                    _stats.record_conversion(True)
                    _stats.record_segment()  # 成功轉檔代表一個完整片段
                else:
                    _stats.record_conversion(False)
            except FileNotFoundError:
                pass  # 檔案已被處理，忽略
            except Exception as e:
                log(f"轉檔失敗 {mkv.name}: {e}", "ERROR")
                _stats.record_conversion(False)
    finally:
        _is_converting = False


# ============================================================================
# GCS 上傳
# ============================================================================

def upload_to_gcs(file_path: Path) -> dict:
    """上傳單一檔案到 GCS"""
    try:
        from google.cloud import storage
    except ImportError:
        log("請安裝 google-cloud-storage: pip install google-cloud-storage", "ERROR")
        return {"file": file_path.name, "success": False, "error": "Missing dependency"}

    creds_path = CONFIG["gcs_credentials"]
    if creds_path and creds_path.exists():
        client = storage.Client.from_service_account_json(str(creds_path))
    else:
        client = storage.Client()
    
    bucket = client.bucket(CONFIG["gcs_bucket"])
    
    # 自動加上日期前綴
    date_prefix = datetime.now().strftime("%Y/%m/%d")
    prefix = CONFIG["gcs_prefix"]
    if prefix:
        blob_name = f"{prefix}/{date_prefix}/{file_path.name}"
    else:
        blob_name = f"{date_prefix}/{file_path.name}"
    
    blob = bucket.blob(blob_name)
    
    file_size = file_path.stat().st_size
    log(f"上傳中: {file_path.name} ({file_size / 1024 / 1024:.1f} MB)", "UP")
    
    blob.upload_from_filename(str(file_path))
    
    return {
        "file": file_path.name,
        "path": file_path,
        "url": f"gs://{CONFIG['gcs_bucket']}/{blob_name}",
        "size": file_size,
        "success": True,
    }


def upload_files(files: list[Path]) -> list:
    """批次上傳檔案（上傳成功後記錄，不立即刪除）"""
    results = []
    
    with ThreadPoolExecutor(max_workers=1) as executor:
        futures = {executor.submit(upload_to_gcs, f): f for f in files}
        
        for future in as_completed(futures):
            file_path = futures[future]
            try:
                result = future.result()
                results.append(result)
                
                if result.get("success"):
                    log(f"✅ {result['file']} -> {result['url']}", "OK")
                    # 記錄已上傳，保留本地檔案供回滾
                    save_uploaded_record(result['file'], result['url'])
                    _stats.record_upload(True, result.get('size', 0))
                else:
                    log(f"❌ {result['file']}: {result.get('error')}", "ERROR")
                    _stats.record_upload(False, error=result.get('error'))
            except Exception as e:
                results.append({
                    "file": file_path.name,
                    "success": False,
                    "error": str(e),
                })
                log(f"❌ {file_path.name}: {e}", "ERROR")
                _stats.record_upload(False, error=str(e))
    
    return results


def try_background_upload():
    """嘗試背景上傳（非阻塞）"""
    global _is_uploading
    
    with _upload_lock:
        if _is_uploading:
            return
        _is_uploading = True
    
    try:
        files = get_pending_files()
        if not files:
            return
        
        # 只上傳一個檔案（依序上傳，對網路衝擊最小）
        batch = files[:1]
        log(f"背景上傳 {len(batch)} 個檔案...", "UP")
        upload_files(batch)
    finally:
        with _upload_lock:
            _is_uploading = False


def run_upload_loop():
    """上傳主迴圈（閒置時間執行）"""
    global _stop_requested
    
    last_cleanup = datetime.min
    
    while not _stop_requested:
        # 每小時執行一次清理
        if (datetime.now() - last_cleanup).total_seconds() > 3600:
            deleted = cleanup_old_files()
            if deleted > 0:
                log(f"清理完成，刪除 {deleted} 個過期檔案", "OK")
                _stats.record_cleanup(deleted)
            last_cleanup = datetime.now()
        
        # 檢查是否為上傳時間
        if not is_upload_time():
            time.sleep(60)
            continue
        
        files = get_pending_files()
        if not files:
            log("沒有待上傳的檔案", "INFO")
            time.sleep(300)  # 5 分鐘後再檢查
            continue
        
        log(f"開始上傳 {len(files)} 個檔案", "UP")
        upload_files(files)
        
        time.sleep(60)  # 上傳完畢後等待 1 分鐘


# ============================================================================
# 主程式
# ============================================================================

def handle_signal(signum, frame):
    """處理停止信號"""
    global _stop_requested
    _stop_requested = True
    log("收到停止信號，正在優雅關閉...", "WARN")


def cmd_daemon(args):
    """自動排程錄製與上傳"""
    # 驗證必填設定
    validate_config(require_rtsp=True, require_gcs=True)
    
    # 初始化日誌系統
    setup_file_logger()
    
    # 啟動統計
    _stats.start()
    
    signal.signal(signal.SIGINT, handle_signal)
    signal.signal(signal.SIGTERM, handle_signal)
    
    log("=" * 60, "INFO")
    log("RTSP 錄製程式啟動", "INFO")
    log(f"  錄製模式: 無縫模式 🚀", "INFO")
    log(f"  錄製時間: {CONFIG['record_start_hour']:02d}:00 - {CONFIG['record_end_hour']:02d}:00", "INFO")
    log(f"  上傳時間: {CONFIG['record_end_hour']:02d}:00 - {CONFIG['record_start_hour']:02d}:00", "INFO")
    log(f"  頻道: {CONFIG['channels']}", "INFO")
    log(f"  每段: {CONFIG['segment_duration']}s", "INFO")
    log(f"  輸出: {CONFIG['output_dir']}", "INFO")
    log(f"  GCS: gs://{CONFIG['gcs_bucket']}/", "INFO")
    log("=" * 60, "INFO")
    
    # 啟動無縫錄製執行緒
    record_thread = threading.Thread(target=run_seamless_recording_loop, daemon=True)
    record_thread.start()
    
    # 啟動上傳執行緒
    upload_thread = threading.Thread(target=run_upload_loop, daemon=True)
    upload_thread.start()
    
    # 主執行緒等待，並定期輸出統計報告
    while not _stop_requested:
        # 每小時輸出統計報告
        if _stats.should_report():
            _stats.log_report()
        time.sleep(1)
    
    # 輸出最終統計報告
    log("輸出最終統計報告...", "INFO")
    _stats.log_report()
    
    log("等待執行緒結束...", "INFO")
    record_thread.join(timeout=30)
    upload_thread.join(timeout=30)
    log("程式已停止", "INFO")


def cmd_record(args):
    """手動錄製"""
    # 驗證必填設定（允許 CLI 覆蓋 RTSP URL）
    if not args.url:
        validate_config(require_rtsp=True)
    
    channels = []
    for part in args.channels.split(","):
        if "-" in part:
            start, end = part.split("-", 1)
            channels.extend(range(int(start), int(end) + 1))
        else:
            channels.append(int(part))
    
    CONFIG["channels"] = channels
    CONFIG["segment_duration"] = args.duration
    CONFIG["output_dir"] = Path(args.output)
    
    if args.url:
        CONFIG["rtsp_base_url"] = args.url
    
    signal.signal(signal.SIGINT, handle_signal)
    signal.signal(signal.SIGTERM, handle_signal)
    
    log(f"手動錄製: {channels}, {args.duration}s", "REC")
    
    # 暫時允許任何時間錄製
    CONFIG["record_start_hour"] = 0
    CONFIG["record_end_hour"] = 24
    
    run_recording_loop()


def cmd_upload(args):
    """手動上傳"""
    files = get_pending_files()
    
    if not files:
        log("沒有待上傳的檔案", "INFO")
        return
    
    if args.dry_run:
        log(f"找到 {len(files)} 個檔案:", "INFO")
        for f in files:
            print(f"  {f.name} ({f.stat().st_size / 1024 / 1024:.1f} MB)")
        return
    
    log(f"開始上傳 {len(files)} 個檔案...", "UP")
    results = upload_files(files)
    
    success = sum(1 for r in results if r.get("success"))
    log(f"上傳完成: {success}/{len(results)}", "OK")


def cmd_status(args):
    """顯示狀態"""
    now = datetime.now()
    
    print("=" * 50)
    print(f"📊 RTSP 錄製狀態")
    print("=" * 50)
    print(f"  目前時間: {now.strftime('%Y-%m-%d %H:%M:%S')}")
    print(f"  錄製時間: {'是' if is_recording_time() else '否'}")
    print(f"  上傳時間: {'是' if is_upload_time() else '否'}")
    
    # 網路狀態
    network_usage = get_network_usage_mbps(CONFIG["network_interface"])
    print(f"  網路使用: {network_usage:.1f} Mbps")
    print(f"  網路閒置: {'是' if is_network_idle() else '否'}")
    
    # 檔案狀態
    files = get_pending_files()
    total_size = sum(f.stat().st_size for f in files) if files else 0
    uploaded = load_uploaded_records()
    print(f"  待上傳: {len(files)} 個檔案 ({total_size / 1024 / 1024:.1f} MB)")
    print(f"  已上傳: {len(uploaded)} 個檔案（等待清理）")
    print(f"  保留時限: {CONFIG['max_retention_hours']} 小時")
    
    # 磁碟空間
    output_dir = CONFIG["output_dir"]
    if output_dir.exists():
        import shutil
        total, used, free = shutil.disk_usage(output_dir)
        print(f"  磁碟空間: {free / 1024 / 1024 / 1024:.1f} GB 可用")
    
    # 統計資訊（從檔案讀取，支援查看正在運行的 daemon）
    saved_stats = Stats.load_from_file()
    if saved_stats:
        # 計算運行時間
        start_time = saved_stats.get("start_time")
        if start_time:
            try:
                start_dt = datetime.fromisoformat(start_time)
                delta = datetime.now() - start_dt
                hours, remainder = divmod(int(delta.total_seconds()), 3600)
                minutes, seconds = divmod(remainder, 60)
                uptime = f"{hours}h {minutes}m {seconds}s"
            except Exception:
                uptime = "未知"
        else:
            uptime = "未啟動"
        
        rec = saved_stats.get("recording", {})
        conv = saved_stats.get("conversion", {})
        up = saved_stats.get("upload", {})
        clean = saved_stats.get("cleanup", {})
        
        print("-" * 50)
        print(f"📈 統計（運行 {uptime}）")
        print(f"  錄製片段: {rec.get('segments', 0)}")
        print(f"  ffmpeg 重啟: {rec.get('ffmpeg_restarts', 0)}")
        print(f"  轉檔: ✅{conv.get('success', 0)} ❌{conv.get('failed', 0)}")
        upload_mb = round(up.get('bytes', 0) / 1024 / 1024, 2)
        print(f"  上傳: ✅{up.get('success', 0)} ❌{up.get('failed', 0)} ({upload_mb} MB)")
        print(f"  清理: {clean.get('files_deleted', 0)} 檔案")
        
        # 顯示最近錯誤
        ffmpeg_errors = rec.get("ffmpeg_errors", {})
        if ffmpeg_errors:
            print(f"  ⚠️ 有錯誤的頻道: {list(ffmpeg_errors.keys())}")
        
        upload_errors = up.get("errors", [])
        if upload_errors:
            last_err = upload_errors[-1]
            print(f"  ⚠️ 最近上傳錯誤: {last_err.get('error', '')[:40]}")
        
        # 最後更新時間
        last_update = saved_stats.get("last_update")
        if last_update:
            print(f"  最後更新: {last_update}")
    
    print("=" * 50)


def cmd_cleanup(args):
    """手動清理已上傳且過期的檔案"""
    CONFIG["max_retention_hours"] = args.hours
    output_dir = CONFIG["output_dir"]
    
    if not output_dir.exists():
        log("輸出目錄不存在", "INFO")
        return
    
    max_age = timedelta(hours=args.hours)
    cutoff = datetime.now() - max_age
    
    # 載入已上傳記錄
    uploaded = load_uploaded_records()
    
    # 找出已上傳且過期的檔案
    expired = []
    for pattern in ("*.mp4", "*.mkv"):
        for f in output_dir.glob(pattern):
            try:
                # 只清理已上傳的檔案
                if f.name not in uploaded:
                    continue
                mtime = datetime.fromtimestamp(f.stat().st_mtime)
                if mtime < cutoff:
                    expired.append((f, mtime, f.stat().st_size))
            except Exception:
                pass
    
    if not expired:
        log(f"沒有已上傳且超過 {args.hours} 小時的檔案", "INFO")
        return
    
    total_size = sum(size for _, _, size in expired)
    log(f"找到 {len(expired)} 個過期檔案 ({total_size / 1024 / 1024:.1f} MB)", "INFO")
    
    if args.dry_run:
        print("\n以下檔案會被刪除:")
        for f, mtime, size in sorted(expired, key=lambda x: x[1]):
            age_hours = (datetime.now() - mtime).total_seconds() / 3600
            print(f"  {f.name} ({size / 1024 / 1024:.1f} MB, {age_hours:.1f}h ago)")
        return
    
    deleted = 0
    updated_records = dict(uploaded)
    for f, _, _ in expired:
        try:
            f.unlink()
            deleted += 1
            # 從記錄中移除
            updated_records.pop(f.name, None)
        except Exception as e:
            log(f"刪除失敗 {f.name}: {e}", "ERROR")
    
    # 更新記錄檔
    if deleted > 0:
        get_uploaded_records_path().write_text(
            json.dumps(updated_records, indent=2, ensure_ascii=False)
        )
    
    log(f"清理完成，刪除 {deleted} 個檔案", "OK")


def cmd_install_service(args):
    """安裝 systemd 服務（互動式）"""
    script_path = Path(__file__).resolve()
    working_dir = script_path.parent
    
    service_content = f"""[Unit]
Description=RTSP Recorder Daemon
After=network.target

[Service]
Type=simple
User={os.getenv('USER')}
WorkingDirectory={working_dir}
ExecStart={sys.executable} {script_path} daemon
Restart=always
RestartSec=10

[Install]
WantedBy=multi-user.target
"""
    
    service_path = Path("/etc/systemd/system/rtsp-recorder.service")
    
    print("📋 systemd 服務內容:")
    print("-" * 50)
    print(service_content)
    print("-" * 50)
    
    # 互動確認
    try:
        answer = input("\n是否安裝此服務？[y/N] ").strip().lower()
    except (EOFError, KeyboardInterrupt):
        print("\n已取消")
        return
    
    if answer not in ("y", "yes"):
        print("已取消")
        return
    
    # 寫入暫存檔
    import tempfile
    with tempfile.NamedTemporaryFile(mode="w", suffix=".service", delete=False) as f:
        f.write(service_content)
        tmp_path = f.name
    
    try:
        # 安裝服務
        print("\n🔧 安裝服務...")
        cmds = [
            ["sudo", "cp", tmp_path, str(service_path)],
            ["sudo", "systemctl", "daemon-reload"],
            ["sudo", "systemctl", "enable", "rtsp-recorder"],
        ]
        
        for cmd in cmds:
            print(f"  $ {' '.join(cmd)}")
            result = subprocess.run(cmd)
            if result.returncode != 0:
                print(f"❌ 執行失敗: {' '.join(cmd)}")
                return
        
        print("\n✅ 服務已安裝！")
        print("\n啟動服務:")
        print("  sudo systemctl start rtsp-recorder")
        print("\n查看狀態:")
        print("  sudo systemctl status rtsp-recorder")
        print("  journalctl -u rtsp-recorder -f")
        
        # 詢問是否立即啟動
        try:
            start = input("\n是否立即啟動服務？[y/N] ").strip().lower()
        except (EOFError, KeyboardInterrupt):
            print()
            return
        
        if start in ("y", "yes"):
            subprocess.run(["sudo", "systemctl", "start", "rtsp-recorder"])
            subprocess.run(["sudo", "systemctl", "status", "rtsp-recorder", "--no-pager"])
    
    finally:
        # 清理暫存檔
        Path(tmp_path).unlink(missing_ok=True)


def main():
    parser = argparse.ArgumentParser(
        description="RTSP 錄製 + 雲端上傳整合工具",
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    
    subparsers = parser.add_subparsers(dest="command", help="子命令")
    
    # daemon 命令
    p_daemon = subparsers.add_parser("daemon", help="🚀 啟動背景服務（自動錄製 + 上傳）")
    p_daemon.set_defaults(func=cmd_daemon)
    
    # record 命令
    p_record = subparsers.add_parser("record", help="🎥 手動錄製（單次）")
    p_record.add_argument("-d", "--duration", type=int, default=600, help="錄製時間（秒）")
    p_record.add_argument("-c", "--channels", type=str, default="1-10", help="頻道範圍")
    p_record.add_argument("-o", "--output", type=str, default=str(CONFIG["output_dir"]), help="輸出目錄")
    p_record.add_argument("--url", type=str, help="RTSP 基底 URL")
    p_record.set_defaults(func=cmd_record)
    
    # upload 命令
    p_upload = subparsers.add_parser("upload", help="☁️ 手動上傳到 GCS")
    p_upload.add_argument("--dry-run", action="store_true", help="只顯示檔案，不實際上傳")
    p_upload.add_argument("--keep", action="store_true", help="上傳後保留本地檔案")
    p_upload.set_defaults(func=cmd_upload)
    
    # status 命令
    p_status = subparsers.add_parser("status", help="📊 查看目前狀態")
    p_status.set_defaults(func=cmd_status)
    
    # cleanup 命令
    p_cleanup = subparsers.add_parser("cleanup", help="🗑️ 清理已上傳的過期檔案")
    p_cleanup.add_argument("--hours", type=int, default=CONFIG["max_retention_hours"], help=f"保留時限（小時），預設 {CONFIG['max_retention_hours']}")
    p_cleanup.add_argument("--dry-run", action="store_true", help="只顯示會刪除的檔案")
    p_cleanup.set_defaults(func=cmd_cleanup)
    
    # install-service 命令
    p_service = subparsers.add_parser("install-service", help="⚙️ 安裝為系統服務（開機自啟）")
    p_service.set_defaults(func=cmd_install_service)
    
    # --daemon 快捷方式
    parser.add_argument("--daemon", action="store_true", help="同 daemon 子命令，啟動背景服務")
    
    args = parser.parse_args()
    
    if args.daemon:
        cmd_daemon(args)
    elif hasattr(args, "func"):
        args.func(args)
    else:
        parser.print_help()


if __name__ == "__main__":
    main()
