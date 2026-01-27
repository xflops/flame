# Running Monte Carlo Pi with Flame Runner (Python)

## Motivation

Building distributed Python apps typically involves manual packaging, uploading,
deploying, and cleanup. The Flame `Runner` is designed to manage the lifecycle
of your application so you can focus on the logic. It packages your project,
registers the application, creates services, and cleans up resources on exit.
That lifecycle management reduces boilerplate, avoids orphaned apps/packages,
and makes iteration fast and predictable.

## Runner Architecture Overview

Runner is a context manager that wraps three core responsibilities:

- **Packaging and storage** via `FlameContext.package`. On entry, Runner builds a
  `.gz` archive of the current working directory, applies exclusion rules, and
  uploads it to the configured `storage` (currently `file://`).
- **Application registration**. Runner clones the `flmrun` application template,
  updates its `name` and `url`, and registers it as the runnable application for
  your package.
- **Service lifecycle**. `Runner.service(...)` creates a `RunnerService` around
  a function, class, or instance. Each method call submits a task and returns an
  `ObjectFuture`. On exit, Runner closes services, unregisters the app, and
  deletes the uploaded package.

`ObjectFuture` provides a simple `get()` API to retrieve results and can be
passed as arguments to avoid unnecessary data movement when chaining tasks.

To make working with multiple `ObjectFuture` results easier, the `Runner` provides helper methods such as `get()`, which efficiently retrieves the final results from slices or collections of `ObjectFuture` instances.

Here's a basic example of using `Runner` in a synchronized way. The `test_fn` can be a function, a class, or an instance—Runner will handle packaging and remote execution transparently:

```python
with Runner("test") as rr:
    results = rr.service(test_fn)
    outputs = rr.get(results)
    print(outputs)
```

In addition to the synchronous approach, `Runner` also supports asynchronous execution via its `select(...)` method. The `select(...)` method returns an iterator that yields each completed `ObjectFuture` as soon as it is ready, enabling you to process results as tasks finish rather than waiting for all to complete.

```python
with Runner("test") as rr:
    results = rr.service(test_fn)

    for result in rr.select(results)
        print(result.get())
```


## Pi Python Example: Monte Carlo with Runner

The Pi example uses the Monte Carlo method to estimate π by splitting work into
multiple batches and running them in parallel. The core computation is a simple
function:

```python
def estimate_batch(num_samples: int) -> int:
    x = np.random.rand(num_samples)
    y = np.random.rand(num_samples)
    inside_circle = np.sum(x*x + y*y <= 1.0)
    return int(inside_circle)
```

Runner turns that function into a remote service and manages everything else:

```python
from flamepy import Runner
from pi import estimate_batch
import math

num_batches = 10
samples_per_batch = 1_000_000
total_samples = num_batches * samples_per_batch

# Create a `pi-estimation` application in Flame cluster,
# and then execute `estimate_batch` remotely.
with Runner("pi-estimation") as rr:
    estimator = rr.service(estimate_batch)
    results = [estimator(samples_per_batch) for _ in range(num_batches)]
    insides = rr.get(results)

pi_estimate = 4.0 * sum(insides) / total_samples
error = abs(pi_estimate - math.pi)
```

Below is a sample output produced by running `uv run main.py` from the `examples/pi/python` directory:


```shell
============================================================
Monte Carlo Estimation of PI using Flame Runner
============================================================

Configuration:
  Batches: 10
  Samples per batch: 1,000,000
  Total samples: 10,000,000

Running distributed Monte Carlo simulation...

Results:
  Estimated PI: 3.1416552000
  Actual PI:    3.1415926536
  Error:        0.0000625464 (0.001991%)
============================================================
```
_Note: Your actual numbers may vary due to randomness and available hardware, but the estimated PI should be accurate to 3-4 digits even with a small number of batches._


**Why this benefits end users:**

- **Lifecycle management**. No manual packaging or cleanup; the context manager
  handles it automatically.
- **Simple parallelism**. A plain Python function becomes a distributed service
  with one line (`rr.service(...)`).
- **Fast iteration**. Update your code and rerun; Runner rebuilds and registers
  the app for you.
- **Cleaner code**. The application logic stays focused on computation, not
  cluster mechanics.

## References

- `RFE280 - Simplify the Python API of Flame` (`../designs/RFE280-runner.md`)
- `Pi Runner example` (`../../examples/pi/python/main.py`)
- `Runner setup` (`../runner-setup.md`)
- `Flame GitHub` (`https://github.com/flame-sh/flame`)
