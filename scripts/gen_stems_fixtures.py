"""生成 4 条乐器分轨测试素材（人声/鼓/贝斯/其他），用于验证 Stems 模式。"""
import numpy as np, wave, struct, os
sr = 48000; dur = 6.0; n = int(sr*dur)
t = np.arange(n, dtype=np.float64)/sr
os.makedirs('examples/fixtures/stems', exist_ok=True)

def write(path, x):
    x = np.clip(x, -1, 1)
    with wave.open(path,'w') as w:
        w.setnchannels(1); w.setsampwidth(2); w.setframerate(sr)
        w.writeframes(b''.join(struct.pack('<h', int(v*32767)) for v in x))

# 人声：基频 + 共振峰式泛音，带缓慢音量起伏（模拟乐句）
f0 = 220
v = 0.5*np.sin(2*np.pi*f0*t)
v += 0.25*np.sin(2*np.pi*2*f0*t) + 0.12*np.sin(2*np.pi*3*f0*t)
env = 0.35 + 0.65*np.abs(np.sin(2*np.pi*0.6*t))
v *= env
write('examples/fixtures/stems/lead_vocals.wav', 0.5*v)

# 鼓：低音鼓（每 0.5s 一次衰减正弦）+ 高频噪声军鼓
kick = np.zeros(n)
for beat in np.arange(0, dur, 0.5):
    i0 = int(beat*sr)
    seg = np.sin(2*np.pi*(60-30*np.linspace(0,1,2400))*np.arange(2400)/sr) * np.exp(-np.arange(2400)/1200)
    kick[i0:i0+2400] += seg[:max(0,min(2400,n-i0))]
rng = np.random.default_rng(7)
snare = rng.normal(0,1,n)*np.exp(-((t%1.0-0.5)/0.02)**2)*0.15
write('examples/fixtures/stems/drum_kit.wav', 0.7*(kick+snare))

# 贝斯：低频正弦 + 二次谐波
b = 0.6*np.sin(2*np.pi*55*t) + 0.2*np.sin(2*np.pi*110*t)
write('examples/fixtures/stems/electric_bass.wav', 0.6*b)

# 其他乐器（吉他/键盘）：和弦
chord = sum(np.sin(2*np.pi*f*t) for f in [261.6, 329.6, 392.0, 523.3])/4
chord *= 0.5+0.5*np.abs(np.sin(2*np.pi*0.3*t))
write('examples/fixtures/stems/guitar_keys.wav', 0.45*chord)

# 成品混音（用于 Separate 模式降级测试）：把四轨叠加
full = 0.5*v + 0.7*(kick+snare) + 0.6*b + 0.45*chord
write('examples/fixtures/fullmix.wav', 0.5*full)
print('stems fixtures ready')
