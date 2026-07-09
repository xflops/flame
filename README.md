# Flame: A Distributed Engine for Elastic Workloads

[![license](https://img.shields.io/github/license/xflops/flame)](http://github.com/xflops/flame)
[![RepoSize](https://img.shields.io/github/repo-size/xflops/flame)](http://github.com/xflops/flame)
[![Release](https://img.shields.io/github/release/xflops/flame)](https://github.com/xflops/flame/releases)
[![CII Best Practices](https://bestpractices.coreinfrastructure.org/projects/7299/badge)](https://bestpractices.coreinfrastructure.org/projects/7299)
[![Ask DeepWiki](https://deepwiki.com/badge.svg)](https://deepwiki.com/xflops/flame)

Flame is a distributed engine for elastic workloads, including AI agents, reinforcement learning, and quantitative computing. It provides a comprehensive suite of core mechanisms for running these workloads at scale. Built on over a decade and a half of experience running diverse high-performance workloads across multiple systems and platforms, Flame incorporates best-of-breed ideas and practices from the open source community.

## Motivation

As AI workloads become increasingly adopted for innovation, a common runtime is essential to accelerate these elastic workloads through the following key aspects:

* **Scale**: Unlike applications running on a single node, Flame scales workloads across multiple nodes to maximize performance acceleration while ensuring fair resource sharing across multiple tenants and sessions.
* **Performance**: Elastic workloads typically involve tens of thousands of short tasks. Flame leverages cutting-edge features to improve roundtrip times and throughput in large-scale environments, while intelligently sharing runtime within sessions to minimize startup time.
* **Security**: Flame utilizes microVM as a runtime for enhanced security, with each runtime environment (executor) dedicated to a single session to prevent data leakage. All Flame components communicate using mTLS for secure inter-component communication.
* **Flexibility**: Flame defines a comprehensive set of general APIs to support multiple user scenarios. Additionally, Flame supports applications across multiple programming languages through gRPC, including Rust, Go, and Python.

## Performance

Flame is designed for high-throughput task execution. Here's a benchmark running 30,000 tasks on a single-node deployment:

```shell
root@06383dd94875:/# flmping -p -t 30000
Session <flmping-1N1sIX> was created in <1 ms>, start to run <30,000> tasks in the session:

============================================================
BENCHMARK RESULTS
============================================================
Duration:        3.29s
Succeeded:       30000/30000
Failed:          0
Throughput:      9124.09 tasks/sec
============================================================

root@06383dd94875:/# flmctl list -s
 ID              State   App      Resources             Priority  Pending  Running  Succeed  Failed  Created
 flmping-1N1sIX  Closed  flmping  cpu=1,mem=1Gi,gpu=0  0         0        0        30000    0       18:11:26
```

## Architecture Overview

![Flame Architecture](docs/images/flame-arch.jpg)

### Core Concepts

**Session:** A `Session` represents a group of related tasks. The `Session Scheduler` allocates resources to each session based on scheduling configurations by requesting the resource manager (e.g., Kubernetes) to launch executors. Clients can continuously create tasks until the session is closed.

**Task:** A task within a `Session` contains the main algorithm defined by the task's metadata and input/output information (e.g., volume paths).

**Executor:** The Executor manages the lifecycle of Applications/Services, which contain the user's code for executing tasks. Applications are typically not reused between sessions, though images may be reused to avoid repeated downloads.

**Shim:** The protocol implementation used by the Executor to manage applications, supporting various protocols such as gRPC, RESTful APIs, stdio, and more.

### How It Works

Flame accepts connections from user clients and creates `Session`s for jobs. Clients can continuously submit tasks to a session until it's closed, with no predefined replica requirements.

The `Session Scheduler` allocates resources to each session based on scheduling configurations by requesting the resource manager (e.g., Kubernetes) to launch executors.

Executors connect back to Flame via `gRPC` to pull tasks from their related `Session` and reuse the executor. Executors are released/deleted when no more tasks remain in the related session.

Services receive notifications when they're bound or unbound to related sessions, allowing them to take appropriate actions (e.g., connecting to databases). Services can then pull tasks from the `Session` and reuse data to accelerate execution.

Future enhancements to the `Session Scheduler` will include features to improve performance and usage, such as proportional allocation, delayed release, and min/max constraints.

## Quick Start Guide

### Option 1: Docker Compose (Recommended for First-Time Users)

This guide uses [Docker Compose](https://docs.docker.com/compose/) to start a local Flame cluster. After installing docker compose, you can start a local Flame cluster with the following steps:

```shell
$ docker compose up -d
```

After the Flame cluster is launched, use the following steps to log into the `flame-console` pod, which serves as a debug tool for both developers and SREs:

```shell
$ docker compose exec flame-console /bin/bash
```

### Option 2: Local Installation with flmadm (Faster for Development)

For development and testing, you can install Flame directly on your machine using `flmadm` (requires [Rust](https://rustup.rs/) and [uv](https://astral.sh/uv)):

```shell
# Build and install flmadm
$ cargo build --release -p flmadm
$ sudo install -m 755 target/release/flmadm /usr/local/bin/

# Install all components from local source and start services
$ sudo flmadm install --all --src-dir . --enable

# Add Flame binaries to PATH
$ source /usr/local/flame/sbin/flmenv.sh
```

For more details, see the [flmadm README](flmadm/README.md).

### Verify the Installation

After starting Flame (via either option), verify the installation with `flmping`:

```shell
$ flmping
Session <flmping-Sf4R2o> was created in <1 ms>, start to run <10> tasks in the session:

 Session         Task  State    Output
 flmping-Sf4R2o  1     Succeed  Completed on <396003ae48dd> in <0> milliseconds with <0> memory
 flmping-Sf4R2o  2     Succeed  Completed on <396003ae48dd> in <0> milliseconds with <0> memory
 flmping-Sf4R2o  3     Succeed  Completed on <396003ae48dd> in <0> milliseconds with <0> memory
 flmping-Sf4R2o  4     Succeed  Completed on <396003ae48dd> in <0> milliseconds with <0> memory
 flmping-Sf4R2o  5     Succeed  Completed on <396003ae48dd> in <0> milliseconds with <0> memory
 flmping-Sf4R2o  6     Succeed  Completed on <396003ae48dd> in <0> milliseconds with <0> memory
 flmping-Sf4R2o  7     Succeed  Completed on <396003ae48dd> in <0> milliseconds with <0> memory
 flmping-Sf4R2o  8     Succeed  Completed on <396003ae48dd> in <0> milliseconds with <0> memory
 flmping-Sf4R2o  9     Succeed  Completed on <396003ae48dd> in <0> milliseconds with <0> memory
 flmping-Sf4R2o  10    Succeed  Completed on <396003ae48dd> in <0> milliseconds with <0> memory


<10> tasks was completed in <153 ms>.
```

You can check session status using `flmctl`. Explore more examples [here](examples):

```shell
$ flmctl list -s
 ID              State   App      Resources             Priority  Pending  Running  Succeed  Failed  Created
 flmping-Sf4R2o  Closed  flmping  cpu=1,mem=1Gi,gpu=0  0         0        0        10       0       13:33:30
```

## CLI Tools

Flame provides two separate command-line tools:

- **`flmctl`**: User-facing CLI for submitting jobs, managing sessions, and querying cluster state
- **`flmadm`**: Administrator CLI for installing, configuring, and managing Flame clusters

### Installing Flame with flmadm

For multi-node bare-metal or VM deployments, use `flmadm` to install Flame components on each node:

```shell
# On control plane node
sudo flmadm install --control-plane --enable

# On worker nodes
sudo flmadm install --worker --enable

# On cache nodes (optional, can be co-located with workers)
sudo flmadm install --cache --enable

# Or deploy all components on a single node
sudo flmadm install --all --enable
```

For more details, see the [flmadm README](flmadm/README.md).

## Documentation

* [API Reference](docs/api/index.md)
* [SDKs](docs/sdk/index.md): [Rust SDK](docs/sdk/rust.md), [Python SDK](docs/sdk/python.md)
* [Rust API Tutorial](docs/tutorials/rust-api.md)
* [Local Development](docs/tutorials/local-development.md)
* [Runner Setup Guide](docs/tutorials/runner-setup.md)
* [Release Process](docs/release-process.md)
* [Design Documents](docs/designs/)

## API Reference

* **Frontend API**: [frontend.proto](rpc/protos/frontend.proto)
* **Shim API**: [shim.proto](rpc/protos/shim.proto)

## Contributing

We welcome contributions through GitHub issues and pull requests.

## License

This project is licensed under the terms specified in the [LICENSE](LICENSE) file.

## Star History

[![Star History Chart](https://api.star-history.com/svg?repos=xflops/flame&type=timeline&legend=top-left)](https://www.star-history.com/#xflops/flame&type=timeline&legend=top-left)
