"""CUDA-P2A: генератор синтетических ONNX-графов для тестов I/O Binding.

Запуск: C:\PocketTTS-GUI\venv\Scripts\python.exe tools/gen_p2a_models.py
       P2A_REGEN=1 — перезаписать уже созданные файлы (по умолчанию — нет).
Куда: crates/shtorm-engine/tests/models (P2A_MODELS_DIR переопределяет).
Готовые модели уже лежат в репозитории; генератор нужен для пересоздания
(например, после правки графа). Файлы крошечные (0.3–2.6 КБ).

Модели:
  stateful.onnx  — state_0/1/2 + динамическая sequence [1,N,4] +
                   zero-size text_embeddings [1,0,8]; ping-pong/reset/типы.
  dynamic.onnx   — state_0 [1,N,4]: динамическая форма состояния (отвергается).
  badnames.onnx  — вход state_0, выход out_state_x (несовпадение имён).
  badtype.onnx   — вход state_0 f32, выход out_state_0 i64 (тип не совпал).
  bigstate.onnx  — 12 x [1,1000,16,64] f32 + 6 x [1] i64, как state_* main:
                   замер стоимости I/O (94 MiB round-trip на прогон).
"""
import os
import sys

import numpy as np
import onnx
from onnx import TensorProto, helper, numpy_helper

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
OUT = os.environ.get("P2A_MODELS_DIR") or os.path.join(
    ROOT, "crates", "shtorm-engine", "tests", "models")
REGEN = os.environ.get("P2A_REGEN") == "1"


def save(model, name):
    path = os.path.join(OUT, name)
    if os.path.exists(path) and not REGEN:
        print(f"skip (exists): {path}")
        return
    model.ir_version = 10  # ORT 1.23 принимает IR <= 11; onnx 1.23 пишет 14
    onnx.checker.check_model(model)
    tmp = path + ".tmp"
    onnx.save(model, tmp)
    os.replace(tmp, path)
    print(f"wrote: {path} ({os.path.getsize(path)} bytes)")


def stateful():
    # CUDA-ядро ReduceSum в ORT 1.23 не умеет reduce по пустой оси даже с
    # keepdims=1 — поэтому ни sequence [1,N,4], ни text_embeddings [1,0,8] не
    # редуцируются напрямую: используются Shape/Gather (работают с пустыми
    # тензорами) и Concat с нулевой строкой перед reduce по непустой оси.
    inputs = [
        helper.make_tensor_value_info("sequence", TensorProto.FLOAT, [1, None, 4]),
        helper.make_tensor_value_info("text_embeddings", TensorProto.FLOAT, [1, 0, 8]),
        helper.make_tensor_value_info("state_0", TensorProto.FLOAT, [1, 4]),
        helper.make_tensor_value_info("state_1", TensorProto.FLOAT, [1, 4]),
        helper.make_tensor_value_info("state_2", TensorProto.INT64, [1]),
    ]
    outputs = [
        helper.make_tensor_value_info("conditioning", TensorProto.FLOAT, [4]),
        helper.make_tensor_value_info("eos_logit", TensorProto.FLOAT, [1]),
        helper.make_tensor_value_info("out_state_0", TensorProto.FLOAT, [1, 4]),
        helper.make_tensor_value_info("out_state_1", TensorProto.FLOAT, [1, 4]),
        helper.make_tensor_value_info("out_state_2", TensorProto.INT64, [1]),
    ]
    inits = {
        "one": np.array([1.0], dtype=np.float32),
        "half": np.array([0.5], dtype=np.float32),
        "zero": np.array([0.0], dtype=np.float32),
        "quarter": np.array([0.25], dtype=np.float32),
        "one_i64": np.array([1], dtype=np.int64),
        "idx1": np.array(1, dtype=np.int64),
        "axe1": np.array([1], dtype=np.int64),
        "axe0": np.array([0], dtype=np.int64),
        "shape1": np.array([1], dtype=np.int64),
        "shape14": np.array([1, 4], dtype=np.int64),
        "shape114": np.array([1, 1, 4], dtype=np.int64),
        "zero_row": np.zeros((1, 1, 4), dtype=np.float32),
    }
    nodes = [
        # sequence: [1,N,4] -> concat с нулевой строкой -> reduce по оси 1
        helper.make_node("Concat", ["sequence", "zero_row"], ["seq_pad"], axis=1),
        helper.make_node("ReduceSum", ["seq_pad", "axe1"], ["seq_sum"], keepdims=1),
        # text_embeddings [1,0,8]: Shape/Gather -> 0.0 (проверка zero-size входа)
        helper.make_node("Shape", ["text_embeddings"], ["emb_shape"]),
        helper.make_node("Gather", ["emb_shape", "idx1"], ["emb_n"], axis=0),
        helper.make_node("Cast", ["emb_n"], ["emb_nf"], to=TensorProto.FLOAT),
        helper.make_node("Mul", ["emb_nf", "zero"], ["emb_zero"]),
        # out_state_0 = (state_0 + seq_sum + 1) с точностью до формы [1,4]
        helper.make_node("Reshape", ["state_0", "shape114"], ["state_0_4d"]),
        helper.make_node("Add", ["state_0_4d", "seq_sum"], ["s0a"]),
        helper.make_node("Add", ["s0a", "one"], ["s0b"]),
        helper.make_node("Add", ["s0b", "emb_zero"], ["s0c"]),
        helper.make_node("Reshape", ["s0c", "shape14"], ["out_state_0"]),
        # out_state_1 = 0.5 * state_1 + state_0
        helper.make_node("Mul", ["state_1", "half"], ["s1a"]),
        helper.make_node("Add", ["s1a", "state_0"], ["out_state_1"]),
        helper.make_node("Add", ["state_2", "one_i64"], ["out_state_2"]),
        # conditioning зависит от ВСЕХ трёх новых состояний (проверка переноса)
        helper.make_node("Cast", ["out_state_2"], ["state2f"], to=TensorProto.FLOAT),
        helper.make_node("Mul", ["state2f", "quarter"], ["state2q"]),
        helper.make_node("Add", ["out_state_0", "out_state_1"], ["cond_a"]),
        helper.make_node("Add", ["cond_a", "state2q"], ["cond4d"]),
        helper.make_node("Reshape", ["cond4d", "shape14"], ["cond_14"]),
        helper.make_node("ReduceSum", ["cond_14", "axe1"], ["cond_1"], keepdims=1),
        helper.make_node("Reshape", ["cond_1", "shape1"], ["conditioning"]),
        # eos_logit = длина sequence (0 для префиксного прогона) как [1]
        helper.make_node("Shape", ["sequence"], ["seq_shape"]),
        helper.make_node("Gather", ["seq_shape", "idx1"], ["seq_n"], axis=0),
        helper.make_node("Cast", ["seq_n"], ["seq_nf"], to=TensorProto.FLOAT),
        helper.make_node("Reshape", ["seq_nf", "shape1"], ["eos_logit"]),
    ]
    initializers = [numpy_helper.from_array(v, k) for k, v in inits.items()]
    graph = helper.make_graph(nodes, "stateful", inputs, outputs, initializers)
    save(helper.make_model(graph, opset_imports=[helper.make_opsetid("", 17)]), "stateful.onnx")


