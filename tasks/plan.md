# Implementation Plan: Typora-style block identity and incremental preview

## Overview

Make top-level Markdown blocks the shared unit between parsing, native editor state, and WebView preview. Preserve block identity through edits, patch only affected DOM blocks, and synchronize scrolling by block identity plus intra-block offset.

## Architecture decisions

- Keep the existing `ParsedDocument` as the single parse result; attach a reusable block index instead of creating a second parser path.
- Use deterministic IDs derived from block content plus neighborhood matching. IDs are preserved across edits when the same logical block can be matched; index position alone is never a match.
- Keep source position as a compatibility fallback for old documents, virtualized previews, and unmatched blocks.
- Treat list, quote, table, and fenced code regions as parent update boundaries; structural ambiguity falls back conservatively.

## Task list

1. Block identity matching and regression tests.
2. Shared `BlockIndexEntry` for parsed/editor/preview state.
3. ID-based WebView block patch contract with index fallback.
4. Block-anchor scroll protocol with source-position fallback.
5. Parent-boundary coverage for nested structures and fenced code.

## Verification checkpoints

- After tasks 1-2: parser, preview, and model tests pass.
- After tasks 3-4: WebView protocol tests and full test suite pass.
- After task 5: full test suite, clippy, and release build pass.

## Risks

| Risk | Mitigation |
|---|---|
| Duplicate or edited blocks receive the wrong identity | Match by content, source overlap, and neighboring IDs; never blindly copy by index. |
| DOM and native state diverge | Keep IDs in HTML markers and return the same block index metadata from the preview protocol. |
| Scroll jumps when a block changes height | Capture `{blockId, offset}` and fall back to source interpolation only when the block is unavailable. |
| Nested syntax changes alter boundaries | Expand to the complete parent block or use full-parse fallback when boundaries are unstable. |
