# Typora-style incremental preview

- [ ] Match stable block IDs by content, source overlap, and neighbors.
- [ ] Add shared `BlockIndexEntry` and expose it to editor/preview.
- [ ] Add `replaceBlock(id, html, sourceStart, sourceEnd)` WebView protocol.
- [ ] Change scroll bridge to block ID plus intra-block offset.
- [ ] Extend parent-boundary tests for list, quote, table, and fenced code.

## Checkpoints

- [ ] Parser and preview tests pass after identity/index work.
- [ ] WebView protocol tests pass after patch/scroll work.
- [ ] Full tests, clippy, and release build pass.
