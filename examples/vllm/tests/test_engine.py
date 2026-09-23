"""Unit tests for the vLLM DAS example without loading a real model."""

import sys
import types
import unittest
from unittest.mock import patch

from flamepy.app._context import _bind_invocation_context
from flamepy.app.runpy import FlameRunpyService
from flamepy.app.types import ServiceContext
from flamepy.core.service import ApplicationContext, SessionContext


class FakeLLM:
    creations = 0

    def __init__(self, *, model: str, enable_prefix_caching: bool) -> None:
        self.model = model
        self.enable_prefix_caching = enable_prefix_caching
        self.cached_prompts = set()
        type(self).creations += 1

    def generate(self, prompt, sampling_params):
        del sampling_params
        cached_tokens = len(prompt.split()) if prompt in self.cached_prompts else 0
        self.cached_prompts.add(prompt)
        return [
            types.SimpleNamespace(
                outputs=[types.SimpleNamespace(text=prompt)],
                num_cached_tokens=cached_tokens,
            )
        ]


class FakeSamplingParams:
    def __init__(self, *, max_tokens: int) -> None:
        self.max_tokens = max_tokens


fake_vllm = types.SimpleNamespace(LLM=FakeLLM, SamplingParams=FakeSamplingParams)
with (
    patch.dict(sys.modules, {"vllm": fake_vllm}),
    patch("flamepy.app.init"),
    patch(
        "flamepy.app.service", return_value=lambda execution_object: execution_object
    ),
):
    from engine import MODEL, VllmEngine, kv_key


class VllmEngineTest(unittest.TestCase):
    def setUp(self) -> None:
        FakeLLM.creations = 0

    def test_service_publishes_prompt_keys(self) -> None:
        service = FlameRunpyService()
        service._set_execution_from_context(
            ServiceContext(VllmEngine, constructor_args=(MODEL,))
        )
        execution_object = service._execution_object
        session = SessionContext(None, "session", ApplicationContext("vllm-das"))
        with _bind_invocation_context(session) as invocation_context:
            first = execution_object.generate("first")
            second = execution_object.generate("second")
        service.publish(invocation_context.attributes)

        self.assertEqual(FakeLLM.creations, 1)
        self.assertEqual(first.cached_tokens, 0)
        self.assertEqual(second.cached_tokens, 0)
        self.assertEqual(
            set(service._take_attributes().attr),
            {kv_key(MODEL, "first"), kv_key(MODEL, "second")},
        )

    def test_rebinding_reuses_cached_prompt(self) -> None:
        service = FlameRunpyService()
        context = ServiceContext(VllmEngine, constructor_args=(MODEL,))
        service._set_execution_from_context(context)
        first_execution_object = service._execution_object
        session = SessionContext(None, "session", ApplicationContext("vllm-das"))
        with _bind_invocation_context(session) as invocation_context:
            first = first_execution_object.generate("same prompt")
        service.publish(invocation_context.attributes)
        service._take_attributes()

        service.on_session_leave()
        service._set_execution_from_context(context)
        second_execution_object = service._execution_object
        with _bind_invocation_context(session) as invocation_context:
            second = second_execution_object.generate("same prompt")
        service.publish(invocation_context.attributes)

        self.assertEqual(first.cached_tokens, 0)
        self.assertGreater(second.cached_tokens, 0)
        self.assertEqual(FakeLLM.creations, 1)
        self.assertIs(second_execution_object, first_execution_object)
        self.assertEqual(
            set(service._take_attributes().attr),
            {kv_key(MODEL, "same prompt")},
        )


if __name__ == "__main__":
    unittest.main()
