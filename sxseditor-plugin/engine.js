'use strict';
/**
 * OneMix JS 混音引擎 —— 与 Rust mixer-core 逐算法对齐的移植版。
 *
 * 运行环境说明（诚实声明）：
 * SXSEditor 的 `ctx.fs` 会把路径夹紧在「插件目录 / 插件数据目录」内，
 * 无法按绝对路径读取用户在面板里选择的音频。宿主源码 `sandbox.js` 也明确写着
 * 「插件仍可 require('node:fs') 绕过」。因此这里优先用 `ctx.fs`（受审计路径），
 * 失败时回退到 `node:fs` 读用户指定的绝对路径。若未来宿主收紧了该边界，
 * 插件需要改为「宿主 data plane + base64」模式。
 */
const nodepath = require('node:path');

let nodeFs = null;
try {
  nodeFs = require('node:fs');
} catch (_) {
  nodeFs = null;
}

const DB_TO_LIN = (db) => Math.pow(10, db / 20);

// ---------- 文件 IO（ctx.fs 优先，绝对路径回退 node:fs） ----------
async function readBytes(ctx, p) {
  try {
    return new Uint8Array(await ctx.fs.readBinary(p));
  } catch (e) {
    if (!nodeFs) throw e;
    try {
      return new Uint8Array(nodeFs.readFileSync(p));
    } catch (e2) {
      throw Object.assign(new Error(`无法读取文件 ${p}: ${e2.message}`), { code: 'FILE_READ' });
    }
  }
}

async function writeBytes(ctx, p, bytes) {
  try {
    return await ctx.fs.writeBinary(p, bytes);
  } catch (e) {
    if (!nodeFs) throw e;
    try {
      nodeFs.mkdirSync(nodepath.dirname(p), { recursive: true });
      nodeFs.writeFileSync(p, Buffer.from(bytes));
      return p;
    } catch (e2) {
      throw Object.assign(new Error(`无法写入文件 ${p}: ${e2.message}`), { code: 'FILE_WRITE' });
    }
  }
}

// ---------- WAV 编解码（8/16/24/32 PCM、32F、WAVE_FORMAT_EXTENSIBLE） ----------
function parseWav(bytes) {
  const dv = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  const str = (off, len) =>
    String.fromCharCode(...new Uint8Array(bytes.buffer, bytes.byteOffset + off, len));
  if (str(0, 4) !== 'RIFF' || str(8, 4) !== 'WAVE') throw new Error('不是 RIFF/WAVE 文件');
  let off = 12;
  let fmt = null;
  let dataOff = 0;
  let dataLen = 0;
  while (off + 8 <= bytes.length) {
    const id = str(off, 4);
    const size = dv.getUint32(off + 4, true);
    const body = off + 8;
    if (id === 'fmt ' && size >= 16 && body + 16 <= bytes.length) {
      let format = dv.getUint16(body, true);
      const channels = dv.getUint16(body + 2, true);
      const sampleRate = dv.getUint32(body + 4, true);
      const bits = dv.getUint16(body + 14, true);
      // WAVE_FORMAT_EXTENSIBLE：真实格式在 SubFormat GUID 的前两字节。
      if (format === 0xfffe && size >= 40 && body + 26 <= bytes.length) {
        format = dv.getUint16(body + 24, true);
      }
      fmt = { format, channels, sampleRate, bits };
    } else if (id === 'data') {
      dataOff = body;
      // 截断文件保护：不允许读取超出实际字节数。
      dataLen = Math.min(size, Math.max(0, bytes.length - body));
    }
    off = body + size + (size % 2);
  }
  if (!fmt || !dataLen) throw new Error('WAV 缺少 fmt/data 块');
  const { channels: ch, sampleRate, bits } = fmt;
  if (!(ch >= 1 && ch <= 8)) throw new Error(`不支持的声道数: ${ch}`);
  if (!(sampleRate >= 8000 && sampleRate <= 768000)) throw new Error(`不支持的采样率: ${sampleRate}`);
  const bytesPer = bits / 8;
  if (![1, 2, 3, 4].includes(bytesPer)) throw new Error(`不支持的位深: ${bits}`);
  const frames = Math.floor(dataLen / (ch * bytesPer));
  const samples = new Float32Array(frames * ch);
  const max = Math.pow(2, bits - 1);

  if (fmt.format === 3 && bits === 32) {
    for (let i = 0; i < samples.length; i++) samples[i] = dv.getFloat32(dataOff + i * 4, true);
  } else if (fmt.format === 3 && bits === 64) {
    for (let i = 0; i < samples.length; i++) samples[i] = dv.getFloat64(dataOff + i * 8, true);
  } else if (bits === 8) {
    // 8-bit WAV 是无符号偏移码。
    for (let i = 0; i < samples.length; i++) samples[i] = (dv.getUint8(dataOff + i) - 128) / 128;
  } else if (bits === 16) {
    for (let i = 0; i < samples.length; i++) samples[i] = dv.getInt16(dataOff + i * 2, true) / max;
  } else if (bits === 24) {
    for (let i = 0; i < samples.length; i++) {
      const o = dataOff + i * 3;
      let v = dv.getUint8(o) | (dv.getUint8(o + 1) << 8) | (dv.getUint8(o + 2) << 16);
      if (v & 0x800000) v -= 0x1000000;
      samples[i] = v / max;
    }
  } else if (bits === 32) {
    for (let i = 0; i < samples.length; i++) samples[i] = dv.getInt32(dataOff + i * 4, true) / max;
  } else {
    throw new Error(`不支持的 WAV 编码: format=${fmt.format}, bits=${bits}`);
  }
  return { samples, channels: ch, sampleRate };
}

