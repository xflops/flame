"""Exercise the serving transport and model lifetime without a GPU."""

import importlib.util
import os
import sys
import types
import unittest
from pathlib import Path
from unittest.mock import MagicMock, patch

import client
import cloudpickle
from flamepy.core import ObjectRef
from flamepy.core.service import ApplicationContext, SessionContext, TaskContext


class FakeLLM:
    creations = 0

    def __init__(self, **kwargs):
        self.model = kwargs["model"]
        self.options = kwargs
        self.environment = dict(os.environ)
        self.cached_prompts = set()
        type(self).creations += 1

    def generate(self, prompts, parameters):
        self.parameters = parameters
        outputs = []
        for prompt in prompts:
            outputs.append(
                types.SimpleNamespace(
                    prompt=prompt,
                    outputs=[types.SimpleNamespace(text=f"{prompt} completion")],
                    num_cached_tokens=16 if prompt in self.cached_prompts else 0,
                )
            )
            self.cached_prompts.add(prompt)
        return outputs


class VllmServingTest(unittest.TestCase):
    def setUp(self):
        FakeLLM.creations = 0
        environment = patch.dict("os.environ", {
            "VLLM_MODEL": "test-model",
            "CUDA_VISIBLE_DEVICES": "GPU-assigned",
            "VLLM_RPC_BASE_PATH": "/tmp/custom",
            "VLLM_USE_FLASHINFER_SAMPLER": "1",
        })
        environment.start()
        self.addCleanup(environment.stop)
        self.sampling = MagicMock(side_effect=lambda **kwargs: kwargs)
        fake = types.SimpleNamespace(LLM=FakeLLM, SamplingParams=self.sampling)
        patcher = patch.dict(sys.modules, {"vllm": fake})
        patcher.start()
        self.addCleanup(patcher.stop)
        spec = importlib.util.spec_from_file_location(
            "vllm_worker", Path(__file__).parents[1] / "service.py"
        )
        self.service = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(self.service)
        self.worker = self.service.ins
        self.contexts = {}
        cache_patch = patch(
            "flamepy.serving.instance.get_object",
            side_effect=lambda ref: cloudpickle.dumps(self.contexts[ref.key]),
        )
        cache_patch.start()
        self.addCleanup(cache_patch.stop)

    def enter(self, session_id, config=None):
        ref = ObjectRef("grpc://cache:9090", f"vllm-serving/{session_id}/context")
        self.contexts[ref.key] = config or {"max_tokens": 32, "temperature": 0.8}
        context = SessionContext(
            ref.encode(), session_id, ApplicationContext("vllm-serving")
        )
        self.worker.on_session_enter(context)

    def invoke(self, worker, session_id, prompts):
        output = worker.on_task_invoke(
            TaskContext(
                task_id="task",
                session_id=session_id,
                input=cloudpickle.dumps({"prompts": prompts}),
            )
        )
        return cloudpickle.loads(output)

    def test_supported_attention_configuration_preserves_runtime_settings(self):
        llm = self.service.llm
        self.assertEqual(llm.options["attention_config"], {"backend": "TRITON_ATTN"})
        self.assertEqual(llm.environment["CUDA_VISIBLE_DEVICES"], "GPU-assigned")
        self.assertEqual(llm.environment["VLLM_RPC_BASE_PATH"], "/tmp/custom")
        self.assertEqual(llm.environment["VLLM_USE_FLASHINFER_SAMPLER"], "1")

    def test_transport_batches_prompts_and_retains_model_across_sessions(self):
        worker = self.worker
        self.assertEqual(FakeLLM.creations, 1)
        self.assertEqual(self.sampling.call_count, 0)
        self.enter("first", {"max_tokens": 16, "temperature": 0.2})
        self.assertEqual(FakeLLM.creations, 1)
        self.assertEqual(self.service.llm.model, "test-model")
        self.assertEqual(self.sampling.call_count, 0)
        first = self.invoke(worker, "first", ["hello", "world"])
        self.assertEqual(self.sampling.call_count, 1)
        self.assertEqual(
            self.service.llm.parameters, {"max_tokens": 16, "temperature": 0.2}
        )
        retained_engine = self.service.llm
        worker.on_session_leave()
        self.enter("second", {"max_tokens": 48, "temperature": 0.1})
        second = self.invoke(worker, "second", ["hello"])
        self.assertEqual(self.sampling.call_count, 2)

        self.assertEqual(
            [item["text"] for item in first], ["hello completion", "world completion"]
        )
        self.assertEqual(first[0]["cached_tokens"], 0)
        self.assertEqual(second[0]["cached_tokens"], 16)
        self.assertEqual(
            self.service.llm.parameters, {"max_tokens": 48, "temperature": 0.1}
        )
        self.assertIs(self.service.llm, retained_engine)
        self.assertEqual(FakeLLM.creations, 1)

    def test_invalid_prompts_do_not_reinitialize_model(self):
        self.enter("first")
        for prompts in [None, [], "hello", [""], [1]]:
            with self.subTest(prompts=prompts), self.assertRaises(ValueError):
                self.service.generate({"prompts": prompts})
        self.assertEqual(FakeLLM.creations, 1)

    def test_deployment_model_cannot_be_overridden_by_session_context(self):
        self.enter("first", {"model": "other-model", "max_tokens": 16})
        retained = self.service.llm
        self.assertEqual(retained.model, "test-model")
        self.worker.on_session_leave()
        self.enter("second", {"model": "another-model", "max_tokens": 48})
        self.assertIs(self.service.llm, retained)
        self.invoke(self.worker, "second", ["hello"])
        self.assertEqual(self.service.llm.parameters["max_tokens"], 48)
        self.assertEqual(FakeLLM.creations, 1)

    def test_client_sends_configuration_as_context_and_prompts_as_request(self):
        session = MagicMock()
        session.__enter__.return_value = session
        session.run.return_value = []
        with (
            patch.object(
                client.serving, "open_session", return_value=session
            ) as open_session,
            patch.object(
                sys,
                "argv",
                [
                    "client.py",
                    "--max-tokens",
                    "64",
                    "--temperature",
                    "0.3",
                    "--prompt",
                    "hello",
                ],
            ),
            patch("builtins.print"),
        ):
            client.main()
        open_session.assert_called_once_with(
            "vllm-serving",
            ctx={"max_tokens": 64, "temperature": 0.3},
            resreq={"gpu": 1},
        )
        session.run.assert_called_once_with({"prompts": ["hello"]})


if __name__ == "__main__":
    unittest.main()
