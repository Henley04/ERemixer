#!/usr/bin/env python3
"""下载 StemFX 权重与上游代码，并导出 W8A32 ONNX 到 `models/stemfx/`。

仓库里已经有一份导出的 `models/stemfx/*.onnx`（内嵌进二进制用），
只有在需要重新导出（换权重 / 换量化参数）时才需要跑这个脚本。

数据来源（均走国内镜像）：
  * 权重：https://hf-mirror.com/barry-mir/stemfx-bsfilm  (best_checkpoint.pt, 110 MB)
  * 代码：https://gh-proxy.com/https://github.com/barry-mir/stemfx.git

用法：
    python scripts/fetch_stemfx.py                # 下载 + 导出（约 5 分钟，CPU 即可）
    python scripts/fetch_stemfx.py --skip-export  # 只下载
    python scripts/fetch_stemfx.py --force        # 忽略缓存重新下载

依赖（导出阶段需要，Torch 仅 CPU 版即可）：
    pip install torch --index-url https://pypi.tuna.tsinghua.edu.cn/simple
    pip install onnx onnxruntime onnx_ir numpy
    pip install --no-deps multiafx librosa audiomentations loudness pyloudnorm soundfile
"""

from __future__ import annotations

import argparse
import hashlib
import shutil
import subprocess
import sys
import urllib.request
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent

CKPT_URL = "https://hf-mirror.com/barry-mir/stemfx-bsfilm/resolve/main/best_checkpoint.pt"
CKPT_SHA256 = "4a1a9879f8e4605679431733af92eb921090afc6ac7d6bffce51cdd6cb69e40f"
UPSTREAM = "https://gh-proxy.com/https://github.com/barry-mir/stemfx.git"


def human(n: float) -> str:
    return f"{n / 1048576:.1f} MB"


def download(url: str, dest: Path, force: bool = False) -> Path:
    if dest.exists() and not force:
        print(f"[skip] 已存在 {dest} ({human(dest.stat().st_size)})")
        return dest
    dest.parent.mkdir(parents=True, exist_ok=True)
    tmp = dest.with_suffix(dest.suffix + ".part")
    print(f"[get ] {url}")
    with urllib.request.urlopen(url, timeout=60) as resp:
        total = int(resp.headers.get("Content-Length") or 0)
        done = 0
        with tmp.open("wb") as f:
            while True:
                chunk = resp.read(1 << 20)
                if not chunk:
                    break
                f.write(chunk)
                done += len(chunk)
                if total:
                    pct = done * 100 // total
                    print(f"\r       {human(done)} / {human(total)}  {pct:3d}%", end="")
        print()
    tmp.replace(dest)
    return dest


def verify(path: Path) -> None:
    print("[hash] 校验 sha256 …")
    h = hashlib.sha256()
    with path.open("rb") as f:
        for chunk in iter(lambda: f.read(1 << 22), b""):
            h.update(chunk)
    got = h.hexdigest()
    if got != CKPT_SHA256:
        path.unlink(missing_ok=True)
        raise SystemExit(f"校验失败：期望 {CKPT_SHA256}，实际 {got}（文件已删除，请重跑）")
    print("       OK")


def ensure_repo(workdir: Path) -> Path:
    repo = workdir / "stemfx"
    if (repo / "stemfx" / "model.py").exists():
        print(f"[skip] 上游代码已存在 {repo}")
        return repo
    if not shutil.which("git"):
        raise SystemExit("需要 git 才能拉取上游代码")
    print(f"[git ] clone {UPSTREAM}")
    subprocess.run(["git", "clone", "--depth", "1", UPSTREAM, str(repo)], check=True)
    return repo


def check_deps() -> None:
    missing = []
    for mod, pip in [
        ("torch", "torch"),
        ("onnx", "onnx"),
        ("onnxruntime", "onnxruntime"),
        ("onnx_ir", "onnx_ir"),
        ("numpy", "numpy"),
    ]:
        try:
            __import__(mod)
        except ImportError:
            missing.append(pip)
    if missing:
        raise SystemExit(
            "导出依赖缺失：" + " ".join(missing) + "\n"
            "  pip install torch --index-url https://pypi.tuna.tsinghua.edu.cn/simple\n"
            "  pip install onnx onnxruntime onnx_ir numpy\n"
            "  pip install --no-deps multiafx librosa audiomentations loudness pyloudnorm soundfile"
        )


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--workdir", default="build/stemfx", help="权重与上游代码的缓存目录")
    ap.add_argument("--out", default="models/stemfx", help="ONNX 输出目录")
    ap.add_argument("--skip-export", action="store_true", help="只下载，不导出")
    ap.add_argument("--force", action="store_true", help="忽略缓存重新下载")
    args = ap.parse_args()

    workdir = (ROOT / args.workdir).resolve()
    out = (ROOT / args.out).resolve()

    ckpt = download(CKPT_URL, workdir / "best_checkpoint.pt", args.force)
    if args.force or not (out / "generator_w8a32.onnx").exists():
        verify(ckpt)
    repo = ensure_repo(workdir)

    if args.skip_export:
        print("✓ 下载完成（--skip-export）")
        return 0

    check_deps()
    cmd = [
        sys.executable,
        str(ROOT / "tools" / "stemfx_export" / "export_stemfx.py"),
        "--ckpt", str(ckpt),
        "--repo", str(repo),
        "--out", str(out),
    ]
    print("[exp ] " + " ".join(cmd[1:]))
    subprocess.run(cmd, check=True)
    print(f"✓ 导出完成：{out}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
