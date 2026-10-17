"""A vLLM engine that runs the Search-R1 loop: think, search, read, answer.

Used by the rollout service of the training run.
"""

import os
import re
import tempfile
from dataclasses import dataclass, field
from typing import Any, Callable

from data import PROMPT, exact_match, last_answer

STOP = ["</search>", "</answer>"]


@dataclass
class _State:
    item: dict
    ids: list[int]
    prompt_len: int
    mask: list[int] = field(default_factory=list)  # 1 for model tokens, per response token
    text: str = ""
    turns: int = 0
    searches: int = 0
    done: bool = False


class SearchEngine:
    def __init__(self, model: str, search: Callable[[list[str], int], list[list[str]]], gpu_memory_utilization: float = 0.4, max_model_len: int = 4096):
        # Keep sockets short, avoid CUDA compiler needs, and keep the model in this
        # process so weights can be updated in place.
        os.environ.setdefault("VLLM_RPC_BASE_PATH", "/tmp")
        os.environ.setdefault("VLLM_USE_FLASHINFER_SAMPLER", "0")
        os.environ["VLLM_ENABLE_V1_MULTIPROCESSING"] = "0"
        from vllm import LLM

        self.llm = LLM(
            model=model,
            gpu_memory_utilization=gpu_memory_utilization,
            max_model_len=max_model_len,
            enforce_eager=True,
            enable_prefix_caching=True,
            attention_config={"backend": "TRITON_ATTN"},
        )
        self.tokenizer = self.llm.get_tokenizer()
        self.search = search
        self.max_model_len = max_model_len
        self.version = -1
        # The weights as published (bf16, on the CPU) and how many parts of each shard are in them.
        self._state, self._applied, self._changed = {}, {}, set()

    def load_weights(self, ref: dict, version: int) -> dict:
        """Bring the engine to a published weights version, once per version.

        The weights are cache objects, one per shard: a first part with every tensor and a
        part per export after it with the elements that changed (see Trainer.export). The
        client cache keeps the parts it has read, so a get downloads only the new ones; this
        executor applies them to a CPU copy of the weights and loads the changed tensors into
        the engine.
        """
        if version == self.version:
            return {}
        import time

        from flamepy.core import ObjectRef, get_object

        self._changed = set()
        fetching = 0.0
        for shard in ref["shards"]:
            mark = time.time()
            get_object(ObjectRef(**shard), lambda parts, key=shard["key"]: self._apply_parts(key, parts, ref["parts"]))
            fetching += time.time() - mark
        mark = time.time()
        changed = [(name, self._state[name]) for name in self._changed]
        self.llm.apply_model(lambda model: model.load_weights(iter(changed)))
        self.llm.reset_prefix_cache()
        self.version = version
        # Cache download and applying the deltas to the CPU copy, versus loading into the engine.
        return {"get_s": fetching, "load_s": time.time() - mark}

    def _apply_parts(self, key: str, parts: list, count: int) -> None:
        """Apply the parts of a shard object this executor has not applied yet, up to `count`."""
        import delta

        applied = self._applied.get(key, 0)
        for part in parts[applied:count]:
            self._changed |= delta.apply(self._state, part)
        self._applied[key] = max(applied, count)

    def _prompt_ids(self, question: str) -> list[int]:
        messages = [{"role": "user", "content": PROMPT.format(question=question.strip())}]
        text = self.tokenizer.apply_chat_template(messages, add_generation_prompt=True, tokenize=False)
        return self.tokenizer.encode(text, add_special_tokens=False)

    def _information(self, passages: list[str]) -> str:
        docs = "\n".join(f"Doc {i + 1}: {' '.join(p.split()[:120])}" for i, p in enumerate(passages))
        return f"\n\n<information>{docs}</information>\n\n"

    def run(
        self,
        items: list[dict],
        n: int = 1,
        max_turns: int = 4,
        temperature: float = 1.0,
        top_k: int = 3,
        max_tokens: int = 512,
    ) -> list[dict]:
        """Run the loop for each item n times; return one trajectory per sample.

        An item is {"id", "question", "answers"?}. A trajectory holds the token ids of
        prompt and response, a mask marking the model's own tokens, the final answer,
        and the exact-match reward when the item has gold answers.
        """
        from vllm import SamplingParams

        states = []
        for item in items:
            ids = self._prompt_ids(item["question"])
            states.extend(_State(item=item, ids=list(ids), prompt_len=len(ids)) for _ in range(n))

        for turn in range(max_turns + 1):
            active = [s for s in states if not s.done]
            if not active:
                break
            params = SamplingParams(temperature=temperature, max_tokens=max_tokens, stop=STOP, include_stop_str_in_output=True)
            outputs = self.llm.generate([{"prompt_token_ids": s.ids} for s in active], params, use_tqdm=False)
            queries: list[tuple[_State, str]] = []
            for state, output in zip(active, outputs):
                completion = output.outputs[0]
                state.ids.extend(completion.token_ids)
                state.mask.extend([1] * len(completion.token_ids))
                state.text += completion.text
                state.turns += 1
                match = re.search(r"<search>(.*?)</search>\s*$", completion.text, re.DOTALL)
                if match and turn < max_turns:
                    queries.append((state, match.group(1).strip()))
                else:
                    state.done = True
            if queries:
                results = self.search([q for _, q in queries], top_k)
                for (state, _), passages in zip(queries, results):
                    info = self.tokenizer.encode(self._information(passages), add_special_tokens=False)
                    state.searches += 1
                    state.text += self._information(passages)
                    if len(state.ids) + len(info) + 64 >= self.max_model_len:
                        state.done = True
                        continue
                    state.ids.extend(info)
                    state.mask.extend([0] * len(info))

        return [self._trajectory(s) for s in states]

    def _trajectory(self, state: _State) -> dict:
        answer = last_answer(state.text)
        golds = state.item.get("answers")
        return {
            "prompt_id": state.item["id"],
            "ids": state.ids,
            "prompt_len": state.prompt_len,
            "mask": state.mask,
            "text": state.text,
            "answer": answer,
            "turns": state.turns,
            "searches": state.searches,
            "reward": float(exact_match(answer, golds)) if golds else None,
        }
