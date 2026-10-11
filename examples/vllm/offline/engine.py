"""Process-local vLLM engine used by the App DAS reuse example."""

import os
from dataclasses import dataclass
from hashlib import sha256

import flamepy.app as app  # noqa: PLR0402

MODEL = "facebook/opt-125m"

app.init("vllm-das", fail_if_exists=True)


def kv_key(model: str, prompt: str) -> bytes:
    """Return an application-owned affinity key for a cached prompt."""
    return sha256(f"{model}\0{prompt}".encode()).digest()


@dataclass(frozen=True)
class GenerationResult:
    text: str
    cached_tokens: int


@app.service(resreq="gpu=1")
class VllmEngine:
    """A vLLM class service retained across App session changes."""

    def __init__(self, model: str = MODEL) -> None:
        # Keep spawned engine sockets short and avoid CUDA compiler dependencies
        # in the dedicated-host smoke example. Preserve explicit runtime settings.
        os.environ.setdefault("VLLM_RPC_BASE_PATH", "/tmp")
        os.environ.setdefault("VLLM_USE_FLASHINFER_SAMPLER", "0")
        from vllm import LLM

        self.model = model
        self.llm = LLM(
            model=model,
            enable_prefix_caching=True,
            tensor_parallel_size=1,
            gpu_memory_utilization=0.1,
            max_model_len=1024,
            enforce_eager=True,
            attention_config={"backend": "TRITON_ATTN"},
        )

    def generate(self, prompt: str) -> GenerationResult:
        from vllm import SamplingParams

        key = kv_key(self.model, prompt)
        output = self.llm.generate(prompt, SamplingParams(max_tokens=32))[0]
        app.publish_attributes({key})

        return GenerationResult(
            text=output.outputs[0].text,
            cached_tokens=output.num_cached_tokens or 0,
        )
