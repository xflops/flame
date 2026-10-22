"""Trainer service: data-parallel GRPO, one GPU per executor.

Every executor holds a full replica (fp32 weights, optimizer state, frozen reference).
Replicas may sit on different nodes; they join one NCCL group and all-reduce gradients,
so each takes a slice of the batch and all stay identical after every step. See
`join` for how the group forms without addressing executors individually.
"""

import socket
import time
import uuid
from datetime import timedelta
from typing import Optional

import delta
from runtime import CHECKPOINT_PREFIX_APP, TRAINERS, app

SHARD_BYTES = 512 * 1024 * 1024


@app.service(resreq="cpu=16,mem=128g,gpu=1", batch_size=TRAINERS)
class Trainer:
    def __init__(self, model: str, lr: float = 1e-6, kl_coef: float = 0.001, micro_tokens: int = 16384, run: str = "run"):
        import torch
        from transformers import AutoModelForCausalLM

        self._torch = torch
        self._run = run
        self._kl_coef = kl_coef
        self._micro_tokens = micro_tokens
        # fp32 master weights: bf16 cannot hold learning-rate-sized updates.
        self.policy = AutoModelForCausalLM.from_pretrained(model, torch_dtype=torch.float32).cuda()
        self.policy.gradient_checkpointing_enable()
        self.policy.config.use_cache = False
        self.reference = AutoModelForCausalLM.from_pretrained(model, torch_dtype=torch.bfloat16).cuda().eval()
        for parameter in self.reference.parameters():
            parameter.requires_grad_(False)
        self.optimizer = torch.optim.AdamW(self.policy.parameters(), lr=lr, weight_decay=0.0)
        self._tied = bool(getattr(self.policy.config, "tie_word_embeddings", False))
        self.id = uuid.uuid4().hex
        self.rank, self.world = 0, 1
        # What the last export published: the bf16 weights, their shard layout and objects.
        self._previous = self._layout = self._refs = self._last = self._prefix = None

    def _logprobs(self, model, batch):
        torch = self._torch
        ids, attention = batch["ids"], batch["attention"]
        with torch.autocast("cuda", dtype=torch.bfloat16):
            logits = model(input_ids=ids, attention_mask=attention).logits[:, :-1]
        targets = ids[:, 1:]
        return -torch.nn.functional.cross_entropy(logits.float().transpose(1, 2), targets, reduction="none")

    def _batches(self, samples):
        torch = self._torch
        samples = sorted(samples, key=lambda s: len(s["ids"]))
        batch, longest = [], 0
        for sample in samples + [None]:
            if sample is not None and (len(batch) + 1) * max(longest, len(sample["ids"])) <= self._micro_tokens:
                batch.append(sample)
                longest = max(longest, len(sample["ids"]))
                continue
            if batch:
                width = longest
                ids = torch.zeros(len(batch), width, dtype=torch.long)
                attention = torch.zeros(len(batch), width, dtype=torch.long)
                loss_mask = torch.zeros(len(batch), width, dtype=torch.float32)
                advantage = torch.zeros(len(batch), 1, dtype=torch.float32)
                for row, s in enumerate(batch):
                    length = len(s["ids"])
                    ids[row, :length] = torch.tensor(s["ids"])
                    attention[row, :length] = 1
                    loss_mask[row, s["prompt_len"] : length] = torch.tensor(s["mask"], dtype=torch.float32)
                    advantage[row, 0] = s["advantage"]
                yield {
                    "ids": ids.cuda(),
                    "attention": attention.cuda(),
                    "mask": loss_mask[:, 1:].cuda(),  # aligned with next-token targets
                    "advantage": advantage.cuda(),
                }
            batch, longest = ([sample], len(sample["ids"])) if sample is not None else ([], 0)

    def join(self, members: dict, count: int, port: int) -> dict:
        """Form the NCCL group of `count` executors. The driver sends this call `count` times at once.

        The service is declared with `batch_size=N` and no warmup: while calls wait, the gang
        binds all N executors or none, so every one of the N calls gets an executor and none
        waits for a member that never starts. Idle executors are shared with the rollout
        service in the same app.

        Each call blocks until every member has joined, so no executor takes two of them and
        calls need no per-executor addressing. Members register in a shared cache object;
        rank is the position of the executor id in the sorted list, and rank 0 is the rendezvous.
        """
        import json

        from flamepy.core import ObjectRef, get_object, patch_object

        torch = self._torch
        probe = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        try:
            probe.connect(("10.255.255.255", 1))  # no packet is sent; picks the outbound address
            host = probe.getsockname()[0]
        finally:
            probe.close()

        ref = ObjectRef(**members)
        patch_object(ref, json.dumps({"id": self.id, "host": host}).encode())
        # Version 0 bypasses the client cache, so every poll sees the latest registrations.
        latest = ObjectRef(ref.endpoint, ref.key, 0)
        deadline = time.time() + 600
        while True:
            table = get_object(latest, lambda parts: {e["id"]: e["host"] for e in map(json.loads, filter(None, parts))})
            if len(table) >= count:
                break
            if time.time() > deadline:
                raise RuntimeError(f"only {len(table)} of {count} trainer executors joined")
            time.sleep(0.5)

        ids = sorted(table)
        self.rank, self.world = ids.index(self.id), len(ids)
        if self.world > 1 and not torch.distributed.is_initialized():
            torch.distributed.init_process_group(
                "nccl",
                init_method=f"tcp://{table[ids[0]]}:{port}",
                rank=self.rank,
                world_size=self.world,
                timeout=timedelta(minutes=10),
                device_id=torch.device("cuda", 0),
            )
        return {"rank": self.rank, "world": self.world, "host": host}

    def update(self, samples: list[dict], total_tokens: int) -> dict:
        """One policy-gradient step. Every replica gets the whole batch and trains on its slice."""
        torch = self._torch
        started = time.time()
        # Longest-first striping keeps replicas balanced.
        mine = sorted(samples, key=lambda s: -len(s["ids"]))[self.rank :: self.world]
        loss_sum, kl_sum = 0.0, 0.0
        self.policy.train()
        self.optimizer.zero_grad(set_to_none=True)
        for batch in self._batches(mine):
            logp = self._logprobs(self.policy, batch)
            with torch.no_grad():
                ref = self._logprobs(self.reference, batch)
            delta = ref - logp
            kl = torch.exp(delta) - delta - 1  # k3 estimator
            per_token = -batch["advantage"] * logp + self._kl_coef * kl
            loss = (per_token * batch["mask"]).sum() / max(1, total_tokens)
            loss.backward()
            loss_sum += float(loss)
            kl_sum += float((kl * batch["mask"]).sum() / max(1, total_tokens))
        if self.world > 1:
            # Replicas must reach this point together even when a slice is empty.
            for parameter in self.policy.parameters():
                if parameter.grad is None:
                    parameter.grad = torch.zeros_like(parameter)
                torch.distributed.all_reduce(parameter.grad)
            totals = torch.tensor([loss_sum, kl_sum], device="cuda")
            torch.distributed.all_reduce(totals)
            loss_sum, kl_sum = totals.tolist()
        grad_norm = float(torch.nn.utils.clip_grad_norm_(self.policy.parameters(), 1.0))
        self.optimizer.step()
        return {"loss": loss_sum, "kl": kl_sum, "grad_norm": grad_norm, "seconds": time.time() - started}

    def export(self, version: int) -> Optional[dict]:
        """Publish the current weights to the object cache as bf16 safetensors, as a delta.

        The weights live in shards of about SHARD_BYTES, one cache object each. The first
        export stores every tensor; each later export appends one part per shard holding only
        the elements whose bf16 value changed (a learning rate this small leaves most of them
        as they were), so readers download the change, not the model. A part is a safetensors
        blob: `name` holds a whole tensor, or `name.idx` and `name.val` the flat indices and
        new values of the changed elements.

        The replicas are identical, so only rank 0 publishes; the driver calls every replica
        and takes the one answer. Objects sit under a placeholder application so a checkpoint
        survives `app.destroy()`. Returns a plain dict: a returned ObjectRef would be taken as
        the task's result, and an ObjectRef argument is resolved on every call, whereas rollout
        workers read each version once (see SearchEngine.load_weights).
        """
        if self.rank != 0:
            return None
        if self._last is not None and self._last[0] == version:
            return self._last[1]
        from flamepy.core import patch_object, put_object

        torch = self._torch
        started = time.time()
        current = {k: v.detach().to(torch.bfloat16) for k, v in self.policy.state_dict().items()}
        if self._tied:
            current.pop("lm_head.weight", None)
        first = self._previous is None
        if first:
            self._layout = self._shard_names(current)
            self._prefix = f"{CHECKPOINT_PREFIX_APP}/{self._run}"
        payloads, changed = [], 0
        for names in self._layout:
            payload, count = delta.encode(names, current, self._previous)
            payloads.append(payload)
            changed += count
        encoded = time.time()
        if first:
            self._refs = [put_object(self._prefix, payload) for payload in payloads]
        else:
            self._refs = [patch_object(ref, payload) for ref, payload in zip(self._refs, payloads)]
        self._previous = current
        total = sum(t.numel() for t in current.values())
        result = {
            "prefix": self._prefix,
            "parts": self._refs[0].version,
            "shards": [{"endpoint": ref.endpoint, "key": ref.key, "version": ref.version} for ref in self._refs],
            # Where the export went: GPU diff, copy and serialization versus the cache upload.
            "timings": {"encode_s": encoded - started, "put_s": time.time() - encoded, "bytes": sum(map(len, payloads)), "changed": changed / total},
        }
        self._last = (version, result)
        return result

    def _shard_names(self, state: dict) -> list[list[str]]:
        """Group tensor names into shards of about SHARD_BYTES; fixed for the whole run."""
        shards, current, size = [], [], 0
        for name, tensor in state.items():
            if current and size + tensor.numel() * 2 > SHARD_BYTES:
                shards.append(current)
                current, size = [], 0
            current.append(name)
            size += tensor.numel() * 2
        if current:
            shards.append(current)
        return shards
