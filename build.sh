#!/usr/bin/env bash
# OneMix 一键构建：core 单测 + CLI + GUI 原生二进制 + NN 模型 + SXSEditor 插件。
set -euo pipefail
cd "$(dirname "$0")"
ROOT=$PWD
export ORT_DYLIB_PATH="$ROOT/models/native/libonnxruntime.so"

echo "═══ 1/5 生成 NN 模型（首次） ═══"
[ -f models/onemix_dyn_enhance_v1.onnx ] || python3 scripts/gen_enhance_model.py

echo "═══ 2/5 生成测试素材 ═══"
python3 scripts/gen_fixtures.py

echo "═══ 3/5 运行 core 单元测试 ═══"
cargo test -p mixer-core -q

echo "═══ 4/5 构建 CLI 与 GUI ═══"
cargo build --release -p mixer-cli -p mixer-app

echo "═══ 5/5 能力一致性校验 ═══"
bash scripts/verify_parity.sh

echo ""
echo "✓ 全部完成。产物："
echo "  CLI : target/release/mixer-cli"
echo "  GUI : target/release/mixer-app"
echo "  插件: dist-plugins/onemix-mixer-0.1.0.sxsp"
