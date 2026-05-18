# Pi Example (Python)

## Motivation

Estimating π (Pi) is a classic computational problem that is well-suited for parallelization. The Monte Carlo method, which randomly samples points in a unit square and determines their location relative to a quarter circle, allows us to estimate Pi by calculating the ratio of points inside the circle to the total number of samples.

In traditional single-threaded programs, processing a very large number of samples can be slow and resource-intensive. By leveraging the `flamepy.Runner` API, we can distribute the workload across multiple processes or machines, running many estimations in parallel. This not only speeds up the calculation but also demonstrates the power and simplicity of distributed computing with Flame.

This example illustrates:
- How to parallelize an embarrassingly parallel problem (each batch is independent)
- How to use `flamepy.Runner` to launch and orchestrate remote computations
- The ease of scaling up workloads to achieve faster, more accurate results

Whether you're estimating Pi or tackling more complex scientific and engineering problems, this approach showcases how distributed computing can make problem-solving more efficient and accessible for Python developers.


## Overview

The Pi example demonstrates how to use the `flamepy.Runner` API to perform a distributed estimation of π (Pi) using the Monte Carlo method.

### How It Works

- **Monte Carlo Estimation**: The core estimation logic (`estimate_batch`) generates many random points within a unit square and counts how many fall inside the quarter circle. The ratio of points inside to total points, multiplied by 4, approximates π.

- **Parallel Batching with Runner**: Instead of estimating with a single massive batch, the workload is split into multiple batches (`num_batches`), each consisting of `samples_per_batch` points. This enables parallel execution.

- **Distributed Execution**: Using `flamepy.Runner`, each batch's computation is submitted as a separate remote task via the `Runner.service()` API. These tasks can run in parallel across the available compute resources in the Flame cluster.

- **Aggregation**: Results from all batches are collected and summed. The final estimate for π is computed based on the total points inside the circle versus all samples.

### Files

- **`main.py`**: Contains the batch estimator and distributed orchestration. It submits batches to the Flame cluster, collects results, and prints the final estimate.
- **`pyproject.toml`**: Defines the package and dependencies, including `flamepy`.
- **`README.md`**: This file; explains the motivation, approach, usage, and output of the example.

### Key Benefits

This example highlights:
- How to apply distributed computing to classic numerical problems.
- The minimal code changes required to scale a standard Python function with Flame Runner.
- How to keep a small Runner example self-contained in one script.

This approach easily adapts to other Monte Carlo or embarrassingly parallel workloads—just replace the batch logic!


## Example Output

Step 1: Start the Flame cluster with Docker Compose

```shell
[klausm@flm-worker flame]$ docker compose up -d
WARN[0000] No services to build                         
[+] up 4/4
 ✔ Network flame_default                    Created                                                                                                                                                           0.0s 
 ✔ Container flame-flame-session-manager-1  Created                                                                                                                                                           0.0s 
 ✔ Container flame-flame-executor-manager-1 Created                                                                                                                                                           0.0s 
 ✔ Container flame-flame-console-1          Created                                                                                                                                                           0.0s 
```

Step 2: log in to flame-console and run the pi example

```shell
[klausm@flm-worker flame]$ docker compose exec -it flame-console /bin/bash
root@0e24c41da5f5:/# cd /opt/examples/pi/python/
root@0e24c41da5f5:/opt/examples/pi/python# ls
README.md  main.py  pyproject.toml
```

Step 3: pi example output

```shell
root@0e24c41da5f5:/opt/examples/pi/python# uv run -n main.py 
============================================================
Monte Carlo Estimation of PI using Flame Runner
============================================================

Configuration:
  Batches: 10
  Samples per batch: 1,000,000
  Total samples: 10,000,000

Running distributed Monte Carlo simulation...


Results:
  Estimated PI: 3.1416348000
  Actual PI:    3.1415926536
  Error:        0.0000421464 (0.001342%)
============================================================

root@0e24c41da5f5:/opt/examples/pi/python# flmctl list -s
 ID  State  App  Resources  Priority  Pending  Running  Succeed  Failed  Created

root@0e24c41da5f5:/opt/examples/pi/python# flmctl list -a
 Name     State    Tags  Created   Shim  Command                              
 flmrun   Enabled        13:20:05  Host  /usr/bin/uv                          
 flmping  Enabled        13:20:05  Host  /usr/local/flame/bin/flmping-service 
 flmexec  Enabled        13:20:05  Host  /usr/local/flame/bin/flmexec-service 
```
