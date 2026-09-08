"""Stateless Runner vLLM service with KV-cache-aware task routing."""

from hashlib import sha256

from flamepy import ResourceRequirement, TaskOptions
from flamepy.runner import Runner
from vllm import LLM, SamplingParams


def kv_key(model: str, prompt: str) -> bytes:
    """Application-owned stable key; production code should hash token blocks."""
    return sha256(f"{model}\0{prompt}".encode()).digest()


class VllmEngine:
    def __init__(self) -> None:
        self.model = "facebook/opt-125m"
        self.llm = LLM(model=self.model)

    def generate(self, prompt: str) -> str:
        key = kv_key(self.model, prompt)
        output = self.llm.generate(prompt, SamplingParams(max_tokens=32))[0]
        # Publish only opaque runtime keys; never persist vLLM's cache object.
        self._flame_session_context.publish({key})
        return output.outputs[0].text


def main() -> None:
    prompt = "Explain data-aware scheduling in one sentence."
    with Runner("vllm-das") as rr:
        service = rr.service(
            VllmEngine,
            warmup=1,  # Start one replica so it can build a warm KV cache.
            resreq=ResourceRequirement(gpu=1),  # vLLM replica GPU requirement.
        )
        print(service.generate(prompt).get())
        key = kv_key("facebook/opt-125m", prompt)
        print(service.generate(prompt, option=TaskOptions(affinity={key})).get())


if __name__ == "__main__":
    main()
