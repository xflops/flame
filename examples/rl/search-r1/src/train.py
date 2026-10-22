"""Search-R1 style RL training driver. Retrieval, rollout and learning run as Flame services.

    python3 src/train.py --model Qwen/Qwen2.5-3B-Instruct --steps 200 --rollouts 8

The driver holds no GPU or model; it samples prompts, fans rollouts out to executors,
hands the trajectories to the trainer, and publishes new weights to the rollout workers
through the Flame object cache.
"""

import argparse
import json
import os
import random
import time
import uuid
from pathlib import Path

from data import load_hotpotqa
from grpo import advantages


def parse_args():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("--model", default="Qwen/Qwen2.5-3B-Instruct")
    p.add_argument("--steps", type=int, default=200)
    p.add_argument("--prompts-per-step", type=int, default=64)
    p.add_argument("--group-size", type=int, default=5, help="samples per prompt")
    p.add_argument("--rollouts", type=int, default=2, help="rollout executors started up front (one GPU each); more are added on demand")
    p.add_argument("--prompts-per-task", type=int, default=4, help="prompts in one rollout call; the call count drives autoscaling")
    p.add_argument("--trainers", type=int, default=1, help="data-parallel trainer executors (one GPU each, any nodes)")
    p.add_argument("--max-turns", type=int, default=4)
    p.add_argument("--lr", type=float, default=1e-6)
    p.add_argument("--kl", type=float, default=0.001)
    p.add_argument("--train-questions", type=int, default=8000)
    p.add_argument("--dev-questions", type=int, default=500)
    p.add_argument("--eval-every", type=int, default=20)
    p.add_argument("--eval-size", type=int, default=200)
    p.add_argument("--sync-every", type=int, default=1, help="publish weights to rollout workers every k steps")
    p.add_argument("--ckpt-every", type=int, default=20)
    p.add_argument("--out", default="sr1-out")
    p.add_argument("--seed", type=int, default=0)
    return p.parse_args()


def gather(futures, retry):
    """Results of futures; a failed call is retried once through `retry(i)`, then dropped."""
    results, dropped = [], 0
    for i, future in enumerate(futures):
        for attempt in (future, None):
            try:
                results.append((attempt or retry(i)).get())
                break
            except Exception as error:  # a lost executor must not stop the run
                print(f"rollout call {i} failed: {error}")
        else:
            dropped += 1
    return results, dropped


def form_trainer_group(trainer, count):
    """Join `count` trainer executors into one NCCL group; return how many nodes they span.

    The call is sent `count` times at once and blocks until all have joined (see Trainer.join).
    Gang scheduling (`batch_size` is `count`) makes sure every call gets an executor.
    """
    import flamepy.app as app
    from flamepy.core import ObjectKey, put_object
    from runtime import NAME

    members = put_object(ObjectKey.for_shared(NAME).to_prefix(), b"")
    members = {"endpoint": members.endpoint, "key": members.key, "version": members.version}
    port = random.randint(20000, 40000)  # one rendezvous port: every member must dial the same one
    joined = app.get([trainer.join(members, count, port) for _ in range(count)])
    return len({entry["host"] for entry in joined})


def publish(trainer, version, count):
    """Publish the weights as `version`. The replicas are identical and rank 0 publishes, so the
    export goes to every replica and one answer is real (see Trainer.export)."""
    import flamepy.app as app

    for _ in range(3):
        published = [result for result in app.get([trainer.export(version) for _ in range(count)]) if result]
        if published:
            return published[0]
    raise RuntimeError("no trainer replica published the weights")


def chunks(items, size):
    return [items[i : i + size] for i in range(0, len(items), size)]


