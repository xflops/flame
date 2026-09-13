"""Stateless Runner vLLM service with instance-attribute publication."""

from hashlib import sha256

from flamepy import ResourceRequirement
from flamepy.runner import Runner, RunnerService
from vllm import LLM, SamplingParams


def kv_key(model: str, prompt: str) -> bytes:
    """Application-owned stable key; production code should hash token blocks."""
    return sha256(f"{model}\0{prompt}".encode()).digest()


class VllmEngine(RunnerService):
    def __init__(self) -> None:
        self.model = "facebook/opt-125m"
        self.llm = LLM(model=self.model)
        self.cached_keys: set[bytes] = set()

    def generate(self, prompt: str) -> str:
        key = kv_key(self.model, prompt)
        output = self.llm.generate(prompt, SamplingParams(max_tokens=32))[0]
        # This small example assumes no KV eviction. Production code should
        # publish the engine's complete current cache index on every response.
        self.cached_keys.add(key)
        self.publish_attributes(self.cached_keys)
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
        print(service.generate(prompt).get())


if __name__ == "__main__":
    main()