function encodeWav(buf, bitDepth) {
  if (![16, 24, 32].includes(bitDepth)) throw new Error(`位深只支持 16/24/32，收到 ${bitDepth}`);
  const ch = buf.channels;
  const n = buf.samples.length;
  const bytesPer = bitDepth / 8;
  const dataLen = n * bytesPer;
  const pad = dataLen % 2; // RIFF 块必须偶数字节
  const out = new Uint8Array(44 + dataLen + pad);
  const dv = new DataView(out.buffer);
  const putStr = (o, s) => {
    for (let i = 0; i < s.length; i++) out[o + i] = s.charCodeAt(i);
  };
  putStr(0, 'RIFF');
  dv.setUint32(4, 36 + dataLen + pad, true);
  putStr(8, 'WAVE');
  putStr(12, 'fmt ');
  dv.setUint32(16, 16, true);
  const isFloat = bitDepth === 32;
  dv.setUint16(20, isFloat ? 3 : 1, true);
  dv.setUint16(22, ch, true);
  dv.setUint32(24, buf.sampleRate, true);
  dv.setUint32(28, buf.sampleRate * ch * bytesPer, true);
  dv.setUint16(32, ch * bytesPer, true);
  dv.setUint16(34, bitDepth, true);
  putStr(36, 'data');
  dv.setUint32(40, dataLen, true);
  if (isFloat) {
    for (let i = 0; i < n; i++) dv.setFloat32(44 + i * 4, buf.samples[i], true);
  } else {
    const max = Math.pow(2, bitDepth - 1) - 1;
    for (let i = 0; i < n; i++) {
      let v = Math.round(Math.max(-1, Math.min(1, buf.samples[i])) * max);
      if (bitDepth === 16) dv.setInt16(44 + i * 2, v, true);
      else {
        if (v < 0) v += 0x1000000;
        const o = 44 + i * 3;
        out[o] = v & 0xff;
        out[o + 1] = (v >> 8) & 0xff;
        out[o + 2] = (v >> 16) & 0xff;
      }
    }
  }
  return out;
}

// ---------- 音频变换 ----------
function resampleLinear(input, from, to) {
  if (from === to) return input.slice();
  const ratio = to / from;
  const outLen = Math.round(input.length * ratio);
  const out = new Float32Array(outLen);
  for (let i = 0; i < outLen; i++) {
    const p = i / ratio;
    const i0 = Math.floor(p);
    const f = p - i0;
    const s0 = input[i0] || 0;
    const s1 = input[i0 + 1] ?? s0;
    out[i] = s0 + (s1 - s0) * f;
  }
  return out;
}

function toPlanar(buf) {
  const ch = buf.channels;
  const frames = buf.samples.length / ch;
  const planes = [];
  for (let c = 0; c < ch; c++) planes.push(new Float32Array(frames));
  for (let f = 0; f < frames; f++) for (let c = 0; c < ch; c++) planes[c][f] = buf.samples[f * ch + c];
  return planes;
}

function fromPlanar(planes, sampleRate) {
  const ch = planes.length;
  const frames = planes[0] ? planes[0].length : 0;
  const samples = new Float32Array(frames * ch);
  for (let f = 0; f < frames; f++) for (let c = 0; c < ch; c++) samples[f * ch + c] = planes[c][f];
  return { samples, channels: ch, sampleRate };
}

function downmixMono(buf) {
  const planes = toPlanar(buf);
  const n = Math.min(...planes.map((p) => p.length));
  const m = new Float32Array(n);
  for (const p of planes) for (let i = 0; i < n; i++) m[i] += p[i] / planes.length;
  return m;
}

/** 归一化到立体声：单声道复制；多声道按简化 ITU 系数下混（与 Rust to_stereo 一致）。 */
function toStereo(buf) {
  if (buf.channels === 2) return buf;
  const planes = toPlanar(buf);
  let l;
  let r;
  if (buf.channels === 1) {
    l = planes[0].slice();
    r = planes[0].slice();
  } else {
    l = planes[0].slice();
    r = planes[1].slice();
    const n = Math.min(...planes.map((p) => p.length));
    for (let c = 2; c < planes.length; c++) {
      let wl = 0;
      let wr = 0;
      if (c === 2) {
        wl = 0.707;
        wr = 0.707;
      } else if (c === 3) {
        wl = 0;
        wr = 0;
      } else if (c === 4) {
        wl = 0.707;
      } else if (c === 5) {
        wr = 0.707;
      } else if (c % 2 === 0) {
        wl = 0.5;
      } else {
        wr = 0.5;
      }
      for (let i = 0; i < n; i++) {
        l[i] += planes[c][i] * wl;
        r[i] += planes[c][i] * wr;
      }
    }
  }
  return fromPlanar([l, r], buf.sampleRate);
}

