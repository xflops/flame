# GPU selection and runtime images

Design: [RFE565: FEM GPU selection](../designs/RFE565-gpu-selection/FS.md).

FEM assigns physical GPUs according to each executor's `resreq.gpu`. Assignments
remain reserved across session bindings, including the executor's Idle retention
period. FEM releases them after the application service has stopped. CPU-only
executors receive an empty `CUDA_VISIBLE_DEVICES` value when the GPU manager
is enabled.

Enable the GPU device manager in the cluster configuration (also the default
when `devices` is omitted):

```yaml
cluster:
  executors:
    shims: [host]
    devices: [gpu]
```

Use `devices: []` to disable discovery and advertise zero GPU capacity. This
also disables Host GPU visibility management; application environment settings
are preserved without assignment overrides. GPU is
the only implemented manager; configuring TPU or other names fails at startup.
The Helm value is `cluster.executors.devices`.

At startup, the configured GPU manager discovers GPU UUIDs through NVML. A FEM-level
`CUDA_VISIBLE_DEVICES` limits its inventory to the specified device indices or
UUIDs. Application-level visibility settings are overridden by FEM's assignment.
Use complete UUIDs rather than abbreviated prefixes. Do not mix device indices,
GPU UUIDs, and MIG identifiers in one inventory. The advertised GPU capacity uses
the refreshed inventory. FEM rediscovers devices before every node registration
and heartbeat, preserving assignments already in use.

## Host execution

The GPU manager supplies `CUDA_VISIBLE_DEVICES` in `DeviceAssignment.envs`.
The Host shim applies environment settings from all assignments after merging
application and installer settings, then starts the service with those values. The executor and its retained model keep the same devices
until release. GPU devices and NVIDIA driver libraries must be available to FEM,
including `LD_LIBRARY_PATH` when the driver is mounted outside standard locations.

CUDA visibility is cooperative selection, not isolation from other device files.
Use dedicated Flame workers and the CRI shim when container device isolation is
required. The Kubernetes Helm chart currently uses the Host shim.

## CRI application images

Install a CDI-capable container runtime, the host NVIDIA driver, and NVIDIA
Container Toolkit. Generate or refresh the CDI specification and check its device
names before starting FEM:

```bash
sudo nvidia-ctk cdi generate --output=/var/run/cdi/nvidia.yaml
nvidia-ctk cdi list
```

The specification must include names for the IDs advertised by FEM, such as
`nvidia.com/gpu=GPU-...` for UUID discovery. NVIDIA documents generation and driver
injection in its [CDI guide](https://docs.nvidia.com/datacenter/cloud-native/container-toolkit/latest/cdi-support.html).
For CRI, leave FEM `CUDA_VISIBLE_DEVICES` unset for NVML UUID discovery, or set
it to complete GPU or modern MIG UUIDs. Numeric CUDA indices do not necessarily
match CDI indices, and legacy MIG path identifiers have different CDI names.
Use identifiers that match the host CDI specification.

Enable CDI in the runtime and select a default runtime that supports GPU workloads.
Flame uses the runtime's configured default; a CPU-only gVisor configuration does
not provide GPU support by itself.

Run one FEM per dedicated host. FEM loads saved assignments before registering
its node and reports its local executors. Runtime lookup happens only inside
CRI shim service creation. Preserve the working directory across the intended restart;
FEM does not infer physical GPU ownership from resource counts.

Configure FEM with `cluster.executors.shim: cri`. FEM and the container runtime
must see the same executor work directories and installed package paths. This is
the existing bare-metal CRI setup; changing the Helm worker deployment to CRI is a
separate deployment task.

Deploy the vLLM serving example with an application image:

```bash
source /usr/local/flame/sbin/flmenv.sh --python-version 3.12
cd "$FLAME_HOME/examples/vllm/serving"
flmctl deploy --name vllm-serving --application . --shim Cri \
  --image xflops/flame-instance-runtime:latest --max-instances 1 \
  --env VLLM_MODEL=facebook/opt-125m
python3 client.py --prompt "The future of distributed inference is"
```

The client requests `gpu=1`. CRI receives only the assigned
`nvidia.com/gpu=<device>` names. CDI provides the devices and host driver libraries;
the image supplies Python and application dependencies. GPU-free containers receive
no CDI devices. FEM overrides `NVIDIA_VISIBLE_DEVICES` to `void` to prevent legacy
NVIDIA runtime injection from adding extra devices. CUDA visibility contains the
assigned identifiers, such as GPU UUIDs, for both Host and CRI. Device managers
populate envs and CDI uniformly; each shim consumes the fields it supports.

The existing `flame-instance-runtime` image is published for amd64 and arm64.
Choose its tag with `--image`; the image and host driver must support the
application's CUDA requirements. Driver injection belongs to the host runtime,
so the image does not bundle a host NVIDIA driver. Applications that use Triton
(vLLM) compile a C helper at the first kernel launch and need a C compiler and
Python headers: use the `-devel` tag (`xflops/flame-instance-runtime:<tag>-devel`),
which adds them.

Keep each FEM's GPU assignment directory private to that worker. Workers may
share installed application packages, but must not share `workdir/gpu`: discovery
validates the saved ledger against that FEM's active assignments. The Compose
deployment mounts a separate anonymous volume at `workdir/gpu` for each replica.

FEM saves each assignment as `<workdir>/gpu/<executor-id>.json`. Allocation
writes that executor's file. Releasing stops the service and unregisters the
executor. Session manager sends Released through the ordinary executor-update
stream after the service has been destroyed. FEM removes the saved assignment
and then removes its executor-map entry. The assignment remains reserved when
cleanup or device release fails.
Each JSON file contains only `devices`, `envs`, and `cdi`; the filename supplies
the executor ID. The manager restores its internal map from these files before
reporting capacity. No CRI handles are written to disk.

Preserve the working directory when restarting FEM to retain device ownership
until cleanup. Helm's work `emptyDir` survives a container restart; replacement
of the Pod requires persistent storage. Session manager marks executors absent
from FEM's local registration report as Releasing, so a FEM process restart
cleans up those services.
CRI shim service creation reconnects an existing executor workload before
installing or starting a new service.
