#!/usr/bin/env bash
# 校验：Rust 引擎与插件 JS 引擎对同一素材输出一致（能力一致性回归测试）。
set -euo pipefail
cd "$(dirname "$0")/.."
CLI=target/debug/mixer-cli

echo "== 构建 CLI =="
ORT_DYLIB_PATH="$PWD/models/native/libonnxruntime.so" cargo build -p mixer-cli -q

echo "== 生成测试素材 =="
python3 scripts/gen_fixtures.py
python3 scripts/gen_stems_fixtures.py

echo "== Rust 混音（传统两轨）=="
"$CLI" mix -v examples/fixtures/vocals.wav -a examples/fixtures/accompaniment.wav \
  -o /tmp/onemix_rust.wav --quiet --json > /tmp/onemix_rust.json

echo "== Rust 混音（多轨分轨直入）=="
"$CLI" mix \
  --stems examples/fixtures/stems/lead_vocals.wav \
  --stems examples/fixtures/stems/drum_kit.wav \
  --stems examples/fixtures/stems/electric_bass.wav \
  --stems examples/fixtures/stems/guitar_keys.wav \
  -o /tmp/onemix_rust_stems.wav --quiet --json > /tmp/onemix_rust_stems.json

echo "== JS 插件引擎混音（两轨 + 多轨）=="
node - <<'NODE'
const fs=require('fs');
const engine=require('./sxseditor-plugin/engine.js');
const ctx={
  fs:{readBinary:async p=>new Uint8Array(fs.readFileSync(p)),writeBinary:async(p,b)=>fs.writeFileSync(p,Buffer.from(b))},
  ui:{progress:()=>{}},log:{warn:()=>{},info:()=>{}},
  ort:{load:async()=>null,run:async()=>({}),unload:()=>{}},
};
(async()=>{
  const r=await engine.runMix(ctx,{...engine.DEFAULT_REQUEST,
    vocals_path:'examples/fixtures/vocals.wav',
    accompaniment_path:'examples/fixtures/accompaniment.wav',
    output_path:'/tmp/onemix_js.wav',preset:'streaming'});
  fs.writeFileSync('/tmp/onemix_js.json',JSON.stringify(r));
  const b='examples/fixtures/stems/';
  const rs=await engine.runMix(ctx,{...engine.DEFAULT_REQUEST,mode:'stems',
    stems:[
      {path:b+'lead_vocals.wav',instrument:'auto',gain_db:0},
      {path:b+'drum_kit.wav',instrument:'auto',gain_db:0},
      {path:b+'electric_bass.wav',instrument:'auto',gain_db:0},
      {path:b+'guitar_keys.wav',instrument:'auto',gain_db:0}],
    output_path:'/tmp/onemix_js_stems.wav',preset:'streaming'});
  fs.writeFileSync('/tmp/onemix_js_stems.json',JSON.stringify(rs));
})();
NODE

echo "== 对比关键指标 =="
python3 - <<'PY'
import json
def check(rj_path, jj_path, label):
    r=json.load(open(rj_path)); j=json.load(open(jj_path))
    print(f"-- {label} --")
    print(f"{'指标':<22}{'Rust':>12}{'JS插件':>12}")
    for k in ['output_lufs','output_true_peak_dbtp','duration_sec','sample_rate','mode']:
        rv=round(r[k],3) if isinstance(r[k],float) else r[k]
        jv=round(j[k],3) if isinstance(j[k],float) else j[k]
        print(f"{k:<22}{str(rv):>12}{str(jv):>12}")
    assert abs(r['output_lufs']-j['output_lufs'])<0.2, f"{label}: 输出响度不一致"
    assert r['sample_rate']==j['sample_rate'], f"{label}: 采样率不一致"
    assert abs(r['duration_sec']-j['duration_sec'])<0.05, f"{label}: 时长不一致"
    assert r['mode']==j['mode'], f"{label}: 模式不一致"
    ri=[s['instrument'] for s in r['stems_used']]
    ji=[s['instrument'] for s in j['stems_used']]
    assert ri==ji, f"{label}: 乐器推断不一致 {ri} vs {ji}"
check('/tmp/onemix_rust.json','/tmp/onemix_js.json','传统两轨')
check('/tmp/onemix_rust_stems.json','/tmp/onemix_js_stems.json','多轨分轨直入')
print("\n✓ Rust 与 JS 插件引擎能力一致（两轨 + 多轨，响度偏差 < 0.2 LUFS，乐器推断一致）")
PY
