# -*- coding: utf-8 -*-
"""Read-only инспекция ONNX-графов Shtorm-v2 (для PREPARATION_REPORT).

Печатает: opset, входы/выходы (тип, размерности, динамические оси),
гистограмму типов инициализаторов (весов), список операторов,
подозрительные на квантование узлы (DynamicQuantizeLinear/QLinear*/ConvInteger).
Никаких файлов не меняет и ничего не скачивает.
"""
import sys
import io
import os
from collections import Counter

sys.stdout = io.TextIOWrapper(sys.stdout.buffer, encoding="utf-8", errors="replace")

import onnx
from onnx import TensorProto

DT = {
    TensorProto.FLOAT: "f32",
    TensorProto.FLOAT16: "f16",
    TensorProto.BFLOAT16: "bf16",
    TensorProto.INT8: "i8",
    TensorProto.UINT8: "u8",
    TensorProto.INT32: "i32",
    TensorProto.INT64: "i64",
    TensorProto.BOOL: "bool",
    TensorProto.DOUBLE: "f64",
}

QUANT_OPS = {
    "QuantizeLinear", "DequantizeLinear", "DynamicQuantizeLinear",
    "QuantizeLinearPerChannel", "QLinearConv", "QLinearMatMul",
    "ConvInteger", "MatMulInteger", "QGemm", "QLinearAdd",
}


def shape_of(v):
    parts = []
    for d in v.type.tensor_type.shape.dim:
        if d.HasField("dim_param"):
            parts.append(d.dim_param)
        elif d.HasField("dim_value"):
            parts.append(str(d.dim_value))
        else:
            parts.append("?")
    return "[" + ",".join(parts) + "]"


def inspect(path):
    m = onnx.load(path, load_external_data=False)
    print(f"\n=== {os.path.basename(os.path.dirname(path))}/{os.path.basename(path)} ===")
    sz = os.path.getsize(path)
    print(f"size: {sz/1e6:.1f} MB")
    for opset in m.opset_import:
        if opset.domain in ("", "ai.onnx"):
            print(f"opset: {opset.version}")
    ir = getattr(m, "ir_version", "?")
    print(f"ir_version: {ir}")
    print("inputs:")
    for v in m.graph.input:
        t = v.type.tensor_type
        print(f"  {v.name}: {DT.get(t.elem_type, t.elem_type)}{shape_of(v)}")
    print("outputs:")
    for v in m.graph.output:
        t = v.type.tensor_type
        print(f"  {v.name}: {DT.get(t.elem_type, t.elem_type)}{shape_of(v)}")
    inits = Counter(DT.get(i.data_type, i.data_type) for i in m.graph.initializer)
    total_params = sum(
        1 for i in m.graph.initializer if i.data_type == TensorProto.FLOAT
    )
    print(f"initializers ({len(m.graph.initializer)}): {dict(inits)}")
    ops = Counter(n.op_type for n in m.graph.node)
    quant = sorted(ops.keys() & QUANT_OPS)
    print(f"nodes: {len(m.graph.node)}, unique ops: {len(ops)}")
    top = ", ".join(f"{k}x{v}" for k, v in ops.most_common(12))
    print(f"top ops: {top}")
    print(f"quantized-op nodes: {quant if quant else 'NONE (graph is pure FP32)'}")
    ext = [i for i in m.graph.initializer if i.data_location == 3]
    if ext:
        print(f"external data initializers: {len(ext)}")


if __name__ == "__main__":
    for p in sys.argv[1:]:
        inspect(p)
