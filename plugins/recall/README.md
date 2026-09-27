# History recall

The `recall` native package serves `eden.history-recall.v1`. It reads the current session through `eden.session-store.v2` (`Read`) and public history's active ancestor path. It never opens another session, mutates history, or executes recorded tools. Records retained before compaction remain eligible.

## Requests and pages

Send `RecallRequest` from `eden_protocol::recall`, for example:

```json
{
  "query": {"operation": "search", "literal": "rare.*", "from_sequence": 1},
  "max_bytes": 8192,
  "max_records": 16
}
```

Search is a case-sensitive literal substring over each record's compact payload JSON, including JSON escapes; it is not regex, semantic retrieval, or a full conversation projection. `from_sequence` and `through_sequence` are optional inclusive bounds. A search literal must contain 1–4096 UTF-8 bytes. Use `{"query":{"operation":"read","sequence":42}}` to retrieve a known record. A sibling-branch record returns `RecordNotFound`; search can return ancestors originally committed on a different branch.

Each chunk identifies its source session, record sequence, origin branch, run and kind, and carries a UTF-8 slice with `byte_start`, `byte_end` (exclusive), and `total_bytes`. Concatenating chunks for that source reconstructs the compact original payload JSON. Session plus sequence is the stable identity within this history; copying a session creates a new identity.

`max_bytes` limits total returned payload text to 4–65536 bytes, default 8192. `max_records` limits chunks per page to 1–128, default 16. Provenance, cursor and JSON framing are excluded from the payload byte budget. Storage currently returns its full snapshot internally; these limits bound the caller's payload rather than the underlying store read. Large individual payloads continue on a UTF-8 boundary, including when a page cannot fit the next character.

`truncated: true` always accompanies `next_cursor`. Send that opaque cursor with the same query to continue; page sizes may change. An empty exhausted search returns no chunks, `truncated: false`, and no cursor. The cursor binds session, active branch, active head, committed revision and query. Pure appends on the same branch allow continuation against that original snapshot, excluding all later records. This lets the model continue after its own tool results have been appended. Navigation, a different session or query, or loss of the original ancestor path returns `StaleCursor` and requires restarting. The service relies on the public store contract that committed records are immutable; it does not rehash history. Malformed, unsupported or invalid-position cursors return `InvalidCursor`.

## Tool contribution

The package also serves `eden.history-recall-tools.v1` with the standard `CatalogRequest`/`Catalog` payloads and `eden.history-recall-tool.v1` with `ToolRequest`/`ToolResult`. The catalog advertises the read-only `history_recall` tool. Its arguments are a recall request; its text and structured `details` carry the recall page, and its `truncated` flag mirrors the page.

Add the contribution to coding-tools configuration and include `history_recall` in the selected tool list:

```json
{
  "tools": ["read", "history_recall"],
  "contributions": [{
    "catalog": "eden.history-recall-tools.v1",
    "execute": "eden.history-recall-tool.v1",
    "read_only": true
  }]
}
```

This requires routing all three recall services to the mounted recall package and the default store to the same current session. The recall service verifies the call's session identity against the returned store snapshot.
