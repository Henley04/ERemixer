"""StemFX -> ONNX (W8A32) exporter.

Splits the StemFX model into two ONNX graphs:

* encoder  : mel power (1,8,80,862) + mixing features (1,64) -> L2-normalized 512-d embedding
* generator: (emb_orig, emb_target, tokens) -> next-token logits (1,358)

Both are then quantized weight-only to int8 (MatMulNBits, bits=8, fp32 activations)
and verified against the PyTorch reference.

通常不直接调用，而是用 `scripts/fetch_stemfx.py`（它会先下权重与上游代码）：

    python scripts/fetch_stemfx.py

单独调用：

    python tools/stemfx_export/export_stemfx.py \
        --ckpt build/stemfx/best_checkpoint.pt \
        --repo build/stemfx/stemfx \
        --out  models/stemfx
"""
from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path

import numpy as np
import torch
import torch.nn as nn
import torch.nn.functional as F

D, H, HD = 512, 8, 64


# --------------------------------------------------------------------------- encoder
class EncoderWrap(nn.Module):
    """mel power + mixing features -> L2-normalized embedding."""

    def __init__(self, enc):
        super().__init__()
        ae = enc.audio_encoder
        self.inorm = ae.mel_preprocessor.instance_norm
        self.film = enc.film_encoder
        self.subnets = ae.subnet_cnns
        self.pool = ae.attention_pooling
        self.n_sub = ae.n_subbands
        self.split_size = ae.split_size
        self.overlap = ae.overlap

    def forward(self, mel, feats):
        x = self.inorm(torch.log(mel + 1e-10))
        fp = self.film(feats)
        outs = []
        for i in range(self.n_sub):
            s = i * self.overlap
            sub = x[:, :, s:s + self.split_size, :]
            outs.append(self.subnets[i](
                sub, fp[f"gamma1_{i}"], fp[f"beta1_{i}"],
                fp[f"gamma2_{i}"], fp[f"beta2_{i}"]))
        cat = torch.stack(outs, dim=1)                       # (B,K,C,F,T)
        b, k, c, f, t = cat.shape
        emb = self.pool(cat.reshape(b, k * c * f, t))
        return F.normalize(emb, p=2, dim=-1)


# --------------------------------------------------------------------------- generator
class Attn(nn.Module):
    """Separate q/k/v projections so the weights stay visible to quantization."""

    def __init__(self, mha):
        super().__init__()
        w, b = mha.in_proj_weight, mha.in_proj_bias
        self.register_buffer("wq", w[:D].clone())
        self.register_buffer("wk", w[D:2 * D].clone())
        self.register_buffer("wv", w[2 * D:].clone())
        self.register_buffer("bq", b[:D].clone())
        self.register_buffer("bk", b[D:2 * D].clone())
        self.register_buffer("bv", b[2 * D:].clone())
        self.register_buffer("wo", mha.out_proj.weight.clone())
        self.register_buffer("bo", mha.out_proj.bias.clone())

    def forward(self, x, mem, mask):
        q = F.linear(x, self.wq, self.bq)
        k = F.linear(mem, self.wk, self.bk)
        v = F.linear(mem, self.wv, self.bv)
        b, s, _ = q.shape
        q = q.view(b, s, H, HD).transpose(1, 2)
        k = k.view(b, -1, H, HD).transpose(1, 2)
        v = v.view(b, -1, H, HD).transpose(1, 2)
        scores = torch.matmul(q, k.transpose(-1, -2)) * (HD ** -0.5)
        if mask is not None:
            scores = scores.masked_fill(mask.unsqueeze(0).unsqueeze(0), float("-inf"))
        p = torch.softmax(scores, dim=-1)
        o = torch.matmul(p, v).transpose(1, 2).reshape(b, s, D)
        return F.linear(o, self.wo, self.bo)


class DecLayer(nn.Module):
    def __init__(self, l):
        super().__init__()
        self.n1, self.n2, self.n3 = l.norm1, l.norm2, l.norm3
        self.sa, self.ca = Attn(l.self_attn), Attn(l.multihead_attn)
        self.l1, self.l2 = l.linear1, l.linear2

    def forward(self, x, mem, mask):
        x = self.n1(x + self.sa(x, x, mask))
        x = self.n2(x + self.ca(x, mem, None))
        return self.n3(x + self.l2(F.relu(self.l1(x))))


