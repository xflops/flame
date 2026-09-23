"""Verify that DAS reuses a retained App process and its vLLM engine."""

import time

import flamepy.app as app  # noqa: PLR0402
from engine import MODEL, VllmEngine, kv_key
from flamepy import TaskOptions

# Repeated same-name initialization is a locked, idempotent no-op.
app.init("vllm-das")

DELAY_RELEASE_SECONDS = 90


def main() -> None:
    prompt = (
        "Data-aware scheduling routes inference requests to workers that already "
        "hold reusable model state, reducing repeated data transfer and prefill "
        "computation while retaining normal fallback scheduling. "
    ) * 4
    try:
        engine = VllmEngine(MODEL)
        first = engine.generate(prompt).get()
        print(first.text)

        # With the default 60-second delay_release, the executor unbinds after
        # waiting for more work and remains Idle for another 120 seconds.
        time.sleep(DELAY_RELEASE_SECONDS)

        second = engine.generate(
            prompt,
            option=TaskOptions(affinity={kv_key(MODEL, prompt)}),
        ).get()
        print(second.text)

        assert second.cached_tokens > 0, "the retained prompt cache was not reused"
        print("reused the cached prompt after the executor unbound")
    finally:
        app.destroy()


if __name__ == "__main__":
    main()
