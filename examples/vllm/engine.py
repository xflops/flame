"""Process-local vLLM engine used by the App DAS reuse example."""

from dataclasses import dataclass
from hashlib import sha256

import flamepy.app as app  # noqa: PLR0402
from vllm import LLM, SamplingParams

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
        self.model = model
        self.llm = LLM(model=model, enable_prefix_caching=True)

    def generate(self, prompt: str) -> GenerationResult:
        key = kv_key(self.model, prompt)
        output = self.llm.generate(prompt, SamplingParams(max_tokens=32))[0]
        app.publish_attributes({key})

        return GenerationResult(
            text=output.outputs[0].text,
            cached_tokens=output.num_cached_tokens or 0,
        )
