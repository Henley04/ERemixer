# SXSEditor 混音插件说明

## 产物

| 文件 | 说明 |
|---|---|
| `dist-plugins/onemix-mixer-0.1.0.sxsp` | **未签名主包**，宿主默认放行（状态 `unsigned`），推荐直接安装 |
| `dist-plugins/onemix-mixer-0.1.0-signed.sxsp` | 自签名包（Ed25519，publisher=OneMix），密钥不在宿主信任库，需先核对指纹并「信任并安装」 |

> 注意：SXSEditor 的安装策略是「未签名可装、未知密钥拒装」（`pluginSignature.js` 的 `PASS_STATUS` 含 `unsigned`，不含 `unknown-key`）。因此把自签名包当默认分发反而会安装失败。

## 安装

1. 打开 SXSEditor → **插件 → 插件管理器… → 安装插件…**
2. 选择 `onemix-mixer-0.1.0.sxsp`
3. 未签名包会显示「未签名」横幅，确认后安装
4. 无需重启。菜单「插件 → onemix」、工具栏按钮与「OneMix 面板」随之出现

如使用签名包：首次会出现 `unknown-key` 横幅，核对指纹 `ed25519-807c927ff46292c6` 后点「信任并安装」。

## 能力

插件声明 `apiVersion: 2`，贡献 **8 个命令**，全部同时在 `contributes.automation.methods` 中暴露给 MCP / CLI：

| 命令 | 参数 | 说明 |
|---|---|---|
| `mix` | `{request: MixRequest}` 或直接 MixRequest | 完整一键混音，返回 MixResult（`long: true`） |
| `mixFiles` | `{vocalsPath, accompanimentPath, outputPath, preset?}` | 快捷两轨混音（`long: true`） |
| `probe` | `{path}` | WAV 探测 |
| `models` | — | NN 模型注册表 |
| `presets` | — | 预设列表 |
| `defaults` | — | 默认 MixRequest + 预设 + 模型 + 模式 |
| `modes` | — | 输入模式（stems / separate） |
| `instruments` | — | 乐器类型与专属链 |

MCP 方法名 = `onemix-mixer.<命令>`。参数与结果 schema 见 `docs/API.md`。

## 运行时架构（以及必须知道的边界）

### 1) 为什么用 JS 移植而不是调用 Rust 二进制

v2 插件运行在 `utilityProcess`，宿主暴露的 `ctx` **没有受支持的子进程 API**，v2 也没有 `child_process` 权限组。当前宿主的 `sandbox.js` 自述「这不是沙箱，插件仍可 `require('node:fs')` 绕过」，`netGuard` 只拦网络模块——也就是说"偷偷 spawn"在技术上可能可行，但属于未声明能力，宿主一旦收紧就会失效。

因此采用：`sxseditor-plugin/engine.js` 把 `mixer-core` 的 DSP **逐算法移植**为 JS，在插件进程内完成混音；NN 用同一个 ONNX 模型。这样能力不依赖未声明的宿主行为。

### 2) 文件 IO：`ctx.fs` 只允许插件目录

这是最容易踩的坑。宿主的 `FsSandbox._resolve()` 把路径夹紧在 `pluginDir`（读）与 `dataDir`（写）内，`permissions.fs` 里的 `userPicked:read` **并没有接入 `ctx.fs`**（它表示"宿主把用户选中的那一个文件的 bytes 给你"，不是按路径读盘）。

所以 `engine.js` 的 `readBytes/writeBytes` 采用：

1. 先走 `ctx.fs`（插件目录 / 数据目录内的路径，受审计）；
2. 失败则回退 `node:fs`（用户在面板里填的绝对路径）。

如果未来宿主收紧 `require('node:fs')`，插件需要改为「base64 通过命令参数传入 / 宿主数据面」方案。这条边界在 `engine.js` 顶部有显式注释。

### 3) `ctx.ort` 的真实 API

宿主 `ortRunner.js` 的形态与常见 ORT 封装不同，必须严格按下面调用：

```js
const sess = await ctx.ort.load('models/onemix_dyn_enhance_v1.onnx', { ep: 'cpu' });
// sess = { handle: 's1', ep, inputs:[{name,type,dims}], outputs:[{name}] }

const out = await ctx.ort.run(sess.handle, {
  x: { type: 'float32', dims: [1, 1, 128], data: env }, // env: Float32Array
});
// out.y = { type:'float32', dims, data: Float32Array }

await ctx.ort.unload(sess.handle);
```

- `run` 的第一个参数是 **handle 字符串**，不是 `load()` 返回的对象；
- `feeds` 每项必须是 `{type, dims, data}`，不能直接传 `Float32Array`；
- `load` 只认 `options.ep`（`cpu`/`dml`），没有 `executionProviders`；`run` 没有 `shape` 选项。

### 4) 面板 RPC 必须自己分派 `command.run`

宿主把面板发来的 `method` **原样**透传给插件的 `panels.rpc`（`pluginRuntimeEntry.js`），只做 `apiScopes` 白名单校验。面板用 `command.run` 时，插件侧要自己按 `params.commandId` 分派——`main.js` 已实现，并对命令 id 做了白名单。

另外宿主调用命令是 `fn(ctx, args)`（裸函数），严格模式下 `this` 为 `undefined`，因此 `mixFiles` 里不能写 `this.mix(...)`。

### 5) 长任务超时

`mix` / `mixFiles` 已标记 `"long": true`，命令超时从 60s 提升到宿主的长任务上限。但面板 RPC 默认只有 30s，长音频仍可能 `PLUGIN_TIMEOUT`；生产环境建议走 MCP 的异步 job，而不是面板直调。

## 一致性验证

`scripts/verify_parity.sh` 用同一素材分别跑 Rust CLI 与插件 JS 引擎，逐项对比：

```
-- 传统两轨 --
指标                            Rust        JS插件
output_lufs                  -14.0       -14.0
output_true_peak_dbtp       -2.267      -2.267
-- 多轨分轨直入 --
output_lufs                  -14.0       -14.0
output_true_peak_dbtp       -3.117      -3.117
```

响度与真峰在小数点后三位完全一致，乐器推断一致。

## 重新打包

```bash
# 目录名必须等于 manifest.id（onemix-mixer），否则官方打包器会拒绝
rm -rf /tmp/onemix-mixer && cp -r sxseditor-plugin /tmp/onemix-mixer
node <sxs>/scripts/package-plugin.js /tmp/onemix-mixer -o dist-plugins

# 可选：自签名（需要先生成密钥）
node <sxs>/scripts/package-plugin.js /tmp/onemix-mixer -o dist-plugins \
  --key <keys>/plugin-signing.key --publisher OneMix
```

- 不要手工 zip：包内 `checksums.json` 必须与文件逐字节对应
- 包内不能有绝对路径/盘符/仅大小写不同的文件名
- 官方打包器把 zip 时间戳写成 0（列表显示 1980-00-00），这是上游行为，不影响宿主安装

## 权限说明

manifest 申请：`fs`（pluginData 写 + userPicked 读）、`ort`（cpu）、`project`（read）、`ui`（menu/panel/toolbar/statusBar/settings）、`automation`（expose）。

其中 `project` 当前未使用，`fs.userPicked` 也不被 `ctx.fs` 消费（见上文第 2 点）。这两项是为了与宿主未来的数据面 API 保持兼容而保留；如果你的安全策略要求"声明即实际使用"，可以删除 `project`。
