#!/usr/bin/env python3
"""组装 ERemixer 发布包（Linux tar.gz / Windows zip）。

包内布局（两个平台一致）：

    ERemixer-v<ver>-<platform>/
      mixer-cli[.exe]
      mixer-app[.exe]
      models/onemix_dyn_enhance_v1.onnx
      models/native/<onnxruntime 动态库>
      onemix-mixer-<ver>.sxsp
      README.md
      docs/
      RUNNING.txt

`mixer-core` 会从可执行文件同级的 `models/native/` 自动发现 ONNX
Runtime，因此解压即用，无需设置环境变量。
"""

from __future__ import annotations

import argparse
import re
import shutil
import sys
import tarfile
import zipfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent

# Windows 控制台默认 cp1252，直接打印 ✓ 会抛 UnicodeEncodeError。
if hasattr(sys.stdout, "reconfigure"):
    sys.stdout.reconfigure(encoding="utf-8", errors="replace")
    sys.stderr.reconfigure(encoding="utf-8", errors="replace")

RUNNING = """ERemixer v{version} — {platform}
========================================

1) 命令行一键混音（分轨直入，自动识别乐器）
   {cli} mix --stems vocal.wav --stems drums.wav --stems bass.wav -o final.wav --json

2) 传统两轨
   {cli} mix -v vocals.wav -a instrumental.wav -o final.wav --preset streaming

3) 查看支持的输入模式 / 乐器类型
   {cli} modes
   {cli} instruments

4) 桌面 GUI
   {gui}

5) SXSEditor 插件
   把 onemix-mixer-{version}.sxsp 拖入「插件 → 插件管理器 → 安装插件」。

完整说明见 README.md 与 docs/。
"""


def workspace_version() -> str:
    text = (ROOT / "Cargo.toml").read_text(encoding="utf-8")
    section = text.split("[workspace.package]", 1)[-1]
    match = re.search(r'^version\s*=\s*"([^"]+)"', section, re.MULTILINE)
    if not match:
        raise SystemExit("无法从 Cargo.toml 读取 workspace 版本")
    return match.group(1)


def copy_into(src: Path, dst: Path) -> None:
    if not src.exists():
        raise SystemExit(f"缺少必需文件: {src}")
    dst.parent.mkdir(parents=True, exist_ok=True)
    if src.is_dir():
        shutil.copytree(src, dst, dirs_exist_ok=True)
    else:
        shutil.copy2(src, dst)


def make_archive(stage: Path, output: Path, kind: str) -> None:
    if kind == "zip":
        with zipfile.ZipFile(output, "w", zipfile.ZIP_DEFLATED, compresslevel=9) as zf:
            for path in sorted(stage.rglob("*")):
                if path.is_file():
                    zf.write(path, path.relative_to(stage.parent))
    else:
        with tarfile.open(output, "w:gz") as tf:
            tf.add(stage, arcname=stage.name, filter=_tar_filter)


def _tar_filter(info: tarfile.TarInfo) -> tarfile.TarInfo:
    # 归一化权限，避免把宿主机的 umask/属主带进发布包。
    if info.isdir():
        info.mode = 0o755
    elif info.name.endswith((".sh", ".py")) or "/mixer-" in info.name:
        info.mode = 0o755
    else:
        info.mode = 0o644
    info.uid = info.gid = 0
    info.uname = info.gname = ""
    return info


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--platform", required=True, choices=["linux-x86_64", "windows-x86_64"])
    parser.add_argument("--version", default=None)
    parser.add_argument("--dist", default="dist")
    args = parser.parse_args()

    version = args.version or workspace_version()
    windows = args.platform.startswith("windows")
    exe = ".exe" if windows else ""
    target = ROOT / "target" / "release"

    stage_name = f"ERemixer-v{version}-{args.platform}"
    stage = ROOT / args.dist / stage_name
    if stage.exists():
        shutil.rmtree(stage)
    stage.mkdir(parents=True)

    copy_into(target / f"mixer-cli{exe}", stage / f"mixer-cli{exe}")
    copy_into(target / f"mixer-app{exe}", stage / f"mixer-app{exe}")
    copy_into(ROOT / "models" / "onemix_dyn_enhance_v1.onnx", stage / "models" / "onemix_dyn_enhance_v1.onnx")

    native = sorted((ROOT / "models" / "native").glob("*")) if (ROOT / "models" / "native").exists() else []
    if not native:
        raise SystemExit("models/native 为空：请先运行 scripts/fetch_ort.py")
    for lib in native:
        if lib.is_file():
            copy_into(lib, stage / "models" / "native" / lib.name)

    plugin = ROOT / "dist-plugins" / f"onemix-mixer-{version}.sxsp"
    copy_into(plugin, stage / plugin.name)
    copy_into(ROOT / "README.md", stage / "README.md")
    docs = ROOT / "docs"
    if docs.is_dir():
        copy_into(docs, stage / "docs")

    (stage / "RUNNING.txt").write_text(
        RUNNING.format(
            version=version,
            platform=args.platform,
            cli=f"./mixer-cli{exe}" if not windows else f"mixer-cli{exe}",
            gui=f"./mixer-app{exe}" if not windows else f"mixer-app{exe}",
        ),
        encoding="utf-8",
    )

    dist = ROOT / args.dist
    dist.mkdir(exist_ok=True)
    if windows:
        archive = dist / f"{stage_name}.zip"
        make_archive(stage, archive, "zip")
    else:
        archive = dist / f"{stage_name}.tar.gz"
        make_archive(stage, archive, "tar")

    size = archive.stat().st_size / 1048576
    print(f"✓ {archive}  ({size:.1f} MB)")
    for item in sorted(stage.rglob("*")):
        if item.is_file():
            print(f"   {item.relative_to(stage)}  {item.stat().st_size / 1048576:.1f} MB")
    return 0


if __name__ == "__main__":
    sys.exit(main())