// ---------- Biquad（RBJ cookbook, DF-I） ----------
function biquad(kind, sr, freq, q, gainDb, buf) {
  const f0 = Math.max(10, Math.min(freq, Math.max(10, sr * 0.49)));
  const w0 = (2 * Math.PI * f0) / sr;
  const cosw = Math.cos(w0);
  const sinw = Math.sin(w0);
  const qq = Math.max(1e-4, q);
  const alpha = sinw / (2 * qq);
  const A = Math.pow(10, gainDb / 40);
  let b0;
  let b1;
  let b2;
  let a0;
  let a1;
  let a2;
  switch (kind) {
    case 'lowpass':
      b0 = (1 - cosw) / 2;
      b1 = 1 - cosw;
      b2 = (1 - cosw) / 2;
      a0 = 1 + alpha;
      a1 = -2 * cosw;
      a2 = 1 - alpha;
      break;
    case 'highpass':
      b0 = (1 + cosw) / 2;
      b1 = -(1 + cosw);
      b2 = (1 + cosw) / 2;
      a0 = 1 + alpha;
      a1 = -2 * cosw;
      a2 = 1 - alpha;
      break;
    case 'peaking':
      b0 = 1 + alpha * A;
      b1 = -2 * cosw;
      b2 = 1 - alpha * A;
      a0 = 1 + alpha / A;
      a1 = -2 * cosw;
      a2 = 1 - alpha / A;
      break;
    case 'highshelf':
      // 与 Rust biquad 相同：交叉项 2*sqrt(A)*alpha。
      b0 = A * ((A + 1) + (A - 1) * cosw + 2 * Math.sqrt(A) * alpha);
      b1 = -2 * A * ((A - 1) + (A + 1) * cosw);
      b2 = A * ((A + 1) + (A - 1) * cosw - 2 * Math.sqrt(A) * alpha);
      a0 = (A + 1) - (A - 1) * cosw + 2 * Math.sqrt(A) * alpha;
      a1 = 2 * ((A - 1) - (A + 1) * cosw);
      a2 = (A + 1) - (A - 1) * cosw - 2 * Math.sqrt(A) * alpha;
      break;
    case 'lowshelf':
      b0 = A * ((A + 1) - (A - 1) * cosw + 2 * Math.sqrt(A) * alpha);
      b1 = 2 * A * ((A - 1) - (A + 1) * cosw);
      b2 = A * ((A + 1) - (A - 1) * cosw - 2 * Math.sqrt(A) * alpha);
      a0 = (A + 1) + (A - 1) * cosw + 2 * Math.sqrt(A) * alpha;
      a1 = -2 * ((A - 1) + (A + 1) * cosw);
      a2 = (A + 1) + (A - 1) * cosw - 2 * Math.sqrt(A) * alpha;
      break;
    default:
      b0 = 1;
      b1 = 0;
      b2 = 0;
      a0 = 1;
      a1 = 0;
      a2 = 0;
  }
  b0 /= a0;
  b1 /= a0;
  b2 /= a0;
  a1 /= a0;
  a2 /= a0;
  let x1 = 0;
  let x2 = 0;
  let y1 = 0;
  let y2 = 0;
  for (let i = 0; i < buf.length; i++) {
    const x = buf[i];
    const y = b0 * x + b1 * x1 + b2 * x2 - a1 * y1 - a2 * y2;
    x2 = x1;
    x1 = x;
    y2 = y1;
    y1 = y;
    buf[i] = y;
  }
}

// ---------- 压缩器 / ducking ----------
function compress(buf, sr, thresholdDb, ratio, kneeDb, attackMs, releaseMs, makeupDb) {
  const ac = Math.exp(-1 / (Math.max(0.01, attackMs) * 0.001 * sr));
  const rc = Math.exp(-1 / (Math.max(0.01, releaseMs) * 0.001 * sr));
  let env = 0;
  for (let i = 0; i < buf.length; i++) {
    const x = buf[i];
    const level = x * x;
    const coef = level > env ? ac : rc;
    env = coef * env + (1 - coef) * level;
    const envDb = 10 * Math.log10(env + 1e-12);
    const over = envDb - thresholdDb;
    let red = 0;
    if (over > kneeDb / 2) red = over * (1 - 1 / Math.max(1, ratio));
    else if (over > -kneeDb / 2) {
      const k = Math.max(0.01, kneeDb);
      red = (((over + k / 2) ** 2) / (2 * k)) * (1 - 1 / Math.max(1, ratio));
    }
    buf[i] *= DB_TO_LIN(-red + makeupDb);
  }
}

function duck(buf, sidechain, sr, reductionDb, attackMs, releaseMs) {
  const ac = Math.exp(-1 / (Math.max(0.01, attackMs) * 0.001 * sr));
  const rc = Math.exp(-1 / (Math.max(0.01, releaseMs) * 0.001 * sr));
  const floor = DB_TO_LIN(reductionDb);
  let env = 0;
  for (let i = 0; i < buf.length; i++) {
    const level = (sidechain[i] || 0) ** 2;
    const coef = level > env ? ac : rc;
    env = coef * env + (1 - coef) * level;
    const envDb = 10 * Math.log10(env + 1e-12);
    const norm = Math.max(0, Math.min(1, (envDb + 40) / 40));
    buf[i] *= Math.max(Math.min(floor, 1), DB_TO_LIN(reductionDb * norm));
  }
}

// ---------- K-weighting + LUFS + 真峰 ----------
function kWeightEnergy(plane, sr) {
  const x = plane.slice();
  biquad('highshelf', sr, 1681.974450955533, 0.7071752369554196, 3.999843853973347, x);
  biquad('highpass', sr, 38.13547087602444, 0.5003270373238773, 0.0, x);
  const e = new Float64Array(x.length);
  for (let i = 0; i < x.length; i++) e[i] = x[i] * x[i];
  return e;
}

const energyToLufs = (z) => -0.691 + 10 * Math.log10(Math.max(z, 1e-12));

