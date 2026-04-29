#!/usr/bin/env python3
"""批次檢測影片健康狀態：能否播放、能否截圖"""

import subprocess
import sys
from pathlib import Path
from concurrent.futures import ThreadPoolExecutor, as_completed


def check_video(video_path: Path) -> dict:
    """檢測單一影片：格式驗證 + 嘗試截圖"""
    result = {
        "file": video_path.name,
        "size_mb": video_path.stat().st_size / (1024 * 1024),
        "playable": False,
        "can_screenshot": False,
        "duration": None,
        "error": None,
    }

    # 1) ffprobe 檢查格式是否完整
    probe_cmd = [
        "ffprobe", "-v", "error",
        "-show_entries", "format=duration",
        "-of", "csv=p=0",
        str(video_path),
    ]
    probe = subprocess.run(probe_cmd, capture_output=True, text=True, timeout=30)
    if probe.returncode != 0:
        result["error"] = probe.stderr.strip()[-200:]
        # 仍嘗試截圖
    else:
        try:
            result["duration"] = float(probe.stdout.strip())
            result["playable"] = True
        except ValueError:
            result["error"] = "無法解析 duration"

    # 2) 嘗試截圖（從第 1 秒）
    screenshot_cmd = [
        "ffmpeg", "-y",
        "-ss", "1",
        "-i", str(video_path),
        "-vframes", "1",
        "-f", "null", "-",
    ]
    shot = subprocess.run(
        screenshot_cmd, capture_output=True, text=True, timeout=30
    )
    result["can_screenshot"] = shot.returncode == 0

    return result


def main():
    video_dir = Path(sys.argv[1]) if len(sys.argv) > 1 else Path("/home/syncrobotic/Desktop/videos")
    videos = sorted(video_dir.glob("*.mp4")) + sorted(video_dir.glob("*.mkv"))

    if not videos:
        print("找不到影片檔案")
        return

    print(f"共 {len(videos)} 支影片，開始檢測...\n")

    ok_count = 0
    screenshot_only = []
    broken = []

    with ThreadPoolExecutor(max_workers=8) as pool:
        futures = {pool.submit(check_video, v): v for v in videos}
        done = 0
        for future in as_completed(futures):
            done += 1
            r = future.result()
            if r["playable"] and r["can_screenshot"]:
                ok_count += 1
            elif not r["playable"] and r["can_screenshot"]:
                screenshot_only.append(r)
            else:
                broken.append(r)

            if done % 50 == 0 or done == len(videos):
                print(f"  進度: {done}/{len(videos)}")

    # 輸出結果
    print(f"\n{'=' * 60}")
    print(f"檢測完成：共 {len(videos)} 支")
    print(f"  ✅ 正常可播放: {ok_count}")
    print(f"  ⚠️  無法播放但可截圖: {len(screenshot_only)}")
    print(f"  ❌ 完全損壞: {len(broken)}")
    print(f"{'=' * 60}")

    if screenshot_only:
        print(f"\n⚠️  無法播放但可截圖（{len(screenshot_only)} 支）：")
        for r in sorted(screenshot_only, key=lambda x: x["file"]):
            print(f"  {r['file']:50s}  {r['size_mb']:7.1f}MB  err: {r['error'] or 'N/A'}")

    if broken:
        print(f"\n❌ 完全損壞（{len(broken)} 支）：")
        for r in sorted(broken, key=lambda x: x["file"]):
            print(f"  {r['file']:50s}  {r['size_mb']:7.1f}MB  err: {r['error'] or 'N/A'}")

    # 寫報告到檔案
    report_path = video_dir / "health_report.txt"
    with open(report_path, "w") as f:
        f.write(f"影片健康檢測報告\n")
        f.write(f"目錄: {video_dir.resolve()}\n")
        f.write(f"總數: {len(videos)}\n")
        f.write(f"正常: {ok_count}\n")
        f.write(f"可截圖: {len(screenshot_only)}\n")
        f.write(f"損壞: {len(broken)}\n\n")
        if screenshot_only:
            f.write("--- 無法播放但可截圖 ---\n")
            for r in sorted(screenshot_only, key=lambda x: x["file"]):
                f.write(f"{r['file']}  {r['size_mb']:.1f}MB\n")
        if broken:
            f.write("\n--- 完全損壞 ---\n")
            for r in sorted(broken, key=lambda x: x["file"]):
                f.write(f"{r['file']}  {r['size_mb']:.1f}MB  err: {r['error']}\n")
    print(f"\n📄 報告已存至: {report_path}")


if __name__ == "__main__":
    main()
