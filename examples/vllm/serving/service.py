"""Retained vLLM worker exposed through Flame serving sessions."""

import os

from flamepy import serving

# Keep engine sockets short and avoid CUDA compiler dependencies in this example.
# Preserve explicit runtime settings and FEM's GPU assignment.
os.environ.setdefault("VLLM_RPC_BASE_PATH", "/tmp")
os.environ.setdefault("VLLM_USE_FLASHINFER_SAMPLER", "0")
from vllm import LLM, SamplingParams  # noqa: E402

ins = serving.Instance()
llm = LLM(
    model=os.getenv("VLLM_MODEL", "facebook/opt-125m"),
    enable_prefix_caching=True,
    tensor_parallel_size=1,
    gpu_memory_utilization=0.1,
    max_model_len=1024,
    enforce_eager=True,
    attention_config={"backend": "TRITON_ATTN"},
)


@ins.entrypoint
def generate(request: dict) -> list[dict]:
    prompts = request.get("prompts")
    if (
        not isinstance(prompts, list)
        or not prompts
        or any(not isinstance(prompt, str) or not prompt for prompt in prompts)
    ):
        raise ValueError("prompts must be a nonempty list of nonempty strings")

    context = ins.context() or {}
    parameters = SamplingParams(
        max_tokens=context.get("max_tokens", 32),
        temperature=context.get("temperature", 0.8),
    )
    outputs = llm.generate(prompts, parameters)
    return [
        {
            "prompt": output.prompt,
            "text": output.outputs[0].text,
            "cached_tokens": output.num_cached_tokens or 0,
        }
        for output in outputs
    ]


if __name__ == "__main__":
    ins.run()
