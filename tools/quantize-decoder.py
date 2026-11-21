# /// script
# requires-python = "==3.13.*"
# dependencies = ["onnxruntime==1.32.0", "onnx==1.23.2", "numpy==2.5.5"]
# ///
"""Makes decoder_joint.int8.onnx, the decoder PoodPim downloads from its GitHub release
(tag model-decoder-int8-1), from decoder_joint.onnx in iapp/OpenThai2.0-ASR-Streaming-ONNX
(the int8 zip of the revision pinned in src/model.rs).

  uv run tools/quantize-decoder.py <model dir>/decoder_joint.onnx decoder_joint.int8.onnx

Dynamic int8 quantization of the weights (MatMul and the LSTMs): 102 MB → 27 MB, and the
beam search spends ~2.7x less time in the decoder. On tools/asr-eval with the bundled
vocabulary it was as accurate as the fp32 decoder: CER 24.0 vs 23.9% (tech set, 60 clips),
26.0 vs 26.0% (BA set, 36 clips); English words found 63.0 vs 63.7%, 50.0 vs 48.8%.
With these versions the output is byte for byte the file in the release
(SHA-256 f5f5a06684cb64e94487d9b1d411ec0829c7a6fa5c1af74c87c0e1a9577f6c7e).
"""
import sys

from onnxruntime.quantization import QuantType, quantize_dynamic

quantize_dynamic(sys.argv[1], sys.argv[2], weight_type=QuantType.QInt8)