def dynamic():
    inputs = [
        helper.make_tensor_value_info("sequence", TensorProto.FLOAT, [1, None, 4]),
        helper.make_tensor_value_info("state_0", TensorProto.FLOAT, [1, None, 4]),
    ]
    outputs = [
        helper.make_tensor_value_info("out_state_0", TensorProto.FLOAT, [1, None, 4]),
        helper.make_tensor_value_info("conditioning", TensorProto.FLOAT, [4]),
        helper.make_tensor_value_info("eos_logit", TensorProto.FLOAT, [1]),
    ]
    nodes = [
        helper.make_node("Add", ["state_0", "sequence"], ["out_state_0"]),
        helper.make_node("ReduceSum", ["out_state_0", "axes012"], ["cond_keep"], keepdims=1),
        helper.make_node("Reshape", ["cond_keep", "shape4"], ["conditioning"]),
        helper.make_node("ReduceSum", ["sequence", "axes01"], ["eos_logit"], keepdims=1),
    ]
    inits = [
        numpy_helper.from_array(np.array([0, 1, 2], dtype=np.int64), "axes012"),
        numpy_helper.from_array(np.array([4], dtype=np.int64), "shape4"),
        numpy_helper.from_array(np.array([0, 1], dtype=np.int64), "axes01"),
    ]
    graph = helper.make_graph(nodes, "dynamic", inputs, outputs, inits)
    save(helper.make_model(graph, opset_imports=[helper.make_opsetid("", 17)]), "dynamic.onnx")


def badnames():
    inputs = [helper.make_tensor_value_info("state_0", TensorProto.FLOAT, [1, 4])]
    outputs = [
        helper.make_tensor_value_info("out_state_x", TensorProto.FLOAT, [1, 4]),
        helper.make_tensor_value_info("conditioning", TensorProto.FLOAT, [4]),
        helper.make_tensor_value_info("eos_logit", TensorProto.FLOAT, [1]),
    ]
    nodes = [
        helper.make_node("Add", ["state_0", "one"], ["out_state_x"]),
        helper.make_node("Reshape", ["out_state_x", "shape4"], ["conditioning"]),
        helper.make_node("ReduceSum", ["out_state_x", "axes01"], ["eos_logit"], keepdims=1),
    ]
    inits = [
        numpy_helper.from_array(np.array([1.0], dtype=np.float32), "one"),
        numpy_helper.from_array(np.array([4], dtype=np.int64), "shape4"),
        numpy_helper.from_array(np.array([0, 1], dtype=np.int64), "axes01"),
    ]
    graph = helper.make_graph(nodes, "badnames", inputs, outputs, inits)
    save(helper.make_model(graph, opset_imports=[helper.make_opsetid("", 17)]), "badnames.onnx")