function integratedLufs(planes, sr) {
  if (!planes.length || !planes[0].length) return -70;
  const energies = planes.map((p) => kWeightEnergy(p, sr));
  const frames = energies[0].length;
  const block = Math.floor(0.4 * sr);
  const hop = Math.max(1, Math.floor(block / 4));
  const blockEnergies = [];
  if (!block || frames < block) {
    // 短于 400ms：整段作为单块测量，而不是直接报 -70。
    let sum = 0;
    for (const e of energies) {
      let s = 0;
      for (let i = 0; i < frames; i++) s += e[i];
      sum += s / frames;
    }
    blockEnergies.push(sum);
  } else {
    for (let pos = 0; pos + block <= frames; pos += hop) {
      let sum = 0;
      for (const e of energies) {
        let s = 0;
        for (let i = pos; i < pos + block; i++) s += e[i];
        sum += s / block;
      }
      blockEnergies.push(sum);
    }
  }
  if (!blockEnergies.length) return -70;
  // 能量域门限（与 Rust / BS.1770 一致）。
  const gatedAbs = blockEnergies.filter((z) => energyToLufs(z) > -70);
  if (!gatedAbs.length) return -70;
  const meanAbs = gatedAbs.reduce((a, b) => a + b, 0) / gatedAbs.length;
  const relGate = energyToLufs(meanAbs) - 10;
  const gatedRel = gatedAbs.filter((z) => energyToLufs(z) > relGate);
  if (!gatedRel.length) return -70;
  return energyToLufs(gatedRel.reduce((a, b) => a + b, 0) / gatedRel.length);
}

// 4 倍多相窗函数 sinc 插值（与 Rust 相同）。
const TP_PHASES = 4;
const TP_HALF = 4;
function sinc(x) {
  if (Math.abs(x) < 1e-9) return 1;
  const px = Math.PI * x;
  return Math.sin(px) / px;
}
const TP_TAPS = (() => {
  const all = [];
  for (let p = 1; p < TP_PHASES; p++) {
    const frac = p / TP_PHASES;
    const taps = [];
    let sum = 0;
    for (let k = -TP_HALF + 1; k <= TP_HALF; k++) {
      const t = frac - k;
      const w = 0.5 * (1 + Math.cos((Math.PI * t) / (TP_HALF + 0.5)));
      const v = sinc(t) * w;
      taps.push(v);
      sum += v;
    }
    all.push(taps.map((v) => v / (sum || 1)));
  }
  return all;
})();

function truePeakEnvelope(buf) {
  const n = buf.length;
  const env = new Float32Array(n);
  if (!n) return env;
  for (let i = 0; i < n; i++) {
    let peak = Math.abs(buf[i]);
    for (const taps of TP_TAPS) {
      let acc = 0;
      for (let j = 0; j < taps.length; j++) {
        const k = j - TP_HALF + 1;
        const idx = Math.max(0, Math.min(n - 1, i + k));
        acc += buf[idx] * taps[j];
      }
      peak = Math.max(peak, Math.abs(acc));
    }
    env[i] = peak;
  }
  return env;
}

function truePeakDbtp(planes) {
  let peak = 0;
  for (const plane of planes) {
    const env = truePeakEnvelope(plane);
    for (let i = 0; i < env.length; i++) peak = Math.max(peak, env[i]);
  }
  return peak > 0 ? 20 * Math.log10(peak) : -70;
}

/** 真峰限幅：前瞻窗口 + 瞬时压制 + 平滑释放 + 终检微缩（与 Rust 同算法）。 */
function limiter(plane, sr, ceilingDbtp, releaseMs) {
  const ceiling = DB_TO_LIN(ceilingDbtp);
  const n = plane.length;
  if (!n) return;
  const env = truePeakEnvelope(plane);
  const la = Math.max(1, Math.min(n, Math.floor(0.005 * sr)));
  const desired = new Float32Array(n).fill(1);
  const dq = [];
  let head = 0;
  for (let i = 0; i < n; i++) {
    const j = i + la - 1;
    if (j < n) {
      while (dq.length > head && env[dq[dq.length - 1]] <= env[j]) dq.pop();
      dq.push(j);
    }
    while (dq.length > head && dq[head] < i) head += 1;
    if (dq.length > head) {
      const peak = env[dq[head]];
      if (peak > ceiling) desired[i] = ceiling / peak;
    }
    if (head > 4096) {
      dq.splice(0, head);
      head = 0;
    }
  }
  const rc = Math.exp(-1 / (Math.max(1, releaseMs) * 0.001 * sr));
  let gain = Math.min(1, desired[0]);
  for (let i = 0; i < n; i++) {
    const target = desired[i];
    if (target < gain) gain = target;
    else gain = rc * gain + (1 - rc) * target;
    const v = plane[i] * gain;
    plane[i] = Math.max(-ceiling, Math.min(ceiling, v));
  }
  // 硬钳位会产生新的样本间峰：按实测真峰整体微缩，严格保证不越界。
  const residual = truePeakEnvelope(plane).reduce((m, v) => Math.max(m, v), 0);
  if (residual > ceiling && residual > 0) {
    const trim = ceiling / residual;
    for (let i = 0; i < n; i++) plane[i] *= trim;
  }
}

function stereoWidth(samples, width) {
  const w = Math.max(0, Math.min(2, width));
  for (let i = 0; i < samples.length; i += 2) {
    const l = samples[i];
    const r = samples[i + 1];
    const mid = (l + r) * 0.5;
    const side = (l - r) * 0.5 * w;
    samples[i] = mid + side;
    samples[i + 1] = mid - side;
  }
}

