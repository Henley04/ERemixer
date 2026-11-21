#!/usr/bin/env python3
"""从已安装的 onnxruntime Python wheel 中提取原生动态库。

CI 里先 `pip install onnxruntime==<version>`，再运行本脚本，把
`onnxruntime/capi/` 下的动态库复制到 `models/native/`，供
`mixer-core` 的 `load-dynamic` 模式在运行时加载。

这样 Windows / Linux 用同一份版本来源，避免依赖 GitHub Release
资产命名的差异。
"""

from __future__ import annotations

import argparse
import shutil
import sys
from pathlib import Path

import onnxruntime

# Windows 控制台编码可能不是 UTF-8，避免打印中文/符号时崩溃。
if hasattr(sys.stdout, "reconfigure"):
    sys.stdout.reconfigure(encoding="utf-8", errors="replace")
    sys.stderr.reconfigure(encoding="utf-8", errors="replace")


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--dest", default="models/native", help="目标目录")
    args = parser.parse_args()

    capi = Path(onnxruntime.__file__).resolve().parent / "capi"
    if not capi.is_dir():
        print(f"找不到 onnxruntime capi 目录: {capi}", file=sys.stderr)
        return 1

    dest = Path(args.dest)
    dest.mkdir(parents=True, exist_ok=True)

    # 清掉上一次提取的库，避免旧版本（如带 soname 的副本）混入发布包。
    for stale in list(dest.glob("libonnxruntime*")) + list(dest.glob("onnxruntime*.dll")):
        if stale.is_file():
            stale.unlink()

    copied: list[Path] = []

    if sys.platform.startswith("win"):
        src = capi / "onnxruntime.dll"
        if not src.exists():
            print(f"找不到 {src}", file=sys.stderr)
            return 1
        target = dest / "onnxruntime.dll"
        shutil.copy2(src, target)
        copied.append(target)
        # CPU EP 通常内置于主库；如存在 provider 共享库则一并带上。
        for extra in capi.glob("onnxruntime_providers_*.dll"):
            out = dest / extra.name
            shutil.copy2(extra, out)
            copied.append(out)
    elif sys.platform == "darwin":
        libs = sorted(capi.glob("libonnxruntime*.dylib"))
        if not libs:
            print(f"{capi} 下没有 libonnxruntime*.dylib", file=sys.stderr)
            return 1
        for lib in libs:
            out = dest / lib.name
            shutil.copy2(lib, out)
            copied.append(out)
    else:
        # Linux wheel 里是 libonnxruntime.so.1.24.1，引擎按固定名探测。
        versioned = sorted(capi.glob("libonnxruntime.so.*"))
        if not versioned:
            print(f"{capi} 下没有 libonnxruntime.so.*", file=sys.stderr)
            return 1
        target = dest / "libonnxruntime.so"
        shutil.copy2(versioned[-1], target)
        copied.append(target)
        for extra in capi.glob("libonnxruntime_providers_*.so"):
            out = dest / extra.name
            shutil.copy2(extra, out)
            copied.append(out)

    print(f"onnxruntime {onnxruntime.__version__} -> {dest}")
    for path in copied:
        print(f"  {path}  ({path.stat().st_size / 1048576:.1f} MB)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
