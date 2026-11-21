'use strict';
/**
 * OneMix 混音插件入口（SXSEditor v2，运行于 utilityProcess）。
 *
 * 与独立软件（Rust / mixer-cli）共享完全一致的：
 *   · MixRequest / MixResult JSON schema
 *   · 参数默认值与预设
 *   · 混音 DSP 算法（engine.js 是 mixer-core 的逐算法移植）
 *   · 自动化命令（mix / mixFiles / probe / models / presets）
 */
const engine = require('./engine');

let disposed = false;

function ok(ctx, message) {
  try { if (ctx && ctx.ui) ctx.ui.notify(message, 'info'); } catch (_) {}
}
function fail(ctx, err) {
  const code = (err && err.code) || 'INTERNAL';
  const message = (err && err.message) || String(err);
  try { if (ctx && ctx.ui) ctx.ui.notify(`混音失败：${message}`, 'error'); } catch (_) {}
  return { ok: false, error: { code, message } };
}

module.exports = {
  id: 'onemix-mixer',
  name: 'OneMix 一键混音',
  version: '0.1.0',
  apiVersion: 2,

  initialize(ctx) {
    ctx.log.info('OneMix 混音插件已就绪（与 Rust 独立软件能力一致）');
    try { ctx.ui.status('status', 'OneMix ready'); } catch (_) {}
    ctx.onDispose(() => { disposed = true; ctx.log.info('OneMix 正在退出'); });
    ctx.onSettingsChanged((_c, values) => {
      ctx.log.info(`OneMix 设置已更新：${JSON.stringify(values)}`);
    });
  },

  commands: {
    /** 完整 MixRequest → MixResult（与 CLI `mix --json` 完全一致）。 */
    async mix(ctx, args) {
      if (disposed) throw Object.assign(new Error('插件正在退出'), { code: 'PLUGIN_UNLOADED' });
      try {
        const req = (args && args.request) ? args.request : args;
        const result = await engine.runMix(ctx, req);
        try { ctx.ui.status('status', `LUFS ${result.output_lufs.toFixed(1)}`); } catch (_) {}
        ok(ctx, `混音完成：${result.output_path}（${result.output_lufs.toFixed(1)} LUFS）`);
        return result;
      } catch (e) { return fail(ctx, e); }
    },

    /** 快捷混音：两个文件 + 可选预设。 */
    async mixFiles(ctx, args) {
      if (!args || !args.vocalsPath || !args.accompanimentPath || !args.outputPath) {
        return fail(ctx, Object.assign(new Error('需要 vocalsPath / accompanimentPath / outputPath'), { code: 'INVALID_PARAM' }));
      }
      const req = {
        ...engine.DEFAULT_REQUEST,
        vocals_path: args.vocalsPath,
        accompaniment_path: args.accompanimentPath,
        output_path: args.outputPath,
        preset: args.preset || 'streaming',
        ...(args.options || {}),
      };
      // 应用插件设置中的默认值（若用户未显式覆盖）。
      try {
        const s = ctx.settings.all() || {};
        if (req.target_lufs === engine.DEFAULT_REQUEST.target_lufs && s.targetLufs != null) req.target_lufs = Number(s.targetLufs);
        if (req.true_peak_ceiling === engine.DEFAULT_REQUEST.true_peak_ceiling && s.truePeak != null) req.true_peak_ceiling = Number(s.truePeak);
        if (req.ducking_db === engine.DEFAULT_REQUEST.ducking_db && s.ducking != null) req.ducking_db = Number(s.ducking);
        if (req.vocal_gain_db === engine.DEFAULT_REQUEST.vocal_gain_db && s.vocalGain != null) req.vocal_gain_db = Number(s.vocalGain);
        if (req.nn_enhance === false && s.nnEnhance != null) {
          req.nn_enhance = s.nnEnhance === true || s.nnEnhance === 'true' || s.nnEnhance === 1;
        }
      } catch (_) {}
      // 注意：宿主调用命令时是 `fn(ctx, args)`（裸函数），严格模式下 this 为 undefined，
      // 因此必须显式通过 module.exports 取同级命令，不能用 this.mix。
      return module.exports.commands.mix(ctx, { request: req });
    },

    /** 探测 WAV 信息。 */
    async probe(ctx, args) {
      try {
        const path = (args && (args.path || args.vocalsPath)) || '';
        if (!path) throw Object.assign(new Error('需要 path'), { code: 'INVALID_PARAM' });
        const bytes = await engine.readBytes(ctx, path);
        const buf = engine.parseWav(new Uint8Array(bytes));
        return {
          path,
          sample_rate: buf.sampleRate,
          channels: buf.channels,
          frames: buf.samples.length / buf.channels,
          duration_sec: (buf.samples.length / buf.channels) / buf.sampleRate,
        };
      } catch (e) { return fail(ctx, e); }
    },

    /** 列出 NN 模型（与 CLI `models` 一致）。 */
    async models(ctx) {
      return { ok: true, models: engine.MODELS };
    },

    /** 列出预设（与 CLI `presets` 一致）。 */
    async presets(ctx) {
      return { ok: true, presets: Object.keys(engine.PRESETS) };
    },

    /** 列出输入模式（与 CLI `modes` 一致）。 */
    async modes(ctx) {
      return {
        ok: true,
        modes: [
          { id: 'stems', desc: '分轨直入：输入已分离的乐器分轨，跳过所有分离模型（默认）' },
          { id: 'separate', desc: '分离再混：输入成品混音，先用 SOTA 分离模型拆分再混' },
        ],
      };
    },

    /** 列出乐器类型（与 CLI `instruments` 一致）。 */
    async instruments(ctx) {
      return { ok: true, instruments: engine.INSTRUMENTS };
    },

    /** 默认请求 + 预设 + 模型 + 模式（供面板同步 UI）。 */
    async defaults(ctx) {
      return {
        ok: true,
        request: engine.DEFAULT_REQUEST,
        presets: engine.PRESETS,
        models: engine.MODELS,
        modes: engine.MODES,
        instruments: engine.INSTRUMENTS,
      };
    },
  },

  panels: {
    async rpc(ctx, method, params) {
      switch (method) {
        // 宿主的 panel RPC 会把方法名原样透传（pluginRuntimeEntry.js），
        // 面板用 `command.run` scope 时这里必须自己分派到命令表。
        case 'command.run': {
          const id = params && params.commandId;
          const allowed = ['mix', 'mixFiles', 'probe', 'models', 'modes', 'instruments', 'presets', 'defaults'];
          if (!allowed.includes(id)) {
            throw Object.assign(new Error(`未知命令 ${id}`), { code: 'NOT_FOUND' });
          }
          return module.exports.commands[id](ctx, (params && params.args) || {});
        }
        // 兼容早期的面板直连方法名。
        case 'onemix.mix':
          return module.exports.commands.mix(ctx, params && params.request ? { request: params.request } : params);
        case 'onemix.probe':
          return module.exports.commands.probe(ctx, params);
        case 'onemix.defaults':
          return module.exports.commands.defaults(ctx);
        default:
          throw Object.assign(new Error(`未知面板方法 ${method}`), { code: 'UNSUPPORTED' });
      }
    },
  },
};