// ---------- 预设（与 Rust 一致） ----------
const PRESETS = {
  streaming: { target_lufs: -14, true_peak: -1, ducking: -2.5, bus_glue: 0.35, stereo_width: 1.1 },
  broadcast: { target_lufs: -16, true_peak: -1, ducking: -2.0, bus_glue: 0.3, stereo_width: 1.05 },
  quiet: { target_lufs: -20, true_peak: -1.5, ducking: -1.5, bus_glue: 0.25, stereo_width: 1.0 },
  loud: { target_lufs: -12, true_peak: -1, ducking: -3.5, bus_glue: 0.45, stereo_width: 1.15 },
};

// 输入模式与乐器类型（与 Rust api.rs 保持一致）。
const MODES = ['stems', 'separate'];
const INSTRUMENTS = ['auto', 'vocals', 'drums', 'bass', 'instrumental', 'other'];

/** 乐器类型解析（命令行/JSON 显式指定）。 */
function instrumentFromName(s) {
  const k = String(s || '').toLowerCase();
  if (['', 'auto'].includes(k)) return 'auto';
  if (['vocal', 'vocals', 'voice', '人声', '主唱'].includes(k)) return 'vocals';
  if (['drum', 'drums', '鼓', '鼓组'].includes(k)) return 'drums';
  if (['bass', '贝斯', '低音'].includes(k)) return 'bass';
  if (['inst', 'instrumental', 'accompaniment', '伴奏', '器乐'].includes(k)) return 'instrumental';
  if (['other', '其他', '乐器'].includes(k)) return 'other';
  return null;
}

/** 自动推断：只看文件名（与 Rust 一致，避免目录名误判）。 */
function inferInstrument(path) {
  const p = String(path || '').replace(/\\/g, '/').toLowerCase();
  const name = p.split('/').pop() || p;
  const has = (kws) => kws.some((k) => name.includes(k));
  if (has(['vocal', 'vocals', 'voice', 'vox', '人声', '主唱', 'sing'])) return 'vocals';
  if (has(['drum', 'drums', 'kick', 'snare', 'hihat', 'perc', '鼓'])) return 'drums';
  if (has(['bass', '贝斯', '低音'])) return 'bass';
  if (has(['inst', 'instrumental', 'accomp', 'backing', 'karaoke', '伴奏', '器乐', 'bgm'])) return 'instrumental';
  return 'other';
}

/** 单一乐器的专属处理链（就地处理每个声道），与 Rust process_instrument 对齐。 */
function processInstrument(planes, inst, sr, r) {
  switch (inst) {
    case 'vocals': {
      const g = DB_TO_LIN(r.vocal_gain_db);
      for (const plane of planes) {
        if (r.vocal_highpass_hz > 0) biquad('highpass', sr, r.vocal_highpass_hz, 0.707, 0, plane);
        biquad('peaking', sr, 250, 0.9, -2, plane);
        biquad('peaking', sr, 3000, 1.0, r.vocal_presence_db, plane);
        biquad('highshelf', sr, 10000, 0.707, 1.5, plane);
        compress(plane, sr, -18, 3, 6, 8, 120, 1.5);
        for (let i = 0; i < plane.length; i++) plane[i] *= g;
      }
      break;
    }
    case 'drums': {
      for (const plane of planes) {
        biquad('highpass', sr, 28, 0.707, 0, plane);
        biquad('peaking', sr, 120, 1.0, 1.5, plane);
        biquad('highshelf', sr, 9000, 0.707, 1.0, plane);
        compress(plane, sr, -14, 2.5, 4, 3, 90, 1.0);
      }
      break;
    }
    case 'bass': {
      // 单声道低频重心：提取两声道低频均值，再与各声道高频相加。
      let monoLow = null;
      if (planes.length >= 2) {
        const n = Math.min(planes[0].length, planes[1].length);
        monoLow = new Float32Array(n);
        for (let i = 0; i < n; i++) monoLow[i] = (planes[0][i] + planes[1][i]) * 0.5;
        biquad('lowpass', sr, 160, 0.707, 0, monoLow);
      }
      for (const plane of planes) {
        biquad('highpass', sr, 30, 0.707, 0, plane);
        biquad('peaking', sr, 220, 0.9, -1.0, plane);
        compress(plane, sr, -20, 4, 3, 10, 130, 1.5);
        if (monoLow) {
          const hi = new Float32Array(plane);
          biquad('highpass', sr, 160, 0.707, 0, hi);
          for (let i = 0; i < Math.min(plane.length, monoLow.length); i++) plane[i] = monoLow[i] + hi[i];
        }
      }
      break;
    }
    case 'instrumental': {
      for (const plane of planes) {
        if (r.accompaniment_carve_db !== 0) biquad('peaking', sr, 2500, 0.7, r.accompaniment_carve_db, plane);
        compress(plane, sr, -20, 2, 6, 15, 180, 0.5);
      }
      break;
    }
    case 'other':
    default: {
      for (const plane of planes) {
        biquad('highpass', sr, 60, 0.707, 0, plane);
        biquad('peaking', sr, 1200, 0.8, 0.8, plane);
        biquad('highshelf', sr, 8000, 0.707, 1.0, plane);
        compress(plane, sr, -20, 2, 8, 20, 200, 0.5);
      }
      break;
    }
  }
}

