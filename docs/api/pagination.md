# List pagination

Each Frontend `ListApplications`, `ListSessions`, `ListNodes`, `ListExecutors`,
and `ListTasks` call returns one page of object metadata. Paging belongs to
the resource filter and uses the same message in requests and responses:

```protobuf
message Paging {
  int32 limit = 1;
  int64 number = 2;
  optional uint64 total = 3;
}
```

A zero or omitted limit selects 100; explicit limits are 1 through 1000.
A zero or omitted number selects the first page. Negative values and offset
overflow are rejected. Responses include effective bounds and filtered total;
a request's total is ignored. SDK list calls return one page; callers set the
next page number in the filter themselves, using total to determine completion.
Each page reads current state, so concurrent
updates can shift records between pages.

Applications sort Enabled first, sessions Open first, and nodes Ready first.
Within each group they sort by newest creation time, then identifier.
Executors sort by node, numeric state, then ID. Tasks sort by numeric ID
descending, independently of state and timestamps.

Application, node, executor and session lists project in-memory metadata. All
retained sessions remain in the existing cache regardless of state or age.
History collection removes expired Closed sessions according to
`cluster.history.retention` (30 days by default), every four hours. Closed sessions
retain the same Session object with final counters and empty task maps/indexes;
terminal task history is read from storage. `max_sessions` limits Open sessions and does not evict history. Task pages
read storage metadata without loading input, output or affinity payloads.
Unfiltered filesystem pages seek directly to the requested records from the end
of the fixed-record file; state-filtered pages scan headers for exact totals.
List responses omit credentials, common-data bytes and events. Detailed gets
and task watches retain their execution-data behavior.

`flmctl list` and `flmctl delete` accept `--limit` and `--number`. Delete inspects
that page and removes only its Closed sessions or NotReady nodes; it does not
traverse later pages. Health checks explicitly traverse all pages. Internal
storage lifecycle queries remain complete when paging is unset. A capped
internal query selects its first page through `filter.paging`; filters have no
separate limit field.
