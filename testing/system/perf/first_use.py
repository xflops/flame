"""First-use latency of a sandbox, new executor versus retained executor.

Run it inside an environment that can reach the cluster, with the Flame Python SDK installed:

    python first_use.py [PAIRS] [WARMUP]

It registers a temporary copy of the ``flmbox`` application with a 3 s release delay, warms the image
cache on the workers with WARMUP sandboxes at once, then runs PAIRS pairs. Each pair creates a sandbox on a
new executor and, as soon as that sandbox is closed, a second one on the retained executor, and times the
create call and the first Python command in each. The temporary application is removed at the end and the
result is printed as JSON.

The timeline points the executor manager and ``flmbox`` log under the ``flame::perf`` target show where
the time goes inside the first command. Enable them with ``RUST_LOG=flame::perf=debug`` on the executor
manager, and put the same variable in the ``environments`` of the application for ``flmbox``, whose log is
``<executor-id>.err`` in the executor's work directory.
"""

import json
import random
import string
import sys
import time

import flamepy.agent.sandbox as sbx
from flamepy.core import ApplicationAttributes, Shim, register_application, unregister_application

pairs = int(sys.argv[1]) if len(sys.argv) > 1 else 12
name = "flmbox-fu-" + "".join(random.choices(string.ascii_lowercase, k=6))
CMD = 'python3 -c "print(1)"'
IMG = "python:3.12-slim"
WARM = int(sys.argv[2]) if len(sys.argv) > 2 else 24
ms = lambda a, b: round((b - a) * 1e3, 1)

register_application(name, ApplicationAttributes(shim=Shim.HOST, command="${FLAME_HOME}/bin/flmbox", arguments=["-d", "--network=host"], description="first-use latency probe", delay_release=3))
sbx._FLMBOX_APP = name
rows = []


def warm_one(_):
    sb = sbx.Sandbox.create(image=IMG)
    try:
        return sb.run(CMD).exit_code
    finally:
        sb.close()


try:
    # Spread sandboxes over the workers so the image alias is cached on each before measuring.
    from concurrent.futures import ThreadPoolExecutor

    t = time.perf_counter()
    with ThreadPoolExecutor(WARM) as pool:
        codes = list(pool.map(warm_one, range(WARM)))
    print("warm-up:", WARM, "sandboxes, exit codes", sorted(set(codes)), round(time.perf_counter() - t, 1), "s", file=sys.stderr)
    time.sleep(7)
    # Warm the SDK connection and imports first; the blog excludes connection setup.
    for i in range(pairs):
        row = {"pair": i}
        for kind in ("new", "retained"):
            wall = time.time()
            t0 = time.perf_counter()
            sb = sbx.Sandbox.create(image=IMG)
            t1 = time.perf_counter()
            r = sb.run(CMD)
            t2 = time.perf_counter()
            r2 = sb.run(CMD)
            t3 = time.perf_counter()
            row[kind] = {"id": sb.id, "wall": wall, "create_ms": ms(t0, t1), "first_run_ms": ms(t1, t2), "total_ms": ms(t0, t2), "second_run_ms": ms(t2, t3), "rc": r.exit_code}
            sb.close()
        rows.append(row)
        time.sleep(7)  # let the 3 s release delay expire so the next pair starts on a new executor
finally:
    unregister_application(name)
print(json.dumps({"app": name, "rows": rows}))