class GeneratorWrap(nn.Module):
    def __init__(self, gen):
        super().__init__()
        self.gen = gen
        self.layers = nn.ModuleList([DecLayer(l) for l in gen.transformer_decoder.layers])

    def forward(self, emb_orig, emb_tgt, tokens):
        mem = self.gen.embedding_proj(
            torch.stack([emb_orig[0], emb_tgt[0]], dim=0).unsqueeze(0))
        mem = mem + self.gen.memory_pos_embedding.unsqueeze(0)
        x = self.gen.pos_encoding(self.gen.token_embedding(tokens))
        s = tokens.size(1)
        idx = torch.arange(s, device=tokens.device)
        mask = idx[:, None] < idx[None, :]
        for l in self.layers:
            x = l(x, mem, mask)
        return self.gen.token_head(x[:, -1:, :]).squeeze(1)


# --------------------------------------------------------------------------- conv w8
def conv_weight_int8(model):
    """Weight-only int8 for Conv (int8 initializer + DequantizeLinear, fp32 activations)."""
    from onnx import helper, numpy_helper

    inits = {i.name: i for i in model.graph.initializer}
    added_nodes, added_inits = [], []
    for node in model.graph.node:
        if node.op_type != "Conv":
            continue
        wname = node.input[1]
        if wname not in inits:
            continue
        w = numpy_helper.to_array(inits[wname]).astype(np.float32)
        flat = w.reshape(w.shape[0], -1)
        scale = np.abs(flat).max(axis=1) / 127.0
        scale[scale == 0] = 1e-8
        q = np.clip(np.round(flat / scale[:, None]), -127, 127).astype(np.int8).reshape(w.shape)
        qn, sn, dn = f"{wname}_q8", f"{wname}_s8", f"{wname}_dq8"
        added_inits.append(numpy_helper.from_array(q, qn))
        added_inits.append(numpy_helper.from_array(scale.astype(np.float32), sn))
        added_nodes.append(helper.make_node(
            "DequantizeLinear", [qn, sn], [dn], name=f"{node.name}_w8dq", axis=0))
        node.input[1] = dn
    model.graph.node.extend(added_nodes)
    model.graph.initializer.extend(added_inits)
    used = {i for nd in model.graph.node for i in nd.input}
    keep = [i for i in model.graph.initializer if i.name in used or i.name in {a.name for a in added_inits}]
    del model.graph.initializer[:]
    model.graph.initializer.extend(keep)
    return model


