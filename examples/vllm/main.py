"""Verify that DAS reuses a retained Runner process and its vLLM engine."""

import time

from flamepy import ResourceRequirement, TaskOptions
from flamepy.runner import Runner

from engine import MODEL, VllmEngine, kv_key


DELAY_RELEASE_SECONDS = 90


def main() -> None:
    prompt = (
        "Data-aware scheduling routes inference requests to workers that already "
        "hold reusable model state, reducing repeated data transfer and prefill "
        "computation while retaining normal fallback scheduling. "
    ) * 4
    with Runner("vllm-das", fail_if_exists=True) as rr:
        first_service = rr.service(
            VllmEngine,
            resreq=ResourceRequirement(gpu=1),
        )
        first = first_service.generate(prompt).get()
        print(first.text)

        # With the default 60-second delay_release, the executor unbinds after
        # waiting for more work and remains Idle for another 120 seconds.
        time.sleep(DELAY_RELEASE_SECONDS)

        second_service = rr.service(
            VllmEngine,
            resreq=ResourceRequirement(gpu=1),
        )
        second = second_service.generate(
            prompt,
            option=TaskOptions(affinity={kv_key(MODEL, prompt)}),
        ).get()
        print(second.text)

        assert second.cached_tokens > 0, "the retained prompt cache was not reused"
        print("reused the cached prompt after the executor unbound")


if __name__ == "__main__":
    main()
