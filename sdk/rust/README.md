# Flame Rust SDK

`flame-rs` is the Rust SDK for Flame clients and host-shim services.

Start with the central guide: [Flame Rust SDK](../../docs/sdk/rust.md).

## Quick Links

- [SDK overview](../../docs/sdk/index.md)
- [Rust API tutorial](../../docs/tutorials/rust-api.md)
- [Rust Pi example](../../examples/pi/rust/README.md)
- [Rust Candle Based example](../../examples/candle/based/README.md)
- [Flame API reference](../../docs/api/index.md)


### Resource list pages

List calls return `Page<T> { items, paging }`; `Paging` contains `limit`,
`number`, and optional `total` (always populated in server responses). Zero
limit selects 100 and zero number selects page 1. Explicit limits are at most
1000. List results are metadata only; use detailed getters and task watches
for payloads and events.

Each existing list method takes `Option<&ResourceFilter>`. Reusable
`ApplicationFilter`, `NodeFilter`, `ExecutorFilter`, `SessionFilter`, and
`TaskFilter` contain exact filters and `paging: Option<Paging>`.
`Session::list_tasks` lists task metadata; the connection also supports direct
`list_tasks(&session_id, filter)` without first fetching the session. Task pages
are ordered by numeric task ID descending, independently of task state.

Callers select each page through the filter. For example, request page 2:

```rust,ignore
let filter = flame_rs::SessionFilter {
    paging: Some(flame_rs::Paging {
        limit: 100,
        number: 2,
        total: None,
    }),
    ..Default::default()
};
let page = connection.list_sessions(Some(&filter)).await?;
```

Responses include the effective page and total. Subsequent calls read current
state, so entries may shift between pages when resources change.
