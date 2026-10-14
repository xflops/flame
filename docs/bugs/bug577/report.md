# Filesystem storage contention delays heartbeats

Concurrent storage activity can delay heartbeat updates and other Session Manager operations while tasks are being created and executed.

### Scenario 1: concurrent Rust workload on a Kubernetes cluster

With filesystem storage and 10 workers, the Rust benchmark creates tasks while workers launch tasks and report their results.

- Workers continue sending heartbeats at five-second intervals. Packet capture confirms new heartbeat messages arrive at the Session Manager host.
- The Session Manager does not update their heartbeat timestamps promptly and reports a heartbeat timeout after 15 seconds.
- Workers are subsequently marked `NotReady`, despite continuing to send heartbeats.
- Task execution stalls and the benchmark keeps waiting for completion. One failed 1,000-task run left 996 tasks `Succeed` and four tasks `Running`.

For example, one worker's last processed heartbeat was at `08:24:48.604`. New heartbeat packets arrived at `08:24:53.457`, `08:24:58.457`, and `08:25:03.457`, but the Session Manager reported its timeout at `08:25:03.642`.

### Scenario 2: burst submission in a local filesystem-backed cluster

On main commit `ac218db3aca7fc431e4ef04c575a678f7830007e`, a local Podman cluster used one worker, two Session Manager Tokio worker threads, and a five-CPU/eight-GiB VM. After a successful initial run, a repeated burst was submitted with:

```sh
flmping -p -t 30000
```

- At `13:18:17.196 UTC`, the Session Manager logged `Heartbeat timeout for node <Some("flame-readme-perf-worker")>. Closing stream.`
- At `13:18:47.251 UTC`, the worker was recorded as `NotReady`.
- The worker then reported a response timeout and attempted to reconnect.
- Progress stopped at 133 tasks `Succeed`, 29,867 `Pending`, and zero `Running`.
- The client exceeded an external 600-second timeout without completing the run.

These scenarios show that heartbeat updates and task execution can stop progressing during concurrent task creation, execution, and storage activity.
