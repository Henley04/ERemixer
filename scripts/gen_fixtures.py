#!/usr/bin/env python3
"""生成合成测试音频：人声（中频谐波 + 动态包络）与伴奏（低频鼓 + 和弦垫）。
仅用于端到端验证混音流水线，不依赖任何外部素材。"""
import os
import struct
import math
import wave

SR = 48000
DUR = 8.0
N = int(SR * DUR)


def write_wav_stereo(path, left, right, sr=SR):
    with wave.open(path, "wb") as w:
        w.setnchannels(2)
        w.setsampwidth(2)
        w.setframerate(sr)
        frames = bytearray()
        for l, r in zip(left, right):
            li = max(-32768, min(32767, int(l * 32767)))
            ri = max(-32768, min(32767, int(r * 32767)))
            frames += struct.pack("<hh", li, ri)
        w.writeframes(bytes(frames))


def make_vocals():
    # 人声：220Hz 基频 + 谐波，带句子级包络（模拟演唱动态）。
    left, right = [], []
    for i in range(N):
        t = i / SR
        # 每 2 秒一句，句内渐强渐弱。
        phrase = (t % 2.0) / 2.0
        env = math.sin(math.pi * phrase) ** 0.7
        f0 = 220.0 + 20.0 * math.sin(2 * math.pi * 0.25 * t)
        sig = (
            math.sin(2 * math.pi * f0 * t)
            + 0.5 * math.sin(2 * math.pi * 2 * f0 * t)
            + 0.25 * math.sin(2 * math.pi * 3 * f0 * t)
        ) * 0.16 * env
        # 轻微左右差异（人声居中略宽）。
        left.append(sig * (1.0 + 0.05 * math.sin(2 * math.pi * 0.1 * t)))
        right.append(sig * (1.0 - 0.05 * math.sin(2 * math.pi * 0.1 * t)))
    return left, right


def make_accompaniment():
    # 伴奏：低频 kick（每 0.5s）+ 中频和弦垫（110/165/220Hz）。
    left, right = [], []
    for i in range(N):
        t = i / SR
        # kick：短促低频包络。
        beat = (t % 0.5) / 0.5
        kick = math.sin(2 * math.pi * 60 * t) * math.exp(-beat * 8) * 0.35
        # pad：稳定和弦。
        pad = (
            math.sin(2 * math.pi * 110 * t)
            + 0.6 * math.sin(2 * math.pi * 165 * t)
            + 0.4 * math.sin(2 * math.pi * 220 * t)
        ) * 0.06
        # hi-hat：高频噪声近似（用高频正弦簇）。
        hat = math.sin(2 * math.pi * 8000 * t) * math.exp(-((t % 0.25) / 0.25) * 20) * 0.02
        mono = kick + pad + hat
        left.append(mono * 0.95)
        right.append(mono * 1.05)  # 伴奏略宽
    return left, right


if __name__ == "__main__":
    out = os.path.join(os.path.dirname(__file__), "..", "examples", "fixtures")
    out = os.path.normpath(out)
    os.makedirs(out, exist_ok=True)
    vl, vr = make_vocals()
    al, ar = make_accompaniment()
    write_wav_stereo(os.path.join(out, "vocals.wav"), vl, vr)
    write_wav_stereo(os.path.join(out, "accompaniment.wav"), al, ar)
    print(f"已生成测试音频到 {out} ({DUR}s @ {SR}Hz)")
