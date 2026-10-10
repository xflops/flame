"""Retained vLLM worker exposed through Flame serving sessions."""

import os

from flamepy import serving

ins = serving.Instance()


class VllmEngine:
    def __init__(self, model: str):
        from vllm import LLM

        self.model = model
        self.llm = LLM(
            model=self.model,
            enable_prefix_caching=True,
            tensor_parallel_size=1,
            gpu_memory_utilization=0.1,
            max_model_len=1024,
            enforce_eager=True,
        )

    def generate(self, request: dict, context: dict) -> list[dict]:
        from vllm import SamplingParams

        prompts = request.get("prompts")
        if (
            not isinstance(prompts, list)
            or not prompts
            or any(not isinstance(prompt, str) or not prompt for prompt in prompts)
        ):
            raise ValueError("prompts must be a nonempty list of nonempty strings")

        context = context or {}
        parameters = SamplingParams(
            max_tokens=context.get("max_tokens", 32),
            temperature=context.get("temperature", 0.8),
        )
        outputs = self.llm.generate(prompts, parameters)
        return [
            {
                "prompt": output.prompt,
                "text": output.outputs[0].text,
                "cached_tokens": output.num_cached_tokens or 0,
            }
            for output in outputs
        ]


engine = VllmEngine(os.getenv("VLLM_MODEL", "facebook/opt-125m"))


@ins.entrypoint
def generate(request: dict) -> list[dict]:
    return engine.generate(request, ins.context())


if __name__ == "__main__":
    ins.run()
