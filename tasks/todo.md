# Typora-style incremental preview

- [x] Match stable block IDs by content, source overlap, and neighbors.
- [x] Add shared `BlockIndexEntry` and expose it to editor/preview.
- [x] Add `replaceBlock(id, html, sourceStart, sourceEnd)` WebView protocol.
- [x] Change scroll bridge to block ID plus intra-block offset.
- [x] Extend parent-boundary tests for list, quote, table, and fenced code.

## Checkpoints

- [x] Parser and preview tests pass after identity/index work.
- [x] WebView protocol tests pass after patch/scroll work.
- [ ] Full tests, clippy, and release build pass.
