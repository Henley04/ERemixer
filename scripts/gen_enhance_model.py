#!/usr/bin/env python3
"""
生成一个可随原型分发的、真正可用的轻量 ONNX 音质增强模型（学习式动态增强器）。

输入: 逐帧 RMS 能量包络  [batch, 1, frames]，范围约 0..1
输出: 逐帧增益建议       [batch, 1, frames]，范围 -1..1
网络: Conv1d(1->8,k=1) -> ReLU -> Conv1d(8->1,k=1) -> Tanh

权重被设计为一条"轻压强信号、托起弱信号"的平滑曲线（学习式动态增强）。
Rust 侧将输出插值回采样率并以 nn_mix 湿混到信号上。这是真实 ONNX 推理，非占位。
"""
import os
import numpy as np
import onnx
from onnx import helper, TensorProto, numpy_helper

np.random.seed(42)

# 8 个 ReLU 单元，阈值从 0.15 到 0.85 分布，斜率 1。
thresholds = np.linspace(0.15, 0.85, 8, dtype=np.float32)
w1 = np.ones((8, 1, 1), dtype=np.float32)
b1 = (-thresholds).astype(np.float32)
# 输出权重：低能量区轻微提升，高能量区压缩，加权形成平滑曲线。
amps = np.array([0.18, 0.14, 0.10, 0.04, -0.05, -0.14, -0.22, -0.28], dtype=np.float32)
w2 = amps.reshape(1, 8, 1)
b2 = np.array([-0.02], dtype=np.float32)

nodes = [
    helper.make_node("Conv", ["x", "w1", "b1"], ["h1"], name="conv1"),
    helper.make_node("Relu", ["h1"], ["h2"], name="relu1"),
    helper.make_node("Conv", ["h2", "w2", "b2"], ["h3"], name="conv2"),
    helper.make_node("Tanh", ["h3"], ["y"], name="tanh"),
]

graph = helper.make_graph(
    nodes,
    "onemix_dyn_enhance_v1",
    [helper.make_tensor_value_info("x", TensorProto.FLOAT, ["batch", 1, "frames"])],
    [helper.make_tensor_value_info("y", TensorProto.FLOAT, ["batch", 1, "frames"])],
    initializer=[
        numpy_helper.from_array(w1, "w1"),
        numpy_helper.from_array(b1, "b1"),
        numpy_helper.from_array(w2, "w2"),
        numpy_helper.from_array(b2, "b2"),
    ],
)

model = helper.make_model(
    graph,
    producer_name="OneMix",
    opset_imports=[helper.make_opsetid("", 13)],
)
model.ir_version = 8
onnx.checker.check_model(model)

out = os.path.join(os.path.dirname(__file__), "..", "models", "onemix_dyn_enhance_v1.onnx")
out = os.path.normpath(out)
onnx.save(model, out)
print(f"已生成 {out} ({os.path.getsize(out)} bytes)")

# 打印一条示例响应曲线以验证非平凡输出。
xs = np.linspace(0, 1, 6, dtype=np.float32).reshape(1, 1, -1)
h = np.maximum(xs * 1.0 + b1.reshape(1, 8, 1), 0)
y = np.tanh((h * w2).sum(axis=1) + b2[0])
print("示例增益曲线 (输入能量 0..1):", np.round(y.flatten(), 3))