const DEFAULT_REQUEST = {
  mode: 'stems',
  stems: [],
  source_path: '',
  separation_model: null,
  vocals_path: '',
  accompaniment_path: '',
  output_path: '',
  target_lufs: -14,
  true_peak_ceiling: -1,
  output_sample_rate: null,
  output_bit_depth: 24,
  vocal_gain_db: 0,
  ducking_db: -2.5,
  vocal_presence_db: 2,
  vocal_highpass_hz: 75,
  accompaniment_carve_db: -2,
  bus_glue: 0.35,
  stereo_width: 1.1,
  nn_enhance: false,
  nn_model: null,
  nn_mix: 0.5,
  models_dir: null,
  preset: 'streaming',
  verbose: true,
};

const MODELS = [
  { id: 'onemix-dyn-v1', name: 'OneMix 轻量动态增强器', task: 'enhance', architecture: 'Conv1d-ReLU-Conv1d-Tanh', bundled: true, local_file: 'models/onemix_dyn_enhance_v1.onnx', input_name: 'x', output_name: 'y', note: '随插件分发，375B，逐帧动态增益曲线。' },
  { id: 'bs-roformer-vocal', name: 'BS-RoFormer 人声分离', task: 'separation', architecture: 'Band-Split RoPE Transformer', bundled: false, source_url: 'https://hf-mirror.com', note: 'MUSDB18-HQ SOTA，人声 SDR ≈10.9 dB。' },
  { id: 'htdemucs-ft', name: 'HT-Demucs FT 4-stem', task: 'separation', architecture: 'Hybrid Transformer Demucs', bundled: false, source_url: 'https://hf-mirror.com', note: '生产默认，4 源 ≈9.0 dB SDR。' },
  { id: 'mel-band-roformer', name: 'Mel-Band RoFormer', task: 'dereverb', architecture: 'Mel-Band Split RoPE Transformer', bundled: false, source_url: 'https://hf-mirror.com', note: '梅尔频带变体，去混响/人声净化。' },
];