def badtype():
    inputs = [helper.make_tensor_value_info("state_0", TensorProto.FLOAT, [1, 4])]
    outputs = [
        helper.make_tensor_value_info("out_state_0", TensorProto.INT64, [1, 4]),
        helper.make_tensor_value_info("conditioning", TensorProto.FLOAT, [4]),
        helper.make_tensor_value_info("eos_logit", TensorProto.FLOAT, [1]),
    ]
    nodes = [
        helper.make_node("Cast", ["state_0"], ["out_state_0"], to=TensorProto.INT64),
        helper.make_node("Reshape", ["state_0", "shape4"], ["conditioning"]),
        helper.make_node("ReduceSum", ["state_0", "axes01"], ["eos_logit"], keepdims=1),
    ]
    inits = [
        numpy_helper.from_array(np.array([4], dtype=np.int64), "shape4"),
        numpy_helper.from_array(np.array([0, 1], dtype=np.int64), "axes01"),
    ]
    graph = helper.make_graph(nodes, "badtype", inputs, outputs, inits)
    save(helper.make_model(graph, opset_imports=[helper.make_opsetid("", 17)]), "badtype.onnx")


def bigstate():
    # 12 f32 [1,1000,16,64] (~3.9 MiB каждый) + 6 i64 [1] — копия структуры
    # state_* main-графа. Измеряет стоимость I/O: старая схема гоняет
    # ~94 MiB/прогон CPU<->GPU, новая — только Add на устройстве.
    shapes = [[1, 1000, 16, 64]] * 12 + [[1]] * 6
    inputs = [helper.make_tensor_value_info("sequence", TensorProto.FLOAT, [1, None, 32])]
    outputs = [
        helper.make_tensor_value_info("conditioning", TensorProto.FLOAT, [4]),
        helper.make_tensor_value_info("eos_logit", TensorProto.FLOAT, [1]),
    ]
    nodes = []
    inits = {
        "one": np.array(1.0, dtype=np.float32),
        "one_i64": np.array(1, dtype=np.int64),
        "idx1": np.array(1, dtype=np.int64),
        "zero": np.array(0.0, dtype=np.float32),
        "shape1": np.array([1], dtype=np.int64),
        "slice_starts": np.array([0, 0, 0, 0], dtype=np.int64),
        "slice_ends": np.array([1, 1, 1, 4], dtype=np.int64),
        "slice_axes": np.array([0, 1, 2, 3], dtype=np.int64),
        "shape4": np.array([4], dtype=np.int64),
    }
    nodes.append(helper.make_node("Shape", ["sequence"], ["seq_shape"]))
    nodes.append(helper.make_node("Gather", ["seq_shape", "idx1"], ["seq_n"], axis=0))
    nodes.append(helper.make_node("Cast", ["seq_n"], ["seq_f"], to=TensorProto.FLOAT))
    nodes.append(helper.make_node("Mul", ["seq_f", "zero"], ["seq0"]))
    for i, shp in enumerate(shapes):
        inputs.append(helper.make_tensor_value_info(f"state_{i}", TensorProto.FLOAT if len(shp) == 4 else TensorProto.INT64, shp))
        outputs.append(helper.make_tensor_value_info(f"out_state_{i}", TensorProto.FLOAT if len(shp) == 4 else TensorProto.INT64, shp))
        one = "one" if len(shp) == 4 else "one_i64"
        if i == 0:
            nodes.append(helper.make_node("Add", ["state_0", "one"], ["s0a"]))
            nodes.append(helper.make_node("Add", ["s0a", "seq0"], ["out_state_0"]))
        else:
            nodes.append(helper.make_node(f"Add", [f"state_{i}", one], [f"out_state_{i}"]))
    nodes.append(helper.make_node("Slice", ["out_state_0", "slice_starts", "slice_ends", "slice_axes"], ["cond_slice"]))
    nodes.append(helper.make_node("Reshape", ["cond_slice", "shape4"], ["conditioning"]))
    nodes.append(helper.make_node("Reshape", ["seq_f", "shape1"], ["eos_logit"]))
    initializers = [numpy_helper.from_array(v, k) for k, v in inits.items()]
    graph = helper.make_graph(nodes, "bigstate", inputs, outputs, initializers)
    save(helper.make_model(graph, opset_imports=[helper.make_opsetid("", 17)]), "bigstate.onnx")


def main():
    os.makedirs(OUT, exist_ok=True)
    stateful()
    dynamic()
    badnames()
    badtype()
    bigstate()
    return 0


if __name__ == "__main__":
    sys.exit(main())