def main():
    args = parse_args()
    # The services are declared with @app.service at import time, so choose the
    # application and the executor count first.
    os.environ["SR1_APP"] = "sr1-train"
    os.environ["SR1_ROLLOUTS"] = str(args.rollouts)
    os.environ["SR1_TRAINERS"] = str(args.trainers)
    from rollout import Rollout
    from runtime import CHECKPOINT_PREFIX_APP, app
    from trainer import Trainer

    out = Path(args.out)
    out.mkdir(parents=True, exist_ok=True)
    run = uuid.uuid4().hex[:8]
    rng = random.Random(args.seed)

    train, dev, corpus = load_hotpotqa(args.train_questions, args.dev_questions, args.seed)
    dev = dev[: args.eval_size]
    print(f"run {run}: {len(train)} train / {len(dev)} dev questions, {len(corpus)} passages")

    try:
        trainer = Trainer.remote(args.model, args.lr, args.kl, run=run)
        rollout = Rollout.remote(args.model, corpus)

        nodes = form_trainer_group(trainer, args.trainers) if args.trainers > 1 else 1
        print(f"{args.trainers} trainer executor(s) on {nodes} node(s), {args.rollouts}+ rollout executor(s) (autoscaled)")

        # Checkpoints live under a placeholder application so they outlive `app.destroy()`.
        import flamepy

        try:
            registered = flamepy.get_application(CHECKPOINT_PREFIX_APP) is not None
        except Exception:
            registered = False
        if not registered:
            flamepy.register_application(CHECKPOINT_PREFIX_APP, flamepy.ApplicationAttributes(description="Search-R1 checkpoints: holds cache objects, never runs"))

        weights, version = None, 0
        log = (out / "metrics.jsonl").open("a")

        def evaluate():
            calls = lambda i: rollout.generate(parts[i], weights, version, 1, args.max_turns, 0.0)
            parts = chunks(dev, args.prompts_per_task)
            results, _ = gather([calls(i) for i in range(len(parts))], calls)
            rewards = [t["reward"] for part in results for t in part]
            return sum(rewards) / max(1, len(rewards))

        def record(step, **fields):
            line = {"step": step, "version": version, **fields}
            log.write(json.dumps(line) + "\n")
            log.flush()
            print(json.dumps({k: round(v, 4) if isinstance(v, float) else v for k, v in line.items()}))

        record(0, eval_em=evaluate(), note="base model")
        for step in range(1, args.steps + 1):
            started = time.time()
            batch = rng.sample(train, args.prompts_per_step)
            parts = chunks(batch, args.prompts_per_task)
            call = lambda i: rollout.generate(parts[i], weights, version, args.group_size, args.max_turns, 1.0)
            rollout_started = time.time()
            results, dropped = gather([call(i) for i in range(len(parts))], call)
            rollout_seconds = time.time() - rollout_started
            calls = [part[0] for part in results if part]
            if sum(len(r) for r in results) < 0.9 * len(batch) * args.group_size:
                print(f"step {step}: too few trajectories ({dropped} calls dropped); skipping")
                continue
            samples, metrics = advantages([t for part in results for t in part])
            total_tokens = sum(sum(s["mask"]) for s in samples)
            # Every trainer executor gets the whole batch and trains on its own slice.
            # Task input is limited to 128 KB, so the batch goes through the cache; an
            # ObjectRef argument is resolved to the object before the call runs.
            batch_ref = app.put(samples)
            updates = app.get([trainer.update(batch_ref, total_tokens) for _ in range(args.trainers)])
            metrics = {**metrics, **updates[0]}
            update_started_total = time.time() - rollout_started - rollout_seconds
            export_seconds = 0.0
            export_timings = {}
            if step % args.sync_every == 0:
                export_started = time.time()
                weights = publish(trainer, step, args.trainers)
                version = step
                export_seconds = time.time() - export_started
                export_timings = {f"export_{k}": v for k, v in weights["timings"].items()}
            fields = {
                **metrics,
                "dropped": dropped,
                "step_seconds": time.time() - started,
                # Where the step went, as seen by the driver and by the rollout workers.
                "rollout_seconds": rollout_seconds,
                "update_call_seconds": update_started_total,
                "export_seconds": export_seconds,
                **export_timings,
                "weights_get_max_s": max((t["weights_get_s"] for t in calls), default=0.0),
                "weights_load_max_s": max((t["weights_load_s"] for t in calls), default=0.0),
                "generate_max_s": max((t["generate_s"] for t in calls), default=0.0),
            }
            if step % args.eval_every == 0:
                fields["eval_em"] = evaluate()
            record(step, **fields)
            if step % args.ckpt_every == 0 or step == args.steps:
                (out / "checkpoint.json").write_text(json.dumps({"model": args.model, "step": step, "version": version, "ref": weights}))
        if version != args.steps:
            weights = publish(trainer, args.steps, args.trainers)
            version = args.steps
        record(args.steps, eval_em=evaluate(), note="final")
        (out / "checkpoint.json").write_text(json.dumps({"model": args.model, "step": args.steps, "version": version, "ref": weights}))
    finally:
        app.destroy()


if __name__ == "__main__":
    main()
