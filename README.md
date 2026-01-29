# Flame: A Distributed System for Agentic AI

[![license](https://img.shields.io/github/license/xflops/flame)](http://github.com/xflops/flame)
[![RepoSize](https://img.shields.io/github/repo-size/xflops/flame)](http://github.com/xflops/flame)
[![Release](https://img.shields.io/github/release/xflops/flame)](https://github.com/xflops/flame/releases)
[![CII Best Practices](https://bestpractices.coreinfrastructure.org/projects/7299/badge)](https://bestpractices.coreinfrastructure.org/projects/7299)
[![Ask DeepWiki](https://deepwiki.com/badge.svg)](https://deepwiki.com/xflops/flame)

Flame is a distributed system designed for Agentic AI, providing a comprehensive suite of mechanisms commonly required by various classes of Agentic AI workloads, including tools, Agent, and more. Built upon over a decade and a half of experience running diverse high-performance workloads at scale across multiple systems and platforms, Flame incorporates best-of-breed ideas and practices from the open source community.

## Motivation

As Agentic AI become increasingly adopted for innovation, a common workload runtime is essential to accelerate these elastic workloads through the following key aspects:

* **Scale**: Unlike applications running on a single node, Flame scales workloads across multiple nodes to maximize performance acceleration while ensuring fair resource sharing across multiple tenants and sessions.
* **Performance**: Elastic workloads typically involve tens of thousands of short tasks. Flame leverages cutting-edge features to improve roundtrip times and throughput in large-scale environments, while intelligently sharing runtime within sessions to minimize startup time.
* **Security**: Flame utilizes microVM as a runtime for enhanced security, with each runtime environment (executor) dedicated to a single session to prevent data leakage. All Flame components communicate using mTLS for secure inter-component communication.
* **Flexibility**: Flame defines a comprehensive set of general APIs to support multiple user scenarios. Additionally, Flame supports applications across multiple programming languages through gRPC, including Rust, Go, and Python.

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

### Option 2: Local Installation (Faster for Development)

For development and testing, you can install Flame directly on your machine using `flmadm`:

```shell
# Quick start with helper script
$ ./hack/local-test.sh install
$ ./hack/local-test.sh start

# Or using Make
$ make install-dev
$ /tmp/flame-dev/bin/flame-session-manager --config /tmp/flame-dev/conf/flame-cluster.yaml &
$ /tmp/flame-dev/bin/flame-executor-manager --config /tmp/flame-dev/conf/flame-cluster.yaml &
```

For more details, see the [Local Development Guide](docs/tutorials/local-development.md).

Then, verify the installation with `flmping` in the pod. Additionally, you can explore more meaningful examples [here](examples):

```shell
root@560624b037c9:/# flmping
Session <1> was created in <3 ms>, start to run <10> tasks in the session:

 Session         Task  State    Output                                                          
 flmping-UdjmHs  8     Succeed  Completed on <fc5afd603feb> in <0> milliseconds with <0> memory 
 flmping-UdjmHs  6     Succeed  Completed on <fc5afd603feb> in <0> milliseconds with <0> memory 
 flmping-UdjmHs  10    Succeed  Completed on <fc5afd603feb> in <0> milliseconds with <0> memory 
 flmping-UdjmHs  7     Succeed  Completed on <fc5afd603feb> in <0> milliseconds with <0> memory 
 flmping-UdjmHs  2     Succeed  Completed on <fc5afd603feb> in <0> milliseconds with <0> memory 
 flmping-UdjmHs  1     Succeed  Completed on <fc5afd603feb> in <0> milliseconds with <0> memory 
 flmping-UdjmHs  3     Succeed  Completed on <fc5afd603feb> in <0> milliseconds with <0> memory 
 flmping-UdjmHs  5     Succeed  Completed on <fc5afd603feb> in <0> milliseconds with <0> memory 
 flmping-UdjmHs  9     Succeed  Completed on <fc5afd603feb> in <0> milliseconds with <0> memory 
 flmping-UdjmHs  4     Succeed  Completed on <fc5afd603feb> in <0> milliseconds with <0> memory 


<10> tasks was completed in <473 ms>.
```

You can check session status using `flmctl` as follows. It also includes several sub-commands, such as `list`:

```shell
root@560624b037c9:/# flmctl list -s
 ID              State   App      Slots  Pending  Running  Succeed  Failed  Created  
 flmping-UdjmHs  Closed  flmping  1      0        0        10       0       06:57:53
```

## CLI Tools

Flame provides two separate command-line tools:

- **`flmctl`**: User-facing CLI for submitting jobs, managing sessions, and querying cluster state
- **`flmadm`**: Administrator CLI for installing, configuring, and managing Flame clusters

### Installing Flame with flmadm

For bare-metal or VM installations, use `flmadm` to install Flame:

```shell
# Basic installation (from GitHub)
sudo flmadm install

# Install from local source
sudo flmadm install --src-dir /path/to/flame

# Install and start services
sudo flmadm install --enable

# User-local installation (no systemd)
flmadm install --no-systemd --prefix ~/flame
```

For more details, see the [flmadm README](flmadm/README.md).

## Documentation

* [Building AI Agents with Flame](docs/blogs/run-ai-agent-with-flame.md)
* [Executing LLM-Generated Code with Flame](docs/blogs/run-generated-script-via-flame.md)
* [Estimating the value of Pi using Monte Carlo](docs/blogs/evaluating-pi-by-monte-carlo.md)
* [Estimating the value of Pi using Flame Python Client](docs/blogs/evaluating-pi-by-flame-python.md)

## API Reference

* **Frontend API**: [frontend.proto](rpc/protos/frontend.proto)
* **Shim API**: [shim.proto](rpc/protos/shim.proto)

## Contributing

We welcome contributions! Please see our [contributing guidelines](CONTRIBUTING.md) for more information.

## License

This project is licensed under the terms specified in the [LICENSE](LICENSE) file.

## Star History

[![Star History Chart](https://api.star-history.com/svg?repos=xflops/flame&type=timeline&legend=top-left)](https://www.star-history.com/#xflops/flame&type=timeline&legend=top-left)
