"""Unit tests for the vLLM DAS example without loading a real model."""

import asyncio
import os
import sys
import types
import unittest
from unittest.mock import patch

from flamepy.app._context import _bind_invocation_context, _stage_response_attributes
from flamepy.app.runpy import FlameRunpyService
from flamepy.app.types import ServiceContext
from flamepy.core.service import ApplicationContext, SessionContext


class FakeLLM:
    creations = 0

    def __init__(self, *, model: str, enable_prefix_caching: bool, **kwargs) -> None:
        self.model = model
        self.options = kwargs
        self.environment = dict(os.environ)
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
    patch("flamepy.app.service", return_value=lambda execution_object: execution_object),
):
    from engine import MODEL, VllmEngine, kv_key


class VllmEngineTest(unittest.TestCase):
    def setUp(self) -> None:
        FakeLLM.creations = 0
        vllm_patch = patch.dict(sys.modules, {"vllm": fake_vllm})
        vllm_patch.start()
        self.addCleanup(vllm_patch.stop)

    def test_compiler_free_defaults_preserve_gpu_assignment(self) -> None:
        with patch.dict(os.environ, {"CUDA_VISIBLE_DEVICES": "GPU-assigned"}, clear=True):
            engine = VllmEngine(MODEL)
        self.assertEqual(engine.llm.options["attention_config"], {"backend": "TRITON_ATTN"})
        self.assertEqual(engine.llm.environment["VLLM_RPC_BASE_PATH"], "/tmp")
        self.assertEqual(engine.llm.environment["VLLM_USE_FLASHINFER_SAMPLER"], "0")
        self.assertEqual(engine.llm.environment["CUDA_VISIBLE_DEVICES"], "GPU-assigned")

    def test_explicit_runtime_settings_are_preserved(self) -> None:
        environment = {"VLLM_RPC_BASE_PATH": "/tmp/custom", "VLLM_USE_FLASHINFER_SAMPLER": "1"}
        with patch.dict(os.environ, environment):
            engine = VllmEngine(MODEL)
        for key, value in environment.items():
            self.assertEqual(engine.llm.environment[key], value)

    def test_service_publishes_prompt_keys(self) -> None:
        service = FlameRunpyService()
        service._set_execution_from_context(ServiceContext(VllmEngine, constructor_args=(MODEL,)))
        execution_object = service._execution_object
        session = SessionContext(None, "session", ApplicationContext("vllm-das"))
        with _bind_invocation_context(session) as invocation_context:
            first = execution_object.generate("first")
            second = execution_object.generate("second")
        _stage_response_attributes(invocation_context.attributes)

        self.assertEqual(FakeLLM.creations, 1)
        self.assertEqual(first.cached_tokens, 0)
        self.assertEqual(second.cached_tokens, 0)
        attributes = service._take_attributes()
        self.assertEqual(set(attributes.publish), {kv_key(MODEL, "first"), kv_key(MODEL, "second")})
        self.assertEqual(list(attributes.retract), [])

    def test_rebinding_reuses_cached_prompt(self) -> None:
        service = FlameRunpyService()
        context = ServiceContext(VllmEngine, constructor_args=(MODEL,))
        service._set_execution_from_context(context)
        first_execution_object = service._execution_object
        session = SessionContext(None, "session", ApplicationContext("vllm-das"))
        with _bind_invocation_context(session) as invocation_context:
            first = first_execution_object.generate("same prompt")
        _stage_response_attributes(invocation_context.attributes)
        service._take_attributes()

        asyncio.run(service.on_session_leave())
        service._set_execution_from_context(context)
        second_execution_object = service._execution_object
        with _bind_invocation_context(session) as invocation_context:
            second = second_execution_object.generate("same prompt")
        _stage_response_attributes(invocation_context.attributes)

        self.assertEqual(first.cached_tokens, 0)
        self.assertGreater(second.cached_tokens, 0)
        self.assertEqual(FakeLLM.creations, 1)
        self.assertIs(second_execution_object, first_execution_object)
        attributes = service._take_attributes()
        self.assertEqual(set(attributes.publish), {kv_key(MODEL, "same prompt")})
        self.assertEqual(list(attributes.retract), [])


if __name__ == "__main__":
    unittest.main()
