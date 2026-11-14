# List pagination

Pagination extends the existing `ListApplications`, `ListSessions`, `ListNodes`,
`ListExecutors` and `ListTasks` RPCs. No additional RPC is required.

Omitting the request's `paging` message retains the complete legacy list,
including task payloads. Sending `paging: {}` requests the first page of 100
items. `page_size` accepts 0 (the default) through 1000; negative or larger
values return `INVALID_ARGUMENT`.

The paging message contains `page_size`, `page_number`, and `order_by`.
Page numbers start at 1; 0 selects the first page. Negative page numbers and
numbers whose calculated offset exceeds the signed 64-bit range return
`INVALID_ARGUMENT`.

```text
page_size = 20, page_number = 3
starting offset = (3 - 1) × 20 = 40
returned matching items = 41 through 60, if available
```

Clients can jump directly to any page. Use the same filters and ordering when
browsing one result set. A page beyond the last page returns no items and still
includes collection counts. There are no continuation tokens or expiry rules.

## Filters and order

Filters are applied before page selection and filtered totals. Repeated exact
IDs or names are ORed; different filter fields are ANDed. An empty repeated
filter selects all values.

| List | Exact filters | Default order |
| --- | --- | --- |
| Applications | state, names | name asc |
| Sessions | application, state, ids | creation_time desc |
| Nodes | state, names | name asc |
| Executors | state, application, node, session_id, ids | id asc |
| Tasks | session_id (required), state, numeric ids | creation_time desc |

`order_by` accepts `id` or `name`, optionally followed by `asc` or `desc`.
Applications, sessions and tasks also support `creation_time`. Creation times
use Unix seconds, with ID as the deterministic tie breaker in the same
direction. Task IDs sort numerically. Unsupported fields or directions return
`INVALID_ARGUMENT`.

Each request pages the latest matching collection; pages are not snapshots.
Unchanged collections produce consistent ordering and totals. Inserts, deletes
or changes to filter membership can shift offsets between requests, so items
can move between pages or be repeated/skipped while traversing a changing
collection. Deep pages require examining skipped records.

## Response summaries

The four unary List responses add a `paging` field alongside their existing
repeated items. It is absent for legacy requests. `PageInfo` contains:

- `page_number` and `page_size`: effective one-based page and size after defaults.
- `total_pages`: `ceil(total_count / page_size)`, or 0 when no items match.
- `total_count`: matching items before selecting a page.
- `resource_count`: all items of that resource before table filters. Tasks are
  scoped to the requested session.
- `state_counts`: counts before table filters, keyed by protobuf state names.
- `totals`: resource aggregates before table filters: session `running_tasks`,
  and node `cpu`, `memory` (bytes), and `gpu`.

`ListTasks` continues returning `stream Task`. For paged requests the initial
binary response metadata entry `flame-page-bin` contains protobuf-encoded
`PageInfo`, including when no tasks match. Read this metadata before consuming
the stream. There are no summary rows in the task stream. Paged tasks omit
input, output, affinity and events unless `include_payloads` is true. Payloads
are then loaded only for selected tasks.

The SDK page APIs reject responses without paging metadata, so an old server
cannot silently turn a paged task request into an unbounded stream.

## SDKs and CLI

Both SDKs retain their existing complete-list methods and add
`list_applications_page`, `list_sessions_page`, `list_nodes_page`,
`list_executors_page` and `list_tasks_page`. Pass a `PageRequest` and the
resource's filter. Results contain `items` and `paging`. Corresponding
`iter_*_pages` helpers (for example, Rust `iter_session_pages` and Python
`iter_sessions_pages`) fetch the next numbered page only when the caller
advances the iterator, stopping at `total_pages`. Python supports synchronous
and asynchronous clients.

```console
flmctl list -s --page-size 20 --page-number 3 --state open --order-by 'creation_time desc'
flmctl list -t --session-id my-session --page-size 50 --output-format json
flmctl list -n --all --page-size 100
```

Without new flags existing list commands keep their table and JSON-array
output. Paged JSON uses `{ "items": [...], "paging": {...} }`. Paged tables
include filtered totals and page numbers. `--all` visits numbered pages from
the first page until the returned total is reached.

## Storage behavior

Application, node and executor pages inspect live in-memory metadata and clone
only selected rows. Session and task pages query their storage engine,
including evicted completed records. SQLite uses metadata projections, counts
and `LIMIT/OFFSET`; filesystem storage scans fixed-size metadata and hydrates
only selected payloads. In-memory storage likewise selects metadata before
cloning the requested page.