def build_net(ckpt_path, repo_dir):
    sys.path.insert(0, str(repo_dir))
    from stemfx.model import StemFX

    ck = torch.load(ckpt_path, map_location="cpu", weights_only=False)
    a = ck["args"]
    net = StemFX(
        encoder_dim=a["encoder_dim"], feature_dim=a["feature_dim"], n_mels=a["n_mels"],
        n_fft=a["n_fft"], hop_length=a["hop_length"], sample_rate=44100, use_film=True,
        encoder_backbone=a["encoder_backbone"], vocab_size=358, hidden_dim=a["hidden_dim"],
        num_layers=a["num_layers"], num_heads=a["num_heads"],
        feedforward_dim=a["feedforward_dim"], dropout=a["dropout"],
        max_seq_len=a["max_seq_len"], pad_token_idx=0, bos_token_idx=1,
        num_memory_tokens=a["num_memory_tokens"],
    )
    net.load_state_dict(ck["model_state_dict"], strict=True)
    return net.eval(), ck


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--ckpt", required=True)
    ap.add_argument(
        "--repo",
        default=str(Path(__file__).resolve().parents[2] / "build" / "stemfx" / "stemfx"),
        help="上游 stemfx 仓库路径（scripts/fetch_stemfx.py 会克隆到这里）",
    )
    ap.add_argument("--out", required=True)
    ap.add_argument("--no-quant", action="store_true")
    ap.add_argument("--block-size", type=int, default=32)
    args = ap.parse_args()

    out = Path(args.out)
    out.mkdir(parents=True, exist_ok=True)
    net, ck = build_net(args.ckpt, args.repo)
    torch.set_grad_enabled(False)
    try:
        torch.backends.mha.set_fastpath_enabled(False)
    except Exception:
        pass

    fb = net.encoder.audio_encoder.mel_preprocessor.mel_transform.mel_scale.fb.numpy().astype(np.float32)
    fb.tofile(out / "mel_fb.bin")
    print("mel fb", fb.shape)

    torch.manual_seed(0)
    mel = torch.rand(1, 8, 80, 862).abs() * 0.5 + 1e-6
    feats = torch.zeros(1, 64)
    enc = EncoderWrap(net.encoder).eval()
    e_ref = enc(mel, feats)
    torch.onnx.export(
        enc, (mel, feats), str(out / "encoder_fp32.onnx"),
        input_names=["mel", "features"], output_names=["embedding"],
        opset_version=17, dynamo=False, do_constant_folding=True)
    print("encoder exported", round((out / "encoder_fp32.onnx").stat().st_size / 1e6, 2), "MB")

    gen = GeneratorWrap(net.generator).eval()
    e1 = F.normalize(torch.randn(1, 512), dim=-1)
    e2 = F.normalize(torch.randn(1, 512), dim=-1)
    toks = torch.tensor([[1, 4, 8, 9, 257, 2]], dtype=torch.long)
    logits_ref = gen(e1, e2, toks)
    torch.onnx.export(
        gen, (e1, e2, toks), str(out / "generator_fp32.onnx"),
        input_names=["emb_orig", "emb_target", "tokens"], output_names=["logits"],
        opset_version=17, dynamo=False, do_constant_folding=True,
        dynamic_axes={"tokens": {1: "seq"}})
    print("generator exported", round((out / "generator_fp32.onnx").stat().st_size / 1e6, 2), "MB")

    if not args.no_quant:
        import onnx
        from onnxruntime.quantization.matmul_nbits_quantizer import MatMulNBitsQuantizer

        for name, do_conv in (("encoder", True), ("generator", False)):
            fp = out / f"{name}_fp32.onnx"
            if do_conv:
                onnx.save(conv_weight_int8(onnx.load(str(fp))), str(fp))
            q = MatMulNBitsQuantizer(
                str(fp), bits=8, block_size=args.block_size,
                is_symmetric=True, accuracy_level=4,
                nodes_to_exclude=[n.name for n in onnx.load(str(fp)).graph.node if "token_head" in n.name])
            q.process()
            q.model.save_model_to_file(str(out / f"{name}_w8a32.onnx"), use_external_data_format=False)
            print(name, "quantized", round((out / f"{name}_w8a32.onnx").stat().st_size / 1e6, 2), "MB")

    import onnxruntime as ort

    def run(path, feeds):
        s = ort.InferenceSession(path, providers=["CPUExecutionProvider"])
        return s.run(None, feeds)

    tags = ["encoder_fp32"] if args.no_quant else ["encoder_fp32", "encoder_w8a32"]
    for tag in tags:
        y = run(str(out / f"{tag}.onnx"), {"mel": mel.numpy(), "features": feats.numpy()})[0]
        print(f"{tag}: max|d|={np.abs(y - e_ref.numpy()).max():.3e} "
              f"cos={float(np.dot(y[0], e_ref.numpy()[0]) / (np.linalg.norm(y) * np.linalg.norm(e_ref))):.6f}")

    tags = ["generator_fp32"] if args.no_quant else ["generator_fp32", "generator_w8a32"]
    for tag in tags:
        y = run(str(out / f"{tag}.onnx"),
                {"emb_orig": e1.numpy(), "emb_target": e2.numpy(), "tokens": toks.numpy()})[0]
        print(f"{tag}: max|d|={np.abs(y - logits_ref.numpy()).max():.3e} "
              f"argmax={int(y.argmax())} ref={int(logits_ref.argmax())}")

    from stemfx.tokenizer import FXChainTokenizer
    from stemfx.normalization import PARAM_RANGES
    tok = FXChainTokenizer()
    meta = {
        "name": "stemfx-bsfilm",
        "paper": "StemFX (ISMIR 2026), arXiv:2607.15634",
        "ckpt": Path(args.ckpt).name,
        "sample_rate": 44100,
        "segment_samples": 441000,
        "n_fft": 2048,
        "hop_length": 512,
        "n_mels": 80,
        "n_subbands": net.encoder.audio_encoder.n_subbands,
        "split_size": net.encoder.audio_encoder.split_size,
        "overlap": net.encoder.audio_encoder.overlap,
        "encoder_dim": 512,
        "quantization": "W8A32 (int8 weights, fp32 activations, 32-sample blocks, symmetric)",
        "vocab_size": tok.vocab_size,
        "value_token_start": tok.value_token_start,
        "num_value_bins": tok.NUM_VALUE_BINS,
        "stems": tok.STEMS,
        "tokens": [tok.idx_to_token[i] for i in range(tok.vocab_size)],
        "params": {k: {"min": v[0], "max": v[1], "log": bool(v[2])} for k, v in PARAM_RANGES.items()},
    }
    (out / "meta.json").write_text(json.dumps(meta, ensure_ascii=False, indent=1))
    print("meta.json written")


if __name__ == "__main__":
    main()
