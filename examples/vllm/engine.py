"""Process-local vLLM engine used by the Runner DAS reuse example."""

from dataclasses import dataclass
from hashlib import sha256
from flamepy.runner import RunnerService
from vllm import LLM, SamplingParams


MODEL = "facebook/opt-125m"


def kv_key(model: str, prompt: str) -> bytes:
    """Return an application-owned affinity key for a cached prompt."""
    return sha256(f"{model}\0{prompt}".encode()).digest()


@dataclass(frozen=True)
class GenerationResult:
    text: str
    cached_tokens: int


class VllmEngine(RunnerService):
    """A vLLM class service retained across Runner session changes."""

    def __init__(self) -> None:
        self.llm = LLM(model=MODEL, enable_prefix_caching=True)

    def generate(self, prompt: str) -> GenerationResult:
        key = kv_key(MODEL, prompt)
        output = self.llm.generate(prompt, SamplingParams(max_tokens=32))[0]
        self.publish_attributes({key})

        return GenerationResult(
            text=output.outputs[0].text,
            cached_tokens=output.num_cached_tokens or 0,
        )
