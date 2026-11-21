'use strict';
/** OneMix 面板 ↔ 宿主桥（与 example-hello 同协议）。 */
const $ = (id) => document.getElementById(id);
const out = $('out');
const barFill = $('barFill');
const stage = $('stage');
let seq = 0;
const pending = new Map();

function log(text) {
  out.textContent = `[${new Date().toLocaleTimeString()}] ${text}\n` + out.textContent;
}

function call(method, params) {
  return new Promise((resolve, reject) => {
    const id = ++seq;
    pending.set(id, { resolve, reject });
    // 面板统一走 command.run scope，参数 {commandId, args}。
    const payload = (method === 'command.run')
      ? params
      : { commandId: method, args: params || {} };
    parent.postMessage({ v: 2, id, kind: 'req', method: 'command.run', params: payload }, '*');
    setTimeout(() => {
      if (!pending.has(id)) return;
      pending.delete(id);
      reject(new Error(`宿主未在 60s 内响应 ${method}`));
    }, 60000);
  });
}

function applyTheme(theme) {
  if (!theme) return;
  const s = document.documentElement.style;
  // 宿主下发的是 { bg, panel, fg, fgMuted, accent, border }；
  // 同时兼容我们自己的语义名（text/input）。
  const map = {
    bg: theme.bg,
    panel: theme.panel,
    text: theme.fg || theme.text,
    border: theme.border,
    input: theme.input || theme.panel,
    accent: theme.accent,
  };
  for (const [k, v] of Object.entries(map)) {
    if (v) s.setProperty(`--${k}`, v);
  }
}

window.addEventListener('message', (event) => {
  if (event.source !== window.parent) return;
  const data = event.data;
  if (!data || typeof data !== 'object' || data.v !== 2) return;
  if (data.kind === 'welcome') {
    applyTheme(data.theme);
    log(`已连接 OneMix 宿主 · ${data.locale || ''}`);
    return;
  }
  if (data.kind === 'evt') {
    if (data.event === 'theme') applyTheme(data.payload);
    if (data.event === 'progress' && data.payload) {
      // 宿主约定 0..1；兼容旧式 0..100。
      const raw = Number(data.payload.value ?? 0);
      const pct = Math.max(0, Math.min(100, raw <= 1 ? raw * 100 : raw));
      barFill.style.width = `${pct}%`;
      stage.textContent = data.payload.text || `${Math.round(pct)}%`;
    }
    return;
  }
  if (data.kind === 'res') {
    const e = pending.get(data.id);
    if (!e) return;
    pending.delete(data.id);
    if (data.ok) e.resolve(data.result);
    else e.reject(Object.assign(new Error((data.error && data.error.message) || '调用失败'), { code: data.error && data.error.code }));
  }
});

function buildRequest() {
  return {
    vocals_path: $('vocals').value.trim(),
    accompaniment_path: $('accom').value.trim(),
    output_path: $('output').value.trim(),
    preset: $('preset').value,
    target_lufs: Number($('lufs').value) || -14,
    vocal_gain_db: Number($('vgain').value) || 0,
    ducking_db: Number($('duck').value) || -2.5,
    nn_enhance: $('nn').checked,
  };
}

$('btnProbe').addEventListener('click', async () => {
  const req = buildRequest();
  for (const [key, label] of [['vocals_path', '人声'], ['accompaniment_path', '伴奏']]) {
    if (!req[key]) continue;
    try {
      const info = await call('command.run', { commandId: 'probe', args: { path: req[key] } });
      log(`${label}: ${info.sample_rate}Hz / ${info.channels}ch / ${info.duration_sec?.toFixed(1)}s`);
    } catch (e) { log(`${label} 探测失败: ${e.message}`); }
  }
});

$('btnMix').addEventListener('click', async () => {
  const req = buildRequest();
  if (!req.vocals_path || !req.accompaniment_path || !req.output_path) {
    log('请填写人声、伴奏与输出路径');
    return;
  }
  const btn = $('btnMix');
  btn.disabled = true;
  barFill.style.width = '0%';
  stage.textContent = '开始…';
  log('开始一键混音…');
  try {
    const r = await call('command.run', { commandId: 'mix', args: { request: req } });
    if (r && r.ok) {
      log(`✓ 完成：输出 ${r.output_lufs?.toFixed(1)} LUFS / ${r.output_true_peak_dbtp?.toFixed(2)} dBTP / ${r.elapsed_ms}ms`);
      log(`  NN: ${r.nn_applied ? r.nn_model_id : '未应用'} · 预设=${r.preset}`);
      stage.textContent = '完成';
    } else {
      log(`✗ 失败：${r?.error?.message || '未知错误'}`);
      stage.textContent = '失败';
    }
  } catch (e) {
    log(`✗ 调用失败: ${e.message}`);
    stage.textContent = '失败';
  } finally {
    btn.disabled = false;
  }
});

// 请求默认值以同步 UI。
call('command.run', { commandId: 'defaults', args: {} }).then((d) => {
  if (d && d.request) {
    const r = d.request;
    $('lufs').value = r.target_lufs;
    $('vgain').value = r.vocal_gain_db;
    $('duck').value = r.ducking_db;
  }
}).catch(() => {});

// 打招呼：告诉宿主"面板已加载"，宿主会回 welcome（带 locale / theme）。
parent.postMessage({ v: 2, kind: 'hello' }, '*');