// ---------- 流水线 ----------
async function runMix(ctx, req) {
  const started = Date.now();
  const stages = [];
  // 宿主约定 progress ∈ [0,1]；stages 保留文本记录。
  const progress = (pct, stage) => {
    stages.push(stage);
    try {
      ctx.ui.progress(Math.max(0, Math.min(1, pct / 100)), stage);
    } catch (_) {
      /* 面板未连接时忽略 */
    }
  };
  const defaults = { ...DEFAULT_REQUEST };
  const r = { ...defaults, ...(req || {}) };

  // 参数校验（与 Rust 入口校验一致）。
  const mode = String(r.mode || 'stems').toLowerCase();
  if (!MODES.includes(mode)) {
    throw Object.assign(new Error(`未知模式: ${r.mode}（可选: stems / separate）`), { code: 'INVALID_PARAM' });
  }
  if (r.target_lufs < -40 || r.target_lufs > 0) {
    throw Object.assign(new Error(`target_lufs 必须在 -40..=0，收到 ${r.target_lufs}`), { code: 'INVALID_PARAM' });
  }
  if (r.true_peak_ceiling < -12 || r.true_peak_ceiling > 0) {
    throw Object.assign(new Error(`true_peak_ceiling 必须在 -12..=0 dBTP`), { code: 'INVALID_PARAM' });
  }
  if (![16, 24, 32].includes(Number(r.output_bit_depth))) {
    throw Object.assign(new Error(`output_bit_depth 只支持 16/24/32`), { code: 'INVALID_PARAM' });
  }
  if (r.output_sample_rate != null && (r.output_sample_rate < 8000 || r.output_sample_rate > 192000)) {
    throw Object.assign(new Error(`output_sample_rate 必须在 8000..=192000`), { code: 'INVALID_PARAM' });
  }
  if (!r.output_path) {
    throw Object.assign(new Error('输出路径不能为空'), { code: 'INVALID_PARAM' });
  }

  // 应用预设（与 Rust 相同：仅覆盖仍是默认值的字段，五项全应用）。
  const preset = PRESETS[String(r.preset || 'streaming').toLowerCase()];
  if (!preset) {
    throw Object.assign(new Error(`未知预设: ${r.preset}`), { code: 'INVALID_PARAM' });
  }
  if (r.target_lufs === defaults.target_lufs) r.target_lufs = preset.target_lufs;
  if (r.true_peak_ceiling === defaults.true_peak_ceiling) r.true_peak_ceiling = preset.true_peak;
  if (r.ducking_db === defaults.ducking_db) r.ducking_db = preset.ducking;
  if (r.bus_glue === defaults.bus_glue) r.bus_glue = preset.bus_glue;
  if (r.stereo_width === defaults.stereo_width) r.stereo_width = preset.stereo_width;

  // 1. 解析输入分轨。
  const inputs = [];
  let separationApplied = false;
  const separationModelId = null;
  if (mode === 'stems') {
    if (Array.isArray(r.stems) && r.stems.length > 0) {
      for (const st of r.stems) {
        if (!st || !st.path) throw Object.assign(new Error('分轨缺少 path'), { code: 'INVALID_PARAM' });
        inputs.push({
          path: st.path,
          instrument: st.instrument || 'auto',
          gain_db: st.gain_db == null ? 0 : Number(st.gain_db),
        });
      }
    } else {
      if (!r.vocals_path && !r.accompaniment_path) {
        throw Object.assign(new Error('分轨列表与人声/伴奏路径均为空'), { code: 'INVALID_PARAM' });
      }
      if (r.vocals_path) inputs.push({ path: r.vocals_path, instrument: 'vocals', gain_db: 0 });
      if (r.accompaniment_path) inputs.push({ path: r.accompaniment_path, instrument: 'instrumental', gain_db: 0 });
    }
  } else {
    const src = r.source_path || r.vocals_path;
    if (!src) throw Object.assign(new Error('分离模式需要 source_path（成品混音）'), { code: 'INVALID_PARAM' });
    // 分离权重未随插件分发：器乐链 + 总线母带，明确标注未分离。
    inputs.push({ path: src, instrument: 'instrumental', gain_db: 0 });
    progress(8, '分离模型检查');
    stages.push('分离模型不可用(降级为器乐链 + 总线母带)');
  }

  progress(3, '校验参数与预设');

  // 2. 读取全部分轨。
  let bufs = [];
  for (const inp of inputs) {
    bufs.push(parseWav(await readBytes(ctx, inp.path)));
  }
  progress(8, '读取音频');

  // 3. 统一采样率/声道/长度。
  const sr = r.output_sample_rate || Math.max(...bufs.map((b) => b.sampleRate));
  bufs = bufs.map((b) =>
    b.sampleRate !== sr ? fromPlanar(toPlanar(b).map((p) => resampleLinear(p, b.sampleRate, sr)), sr) : b
  );
  bufs = bufs.map((b) => toStereo(b));
  const maxFrames = Math.max(...bufs.map((b) => b.samples.length / b.channels));
  bufs = bufs.map((b) => {
    const frames = b.samples.length / b.channels;
    if (frames === maxFrames) return b;
    const planes = toPlanar(b).map((p) => {
      const q = new Float32Array(maxFrames);
      q.set(p.subarray(0, Math.min(p.length, maxFrames)));
      return q;
    });
    return fromPlanar(planes, sr);
  });
  progress(12, '统一采样率/声道/长度');

  // 乐器解析结果。
  const stemsUsed = inputs.map((inp) => {
    const inferred = inp.instrument === 'auto';
    const parsed = instrumentFromName(inp.instrument);
    if (!inferred && !parsed) {
      throw Object.assign(new Error(`未知乐器类型: ${inp.instrument}`), { code: 'INVALID_PARAM' });
    }
    return {
      path: inp.path,
      instrument: inferred ? inferInstrument(inp.path) : parsed,
      inferred,
      gain_db: inp.gain_db,
    };
  });

  const inputVocalsLufs = (() => {
    const idx = stemsUsed.findIndex((s) => s.instrument === 'vocals');
    return idx < 0 ? -100 : integratedLufs(toPlanar(bufs[idx]), sr);
  })();
  const inputAccompanimentLufs = (() => {
    const idx = stemsUsed.findIndex((s) => s.instrument !== 'vocals');
    return idx < 0 ? -100 : integratedLufs(toPlanar(bufs[idx]), sr);
  })();

  // 4. NN 增强（仅人声轨，经 ctx.ort 推理；按宿主真实 API 调用）。
  let nnApplied = false;
  let nnModelId = null;
  if (r.nn_enhance && ctx.ort && typeof ctx.ort.load === 'function') {
    let sess = null;
    try {
      sess = await ctx.ort.load('models/onemix_dyn_enhance_v1.onnx', { ep: 'cpu' });
      for (let k = 0; k < bufs.length; k++) {
        if (stemsUsed[k].instrument !== 'vocals') continue;
        const mono = downmixMono(bufs[k]);
        const frames = 128;
        const win = Math.max(1, Math.floor(mono.length / frames));
        const env = new Float32Array(frames);
        let peak = 0;
        for (let i = 0; i < frames; i++) {
          const start = Math.min(i * win, mono.length);
          const end = Math.min((i + 1) * win, mono.length);
          let s = 0;
          for (let j = start; j < end; j++) s += mono[j] * mono[j];
          env[i] = Math.sqrt(s / Math.max(1, end - start));
          peak = Math.max(peak, env[i]);
        }
        if (peak > 0) for (let i = 0; i < frames; i++) env[i] = Math.max(0, Math.min(1, env[i] / peak));
        const out = await ctx.ort.run(sess.handle, {
          x: { type: 'float32', dims: [1, 1, frames], data: env },
        });
        const raw = out && out.y && out.y.data ? out.y.data : null;
        if (!raw) throw new Error('模型缺少输出 y');
        const frameGain = Array.from(raw).map((g) => Math.max(0.5, Math.min(1.5, 1 + r.nn_mix * g)));
        const vp = toPlanar(bufs[k]);
        for (const plane of vp) {
          for (let i = 0; i < plane.length; i++) {
            const pos = (i / Math.max(1, plane.length)) * (frameGain.length - 1);
            const i0 = Math.floor(pos);
            const f = pos - i0;
            plane[i] *= frameGain[i0] + (frameGain[Math.min(i0 + 1, frameGain.length - 1)] - frameGain[i0]) * f;
          }
        }
        bufs[k] = fromPlanar(vp, sr);
        nnApplied = true;
        nnModelId = 'onemix-dyn-v1';
      }
    } catch (e) {
      try {
        ctx.log.warn(`NN 增强降级: ${e && e.message}`);
      } catch (_) {}
    } finally {
      if (sess && ctx.ort && typeof ctx.ort.unload === 'function') {
        try {
          await ctx.ort.unload(sess.handle);
        } catch (_) {}
      }
    }
  }
  progress(25, nnApplied ? 'NN 音质增强(完成)' : 'NN 增强(跳过)');

  // 5. 逐轨专属 DSP 链。
  const processed = [];
  for (let k = 0; k < bufs.length; k++) {
    const planes = toPlanar(bufs[k]);
    processInstrument(planes, stemsUsed[k].instrument, sr, r);
    const g = DB_TO_LIN(stemsUsed[k].gain_db);
    if (g !== 1) for (const plane of planes) for (let i = 0; i < plane.length; i++) plane[i] *= g;
    processed.push(fromPlanar(planes, sr));
    progress(30 + Math.floor((25 * (k + 1)) / bufs.length), '处理分轨');
  }
  progress(55, '逐轨专属 DSP 链');

  // 6. 侧链 ducking（以人声合成单声道为侧链，对非人声轨）。
  progress(60, '侧链 ducking');
  const vocalIdx = stemsUsed.map((s, i) => (s.instrument === 'vocals' ? i : -1)).filter((i) => i >= 0);
  if (vocalIdx.length > 0) {
    const n = Math.min(...processed.map((b) => b.samples.length / b.channels));
    const vocalMono = new Float32Array(n);
    for (const k of vocalIdx) {
      const vm = downmixMono(processed[k]);
      for (let i = 0; i < Math.min(n, vm.length); i++) vocalMono[i] += vm[i];
    }
    for (let k = 0; k < processed.length; k++) {
      if (stemsUsed[k].instrument === 'vocals') continue;
      const planes = toPlanar(processed[k]);
      for (const plane of planes) duck(plane, vocalMono, sr, r.ducking_db, 20, 250);
      processed[k] = fromPlanar(planes, sr);
    }
  }
  progress(65, '侧链 ducking(人声主控)');

  // 7. 总线混合。
  const allPlanes = processed.map((b) => toPlanar(b));
  const frames = Math.min(...allPlanes.map((p) => p[0].length));
  let mixPlanes = [new Float32Array(frames), new Float32Array(frames)];
  for (const planes of allPlanes) {
    for (let c = 0; c < 2; c++) for (let i = 0; i < frames; i++) mixPlanes[c][i] += planes[c][i];
  }
  progress(75, '总线混合');

  // 8. Glue 压缩（每声道独立包络，与 Rust 一致）。
  if (r.bus_glue > 0) {
    for (const plane of mixPlanes) compress(plane, sr, -16, 1 + r.bus_glue * 2, 8, 20, 200, 0);
  }
  // 9. 立体声宽度。
  if (r.stereo_width !== 1) {
    const sbuf = fromPlanar(mixPlanes, sr);
    stereoWidth(sbuf.samples, r.stereo_width);
    mixPlanes = toPlanar(sbuf);
  }
  progress(82, '总线压缩与立体声');

  // 10. 响度标准化（闭环）+ 真峰限幅。
  const premasterLufs = integratedLufs(mixPlanes, sr);
  let gainDb = Math.max(-24, Math.min(24, r.target_lufs - premasterLufs));
  let gainLin = DB_TO_LIN(gainDb);
  for (const plane of mixPlanes) for (let i = 0; i < plane.length; i++) plane[i] *= gainLin;
  for (const plane of mixPlanes) limiter(plane, sr, r.true_peak_ceiling, 60);
  let outputLufs = integratedLufs(mixPlanes, sr);
  for (let it = 0; it < 2; it++) {
    const err = r.target_lufs - outputLufs;
    if (Math.abs(err) <= 0.3) break;
    const extra = Math.max(-6, Math.min(6, err));
    gainDb += extra;
    gainLin = DB_TO_LIN(extra);
    for (const plane of mixPlanes) for (let i = 0; i < plane.length; i++) plane[i] *= gainLin;
    for (const plane of mixPlanes) limiter(plane, sr, r.true_peak_ceiling, 60);
    outputLufs = integratedLufs(mixPlanes, sr);
  }
  progress(96, '响度标准化与限幅');

  // 11. 导出。
  const outBuf = fromPlanar(mixPlanes, sr);
  const wavBytes = encodeWav(outBuf, Number(r.output_bit_depth));
  await writeBytes(ctx, r.output_path, wavBytes);
  progress(100, '完成');

  const finalPlanes = toPlanar(parseWav(await readBytes(ctx, r.output_path)));
  const finalLufs = integratedLufs(finalPlanes, sr);
  const outputTruePeak = truePeakDbtp(finalPlanes);

  return {
    ok: true,
    output_path: r.output_path,
    sample_rate: sr,
    channels: 2,
    duration_sec: frames / sr,
    input_vocals_lufs: inputVocalsLufs,
    input_accompaniment_lufs: inputAccompanimentLufs,
    premaster_lufs: premasterLufs,
    output_lufs: finalLufs,
    output_true_peak_dbtp: outputTruePeak,
    normalization_gain_db: gainDb,
    nn_applied: nnApplied,
    nn_model_id: nnModelId,
    elapsed_ms: Date.now() - started,
    preset: r.preset,
    stages,
    mode,
    stems_used: stemsUsed,
    separation_applied: separationApplied,
    separation_model_id: separationModelId,
    loudness_target_met: Math.abs(finalLufs - r.target_lufs) <= 0.5,
  };
}

module.exports = {
  runMix,
  parseWav,
  encodeWav,
  integratedLufs,
  truePeakDbtp,
  PRESETS,
  DEFAULT_REQUEST,
  MODELS,
  MODES,
  INSTRUMENTS,
  inferInstrument,
  instrumentFromName,
  readBytes,
  writeBytes,
  DB_TO_LIN,
};
