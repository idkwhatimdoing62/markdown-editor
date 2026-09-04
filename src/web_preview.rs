//! 使用系统浏览器引擎执行主题 CSS 的 Markdown 预览。

use std::borrow::Cow;
use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use crate::markdown::{is_block_tag, is_block_tag_end};
use pulldown_cmark::{CowStr, Event, Tag, html};

// DOM bridge markers are intentionally namespaced away from the IPC
// `md-source:` messages and from arbitrary comments in user-authored HTML.
const DOM_SOURCE_MARKER_PREFIX: &str = "<!--md-editor-source:";
const DOM_BLOCK_MARKER_PREFIX: &str = "<!--md-editor-block:";
#[cfg(target_os = "windows")]
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use wry::dpi::{PhysicalPosition, PhysicalSize};
use wry::http::header::{ACCESS_CONTROL_ALLOW_ORIGIN, CACHE_CONTROL, CONTENT_TYPE};
use wry::http::{Request, Response};
use wry::{DragDropEvent, Rect, WebView, WebViewBuilder};
#[cfg(target_os = "windows")]
use wry::{ScrollBarStyle, WebViewBuilderExtWindows};

pub struct BrowserPreview {
    webview: Option<WebView>,
    document_hash: u64,
    document_source: Option<Arc<PreviewDocument>>,
    document_changed: bool,
    bounds: Option<[i32; 4]>,
    visible: bool,
    frozen_frame: Option<egui::TextureHandle>,
    scroll_bridge: Arc<Mutex<ScrollBridge>>,
    document_payload: Arc<Mutex<Option<Arc<PreviewDocument>>>>,
    local_image_requests: Arc<AtomicUsize>,
    mermaid_runtime_requests: Arc<AtomicUsize>,
    font_asset_requests: Arc<[AtomicUsize; 4]>,
    applied_font_size: Option<(u32, u32)>,
}

#[derive(Clone)]
pub struct PreviewDocument {
    shell: Arc<str>,
    body_range: Option<(usize, usize)>,
    virtual_manifest: Option<Arc<str>>,
    blocks: Arc<[PreviewChunk]>,
    chunks: Arc<[PreviewChunk]>,
    hash: u64,
    chrome_hash: u64,
    has_mermaid: bool,
    total_bytes: usize,
    block_fingerprint: u64,
    /// Stable identity for each top-level Markdown block, independent of its
    /// position in the document. These IDs are embedded in the preview DOM so
    /// navigation and incremental updates can target a block after edits.
    block_ids: Arc<[u64]>,
    /// The same shared index carried by `ParsedDocument`, enriched with the
    /// latest direct-preview height estimates when available.
    block_index: Arc<[crate::markdown::BlockIndexEntry]>,
    update_stats: PreviewUpdateStats,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct PreviewUpdateStats {
    pub replaced_blocks: usize,
    pub replaced_virtual_chunks: usize,
}

const VIRTUALIZE_AT_BYTES: usize = 512 * 1024;
const VIRTUALIZE_AT_IMAGES: usize = 8;
const VIRTUAL_CHUNK_TARGET_BYTES: usize = 96 * 1024;
const VIRTUAL_CHUNK_TARGET_IMAGES: usize = 4;
const ESTIMATED_IMAGE_HEIGHT: f32 = 480.0;

#[derive(Debug, Clone)]
struct PreviewChunk {
    html: Arc<str>,
    block_id: u64,
    content_hash: u64,
    source_start: f32,
    source_end: f32,
    source_anchors: Arc<[f32]>,
    estimated_height: f32,
    heading_start: Option<usize>,
    heading_end: Option<usize>,
}

#[derive(Debug, Clone)]
struct BodyPatch {
    start: usize,
    delete_count: usize,
    insert_count: usize,
    old_block_count: usize,
    anchor_map: Vec<Option<usize>>,
    block_ids: Vec<u64>,
}

impl PreviewDocument {
    #[allow(dead_code)]
    pub fn total_bytes(&self) -> usize {
        self.total_bytes
    }

    /// Number of virtual chunks currently represented by this preview.
    /// A zero value means the document uses the ordinary single-body path.
    #[allow(dead_code)]
    pub fn virtual_chunk_count(&self) -> usize {
        self.chunks.len()
    }

    /// Stable IDs for top-level Markdown blocks in document order.
    ///
    /// The IDs are derived from block content (with duplicate ordinals), not
    /// from the block's current index, so callers can retain a target while
    /// unrelated blocks are inserted or removed.
    #[allow(dead_code)]
    pub fn top_level_block_ids(&self) -> &[u64] {
        &self.block_ids
    }

    #[allow(dead_code)]
    pub fn block_index(&self) -> &[crate::markdown::BlockIndexEntry] {
        &self.block_index
    }

    pub fn update_stats(&self) -> PreviewUpdateStats {
        self.update_stats
    }

    /// Counts virtual chunks whose identity or rendered content changed.
    #[allow(dead_code)]
    pub fn changed_virtual_chunk_count(&self, next: &Self) -> usize {
        self.chunks
            .iter()
            .zip(next.chunks.iter())
            .filter(|(old, new)| {
                old.block_id != new.block_id || old.content_hash != new.content_hash
            })
            .count()
            + self.chunks.len().abs_diff(next.chunks.len())
    }

    fn can_patch_body_into(&self, next: &Self) -> bool {
        self.body_range.is_some()
            && next.body_range.is_some()
            && !self.blocks.is_empty()
            && !next.blocks.is_empty()
            && self.chrome_hash == next.chrome_hash
            && self.has_mermaid == next.has_mermaid
    }

    fn can_patch_virtual_into(&self, next: &Self) -> bool {
        self.virtual_manifest.is_some()
            && next.virtual_manifest.is_some()
            && self.chrome_hash == next.chrome_hash
            && self.has_mermaid == next.has_mermaid
    }

    fn can_patch_into(&self, next: &Self) -> bool {
        self.can_patch_body_into(next) || self.can_patch_virtual_into(next)
    }

    fn source_anchors(&self) -> Vec<f32> {
        let mut anchors = self
            .blocks
            .iter()
            .map(|block| block.source_start)
            .collect::<Vec<_>>();
        if let Some(last) = self.blocks.last() {
            anchors.push(last.source_end);
        }
        anchors
    }

    fn body_patch_into(&self, next: &Self) -> Option<BodyPatch> {
        if !self.can_patch_body_into(next) {
            return None;
        }

        let old_len = self.blocks.len();
        let new_len = next.blocks.len();
        let common_len = old_len.min(new_len);
        let mut start = 0;
        while start < common_len
            && preview_block_content(self.blocks[start].html.as_ref())
                == preview_block_content(next.blocks[start].html.as_ref())
        {
            start += 1;
        }

        let mut suffix = 0;
        while suffix < old_len.saturating_sub(start)
            && suffix < new_len.saturating_sub(start)
            && preview_block_content(self.blocks[old_len - 1 - suffix].html.as_ref())
                == preview_block_content(next.blocks[new_len - 1 - suffix].html.as_ref())
        {
            suffix += 1;
        }

        let mut used = vec![false; new_len];
        let mut anchor_map = Vec::with_capacity(old_len + 1);
        for (old_index, old_block) in self.blocks.iter().enumerate() {
            let same_position = (old_index < new_len).then_some(old_index).filter(|index| {
                !used[*index]
                    && old_block.block_id == next.blocks[*index].block_id
                    && old_block.content_hash == next.blocks[*index].content_hash
            });
            let match_index = same_position.or_else(|| {
                next.blocks
                    .iter()
                    .enumerate()
                    .find(|(index, block)| {
                        !used[*index] && block.content_hash == old_block.content_hash
                    })
                    .map(|(index, _)| index)
            });
            if let Some(index) = match_index {
                used[index] = true;
            }
            anchor_map.push(match_index);
        }
        anchor_map.push(Some(new_len));

        Some(BodyPatch {
            start,
            delete_count: old_len - start - suffix,
            insert_count: new_len - start - suffix,
            old_block_count: old_len,
            anchor_map,
            block_ids: next.blocks[start..start + (new_len - start - suffix)]
                .iter()
                .map(|block| block.block_id)
                .collect(),
        })
    }
}

#[derive(Default)]
struct ScrollBridge {
    source_position: Option<f32>,
    user_source_position: Option<f32>,
    user_anchor: Option<ScrollAnchor>,
    dropped_paths: Vec<PathBuf>,
    ready: Option<WebViewReady>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ScrollAnchor {
    pub block_id: u64,
    /// Normalized position inside the block (0 = top, 1 = bottom).
    pub offset: f32,
    pub source_position: f32,
}

#[derive(Debug, Clone)]
pub struct WebViewReady {
    pub content_height: f32,
    pub viewport_height: f32,
    pub element_count: usize,
    pub error: Option<String>,
}

const SCROLL_SYNC_SCRIPT: &str = r#"
(() => {
  let suppressUntil = performance.now() + 300;
  let scheduled = false;
  let animationFrame = 0;
  let targetSource = 0;
  let anchorCache = null;
  let navigationRevision = 0;
  let searchRevision = 0;

  const beginNavigation = () => {
    navigationRevision += 1;
    window.__mdNavigationRevision = navigationRevision;
  };

  const cancelSourceAnimation = () => {
    if (animationFrame) window.cancelAnimationFrame(animationFrame);
    animationFrame = 0;
  };

  const anchors = () => {
    if (anchorCache) return anchorCache;
    anchorCache = [];
    const walker = document.createTreeWalker(document.body, NodeFilter.SHOW_COMMENT);
    while (walker.nextNode()) {
      const match = /^md-editor-source:([0-9.]+)$/.exec(walker.currentNode.nodeValue || '');
      if (match) anchorCache.push({ source: Number(match[1]), node: walker.currentNode });
    }
    return anchorCache;
  };

  const blockMarkers = () => {
    const result = new Map();
    const walker = document.createTreeWalker(document.body, NodeFilter.SHOW_COMMENT);
    while (walker.nextNode()) {
      const match = /^md-editor-block:(\d+)$/.exec(walker.currentNode.nodeValue || '');
      if (match) result.set(match[1], walker.currentNode);
    }
    return result;
  };

  const elementAfter = (node) => {
    let sibling = node?.nextSibling;
    while (sibling && sibling.nodeType !== Node.ELEMENT_NODE) sibling = sibling.nextSibling;
    return sibling;
  };

  const isBoundaryComment = (node) => node?.nodeType === Node.COMMENT_NODE
    && /^(md-editor-source|md-editor-block):/.test(node.nodeValue || '');

  const replaceBlock = (id, html, sourceStart, sourceEnd) => {
    const marker = blockMarkers().get(String(id));
    if (!marker) return false;
    const boundary = (() => {
      let node = marker.nextSibling;
      while (node && !isBoundaryComment(node)) node = node.nextSibling;
      return node;
    })();
    const template = document.createElement('template');
    template.innerHTML = String(html || '');
    const fragment = template.content.cloneNode(true);
    [...fragment.childNodes].forEach((node) => {
      if (isBoundaryComment(node)) node.remove();
    });
    const range = document.createRange();
    range.setStartAfter(marker);
    if (boundary) range.setEndBefore(boundary);
    else range.setEndAfter(marker.parentNode.lastChild || marker);
    range.deleteContents();
    range.insertNode(fragment);
    let source = marker.previousSibling;
    while (source && source.nodeType !== Node.COMMENT) source = source.previousSibling;
    if (source && /^md-editor-source:[0-9.]+$/.test(source.nodeValue || '') && Number.isFinite(Number(sourceStart))) {
      source.nodeValue = `md-editor-source:${sourceStart}`;
    }
    // Keep the full source range in the public contract even though the end
    // marker is refreshed from the complete anchor list below.
    void sourceEnd;
    anchorCache = null;
    return true;
  };

  window.__mdEditorReplaceBlock = replaceBlock;

  const blockAnchorForY = (y) => {
    const markers = [...blockMarkers().entries()];
    let selected = null;
    for (const [blockId, marker] of markers) {
      const element = elementAfter(marker);
      if (!element) continue;
      const top = Math.max(0, element.getBoundingClientRect().top + window.scrollY);
      if (top <= y + 1) {
        const height = Math.max(1, element.getBoundingClientRect().height);
        selected = { blockId, offset: Math.min(1, Math.max(0, (y - top) / height)) };
      } else if (selected) {
        break;
      }
    }
    return selected;
  };

  const maxScroll = () => Math.max(0, document.documentElement.scrollHeight - window.innerHeight);

  const anchorY = (anchor) => {
    let sibling = anchor.node.nextSibling;
    while (sibling && sibling.nodeType !== Node.ELEMENT_NODE) sibling = sibling.nextSibling;
    if (!sibling) return maxScroll();
    return Math.min(maxScroll(), Math.max(0, sibling.getBoundingClientRect().top + window.scrollY));
  };

  const interpolate = (value, aValue, bValue, aResult, bResult) => {
    if (bValue <= aValue) return aResult;
    const t = Math.min(1, Math.max(0, (value - aValue) / (bValue - aValue)));
    return aResult + (bResult - aResult) * t;
  };

  const yForSource = (source) => {
    if (window.__mdVirtualPreview) return window.__mdVirtualPreview.yForSource(source);
    const list = anchors();
    if (!list.length) return 0;
    let low = 0;
    let high = list.length;
    while (low < high) {
      const mid = (low + high) >> 1;
      if (list[mid].source <= source) low = mid + 1;
      else high = mid;
    }
    const a = list[Math.max(0, low - 1)];
    const b = list[Math.min(list.length - 1, low)];
    return interpolate(source, a.source, b.source, anchorY(a), anchorY(b));
  };

  const sourceForY = (y) => {
    if (window.__mdVirtualPreview) return window.__mdVirtualPreview.sourceForY(y);
    const list = anchors();
    if (!list.length) return 0;
    let low = 0;
    let high = list.length;
    while (low < high) {
      const mid = (low + high) >> 1;
      if (anchorY(list[mid]) <= y) low = mid + 1;
      else high = mid;
    }
    const a = list[Math.max(0, low - 1)];
    const b = list[Math.min(list.length - 1, low)];
    return interpolate(y, anchorY(a), anchorY(b), a.source, b.source);
  };

  const captureViewportAnchor = () => {
    const block = blockAnchorForY(window.scrollY);
    if (block) return { ...block, source: sourceForY(window.scrollY) };
    const list = anchors();
    if (!list.length) return null;
    let index = 0;
    for (let candidate = 1; candidate < list.length; candidate += 1) {
      if (anchorY(list[candidate]) > window.scrollY + 1) break;
      index = candidate;
    }
    return { index, offset: anchorY(list[index]) - window.scrollY };
  };

  const restoreViewportAnchor = (saved, patch = null) => {
    if (!saved) return false;
    if (saved.blockId !== undefined) {
      const marker = blockMarkers().get(String(saved.blockId));
      const element = elementAfter(marker);
      if (element) {
        const top = element.getBoundingClientRect().top + window.scrollY;
        const offset = Math.min(1, Math.max(0, Number(saved.offset) || 0));
        window.scrollTo(0, Math.max(0, top + offset * Math.max(1, element.getBoundingClientRect().height)));
        return true;
      }
    }
    const list = anchors();
    const mappedIndex = mapPatchedAnchorIndex(saved.index, patch, Math.max(0, list.length - 1));
    const anchor = list[Math.min(mappedIndex, Math.max(0, list.length - 1))];
    if (!anchor) return false;
    window.scrollTo(0, Math.max(0, anchorY(anchor) - saved.offset));
    return true;
  };

  const placeSearchAnchor = (sourcePosition, backwards) => {
    const selection = window.getSelection();
    if (!selection) return;
    selection.removeAllRanges();
    if (!Number.isFinite(sourcePosition)) return;
    const walker = document.createTreeWalker(document.body, NodeFilter.SHOW_COMMENT);
    let candidate = null;
    while (walker.nextNode()) {
      const match = /^md-editor-source:([0-9.]+)$/.exec(walker.currentNode.nodeValue || '');
      if (!match || Number(match[1]) > sourcePosition) break;
      candidate = walker.currentNode;
    }
    if (!candidate) return;
    let element = candidate.nextSibling;
    while (element && element.nodeType !== Node.ELEMENT_NODE) element = element.nextSibling;
    if (!element) return;
    const range = document.createRange();
    range.selectNodeContents(element);
    range.collapse(!backwards);
    selection.addRange(range);
  };

  const report = () => {
    scheduled = false;
    const sourcePosition = sourceForY(window.scrollY);
    const source = performance.now() > suppressUntil ? 'user' : 'program';
    const block = blockAnchorForY(window.scrollY);
    if (block) {
      window.name = `md-anchor:${block.blockId}:${block.offset}:${sourcePosition}`;
      window.ipc.postMessage(`md-anchor:${source}:${block.blockId}:${block.offset}:${sourcePosition}`);
    } else {
      window.name = `md-source:${sourcePosition}`;
      window.ipc.postMessage(`md-source:${source}:${sourcePosition}`);
    }
  };

  window.__mdEditorSuppressScroll = (milliseconds) => {
    suppressUntil = Math.max(suppressUntil, performance.now() + milliseconds);
  };

  window.__mdEditorScrollHeading = async (index) => {
    beginNavigation();
    cancelSourceAnimation();
    suppressUntil = Math.max(suppressUntil, performance.now() + 700);
    if (window.__mdVirtualPreview) {
      await window.__mdVirtualPreview.scrollHeading(index);
    } else {
      document.getElementById(`md-heading-${index}`)?.scrollIntoView({ behavior: 'smooth', block: 'start' });
    }
  };

  const animateToTarget = () => {
    const target = yForSource(targetSource);
    const distance = target - window.scrollY;
    if (Math.abs(distance) < 0.75) {
      window.scrollTo(0, target);
      animationFrame = 0;
      return;
    }
    window.scrollTo(0, window.scrollY + distance * 0.38);
    animationFrame = window.requestAnimationFrame(animateToTarget);
  };

  window.__mdEditorSetSourcePosition = (value, smooth = true) => {
    beginNavigation();
    const sourcePosition = Math.max(0, Number(value) || 0);
    targetSource = sourcePosition;
    suppressUntil = performance.now() + (smooth ? 600 : 180);
    window.name = `md-source:${sourcePosition}`;
    if (!smooth) {
      cancelSourceAnimation();
      window.scrollTo(0, yForSource(sourcePosition));
    } else if (!animationFrame) {
      animationFrame = window.requestAnimationFrame(animateToTarget);
    }
  };

  window.__mdEditorSetBlockAnchor = (blockId, offset, fallbackSource = 0, smooth = true) => {
    beginNavigation();
    const marker = blockMarkers().get(String(blockId));
    const element = elementAfter(marker);
    const normalizedOffset = Math.min(1, Math.max(0, Number(offset) || 0));
    if (!element) {
      window.__mdEditorSetSourcePosition?.(fallbackSource, smooth);
      return;
    }
    const top = element.getBoundingClientRect().top + window.scrollY;
    targetSource = Number(fallbackSource) || 0;
    const target = top + normalizedOffset * Math.max(1, element.getBoundingClientRect().height);
    suppressUntil = performance.now() + (smooth ? 600 : 180);
    window.name = `md-anchor:${blockId}:${normalizedOffset}:${targetSource}`;
    if (!smooth) {
      cancelSourceAnimation();
      window.scrollTo(0, Math.max(0, target));
    } else {
      cancelSourceAnimation();
      const animate = () => {
        const distance = target - window.scrollY;
        if (Math.abs(distance) < 0.75) {
          window.scrollTo(0, target);
          animationFrame = 0;
          return;
        }
        window.scrollTo(0, window.scrollY + distance * 0.38);
        animationFrame = window.requestAnimationFrame(animate);
      };
      animationFrame = window.requestAnimationFrame(animate);
    }
  };

  window.__mdEditorSetFontSize = (value, baseValue = 15) => {
    const base = Math.max(1, Number(baseValue) || 15);
    const target = Math.max(1, Number(value) || base);
    document.documentElement.style.setProperty('--md-body-font-size', `${target}px`);
    document.documentElement.style.setProperty('--md-font-scale', String(target / base));
  };

  window.__mdEditorFindText = async (query, sourcePosition, backwards = false) => {
    const requestRevision = ++searchRevision;
    const needle = String(query || '');
    if (!needle) {
      window.getSelection()?.removeAllRanges();
      return;
    }
    if (window.__mdVirtualPreview && Number.isFinite(sourcePosition)) {
      await window.__mdVirtualPreview.loadSource(sourcePosition);
    }
    placeSearchAnchor(sourcePosition, backwards);
    if (Number.isFinite(sourcePosition)) {
      window.__mdEditorSetSourcePosition(sourcePosition, true);
    }
    await new Promise((resolve) => requestAnimationFrame(() => requestAnimationFrame(resolve)));
    if (requestRevision !== searchRevision) return;
    if (typeof window.find === 'function') {
      window.find(needle, false, !!backwards, true, false, false, false);
    }
  };

  const mapPatchedAnchorIndex = (index, patch, newBlockCount) => {
    if (!patch) return Math.min(index, newBlockCount);
    if (Array.isArray(patch.anchorMap) && Number.isInteger(patch.anchorMap[index])) {
      return Math.min(patch.anchorMap[index], newBlockCount);
    }
    const oldSuffixStart = patch.start + patch.deleteCount;
    if (index < patch.start) return index;
    if (index >= oldSuffixStart) {
      return patch.start + patch.insertCount + (index - oldSuffixStart);
    }
    return Math.min(patch.start, newBlockCount);
  };

  const replaceBody = async (revision) => {
    const response = await fetch(`/body?revision=${encodeURIComponent(revision)}`, { cache: 'no-store' });
    if (!response.ok) throw new Error(`preview body request failed: ${response.status}`);
    document.body.innerHTML = await response.text();
    anchorCache = null;
  };

  window.__mdEditorPatchBody = async (revision, patch = null) => {
    const patchRevision = ++navigationRevision;
    cancelSourceAnimation();
    suppressUntil = performance.now() + 500;
    const match = /^md-source:([0-9.]+)$/.exec(window.name || '');
    const saved = match ? Number(match[1]) : 0;
    const savedVirtualViewportAnchor = window.__mdVirtualPreview?.captureViewportAnchor?.() || null;
    const savedViewportAnchor = savedVirtualViewportAnchor ? null : captureViewportAnchor();
    let blockPatched = false;
    let virtualPatched = false;
    try {
      if (window.__mdVirtualPreview) {
        virtualPatched = await window.__mdVirtualPreview.patch(
          revision,
          saved,
          savedVirtualViewportAnchor,
        );
      } else {
        if (patchRevision !== navigationRevision) return;
        let idPatched = false;
        if (patch && Array.isArray(patch.blockIds)
            && patch.deleteCount === patch.insertCount
            && patch.blockIds.length === patch.insertCount
            && patch.blockIds.length > 0) {
          const ids = patch.blockIds.join(',');
          const response = await fetch(`/blocks?revision=${encodeURIComponent(revision)}&ids=${encodeURIComponent(ids)}`, { cache: 'no-store' });
          if (response.ok) {
            const payload = await response.json();
            const blocks = Array.isArray(payload) ? payload : payload.blocks;
            idPatched = Array.isArray(blocks) && blocks.length === patch.blockIds.length
              && blocks.every((block) => replaceBlock(
                block.id,
                block.html,
                block.sourceStart,
                block.sourceEnd,
              ));
            if (idPatched) {
              const sourceAnchors = Array.isArray(payload) ? null : payload.sourceAnchors;
              if (sourceAnchors) {
                const refreshed = anchors();
                for (let index = 0; index < Math.min(refreshed.length, sourceAnchors.length); index += 1) {
                  refreshed[index].node.nodeValue = `md-editor-source:${sourceAnchors[index]}`;
                }
              }
              blockPatched = true;
            }
          }
        }
        if (idPatched) {
          if (window.__mdRenderMermaid) await window.__mdRenderMermaid(document);
        } else {
        const blockQuery = patch
          ? `&start=${encodeURIComponent(patch.start)}&count=${encodeURIComponent(patch.insertCount)}`
          : '';
        const blocksResponse = await fetch(`/blocks?revision=${encodeURIComponent(revision)}${blockQuery}`, { cache: 'no-store' });
        if (blocksResponse.ok) {
          const blockPayload = await blocksResponse.json();
          const blocks = Array.isArray(blockPayload) ? blockPayload : blockPayload.blocks;
          const sourceAnchors = Array.isArray(blockPayload) ? null : blockPayload.sourceAnchors;
          const current = anchors();
          const patchStart = patch?.start ?? 0;
          const patchDeleteCount = patch?.deleteCount ?? current.length - 1;
          const oldBlockCount = patch?.oldBlockCount ?? current.length - 1;
          if (current.length === oldBlockCount + 1
              && current.length >= patchStart + patchDeleteCount + 1) {
            const start = current[patchStart]?.node;
            const end = current[patchStart + patchDeleteCount]?.node;
            if (!start || !end) throw new Error('preview block anchors missing');
            const range = document.createRange();
            range.setStartBefore(start);
            range.setEndBefore(end);
            range.deleteContents();
            const template = document.createElement('template');
            template.innerHTML = blocks.map((block) => block.html).join('');
            range.insertNode(template.content.cloneNode(true));
            anchorCache = null;
            if (sourceAnchors) {
              const refreshed = anchors();
              for (let index = 0; index < Math.min(refreshed.length, sourceAnchors.length); index += 1) {
                refreshed[index].node.nodeValue = `md-editor-source:${sourceAnchors[index]}`;
              }
            }
            anchorCache = null;
            blockPatched = true;
          } else {
            await replaceBody(revision);
          }
        } else {
          await replaceBody(revision);
        }
        if (window.__mdRenderMermaid) await window.__mdRenderMermaid(document);
        }
      }
      await document.fonts.ready;
      await new Promise((resolve) => requestAnimationFrame(() => requestAnimationFrame(resolve)));
      if (patchRevision !== navigationRevision) return;
      const restoredViewportAnchor = virtualPatched
        || (blockPatched && restoreViewportAnchor(savedViewportAnchor, patch));
      window.name = `md-source:${saved}`;
      window.ipc.postMessage(`md-source:program:${saved}`);
      const height = Math.max(document.body.scrollHeight, document.documentElement.scrollHeight);
      window.ipc.postMessage(`md-ready:${height}:${window.innerHeight}:${document.body.getElementsByTagName('*').length}`);
      if (!restoredViewportAnchor) window.__mdEditorSetSourcePosition(saved, false);
    } catch (error) {
      window.ipc.postMessage(`md-patch-error:${error?.stack || error?.message || String(error)}`);
    }
  };

  window.addEventListener('scroll', () => {
    if (!scheduled) {
      scheduled = true;
      window.requestAnimationFrame(report);
    }
  }, { passive: true });

  window.addEventListener('load', () => {
    if (window.__mdVirtualPreview) return;
    suppressUntil = performance.now() + 300;
    const match = /^md-source:([0-9.]+)$/.exec(window.name);
    const saved = match ? Number(match[1]) : 0;
    const restoreRevision = navigationRevision;
    window.requestAnimationFrame(() => {
      window.requestAnimationFrame(() => {
        if (saved <= 0) window.scrollTo(0, 0);
        window.name = `md-source:${saved}`;
        window.ipc.postMessage(`md-source:program:${saved}`);
        document.fonts.ready.then(() => {
          window.requestAnimationFrame(() => {
            const height = Math.max(document.body.scrollHeight, document.documentElement.scrollHeight);
            window.ipc.postMessage(`md-ready:${height}:${window.innerHeight}:${document.body.getElementsByTagName('*').length}`);
            if (saved > 0) {
              const restore = () => {
                if (restoreRevision !== navigationRevision) return;
                window.__mdEditorSetSourcePosition(saved, false);
              };
              if ('requestIdleCallback' in window) window.requestIdleCallback(restore);
              else window.setTimeout(restore, 0);
            }
          });
        });
      });
    });
  });
})();
"#;

const VIRTUAL_PREVIEW_SCRIPT: &str = r#"
(() => {
  const manifestNode = document.getElementById('md-virtual-manifest');
  if (!manifestNode) return;
  let chunks = JSON.parse(manifestNode.textContent || '[]');
  let revision = new URL(location.href).searchParams.get('revision') || '';
  let updateRevision = 0;
  let scheduled = false;

  const placeholderFor = (chunk) => {
    const node = document.createElement('div');
    node.className = 'md-virtual-placeholder';
    node.dataset.chunk = String(chunk.index);
    node.style.cssText = `display:block;height:${chunk.height}px;margin:0;padding:0;border:0;contain:strict;content-visibility:auto;`;
    chunk.placeholder = node;
    return node;
  };

  const edge = (kind, index) => {
    const node = document.createElement('div');
    node.dataset.mdChunkEdge = `${kind}:${index}`;
    node.style.cssText = 'display:block;height:0!important;min-height:0!important;margin:0!important;padding:0!important;border:0!important;overflow:hidden!important;';
    return node;
  };

  const chunkTop = (chunk) => {
    const node = chunk.placeholder || chunk.start;
    return node ? node.offsetTop : 0;
  };

  const findBySource = (source) => {
    let low = 0;
    let high = chunks.length;
    while (low < high) {
      const mid = (low + high) >> 1;
      if (chunks[mid].sourceEnd <= source) low = mid + 1;
      else high = mid;
    }
    return chunks[Math.min(chunks.length - 1, low)] || null;
  };

  const findByY = (y) => {
    let low = 0;
    let high = chunks.length;
    while (low < high) {
      const mid = (low + high) >> 1;
      if (chunkTop(chunks[mid]) + chunks[mid].height <= y) low = mid + 1;
      else high = mid;
    }
    return chunks[Math.min(chunks.length - 1, low)] || null;
  };

  const yForSource = (source) => {
    const chunk = findBySource(source);
    if (!chunk) return 0;
    const span = Math.max(1, chunk.sourceEnd - chunk.sourceStart);
    const ratio = Math.min(1, Math.max(0, (source - chunk.sourceStart) / span));
    return chunkTop(chunk) + ratio * chunk.height;
  };

  const sourceForY = (y) => {
    const chunk = findByY(y);
    if (!chunk) return 0;
    const ratio = Math.min(1, Math.max(0, (y - chunkTop(chunk)) / Math.max(1, chunk.height)));
    return chunk.sourceStart + ratio * (chunk.sourceEnd - chunk.sourceStart);
  };

  const captureVirtualViewportAnchor = () => {
    const chunk = findByY(window.scrollY);
    if (!chunk) return null;
    const loaded = loadedBlockElements(chunk);
    for (const [blockId, element] of loaded) {
      const rect = element.getBoundingClientRect();
      const top = rect.top + window.scrollY;
      const bottom = rect.bottom + window.scrollY;
      if (bottom > window.scrollY && top <= window.scrollY + 1) {
        return {
          blockId,
          offset: Math.min(1, Math.max(0, (window.scrollY - top) / Math.max(1, rect.height))),
          source: sourceForY(window.scrollY),
        };
      }
    }
    return {
      blockId: chunk.blockId,
      offset: window.scrollY - chunkTop(chunk),
      source: sourceForY(window.scrollY),
    };
  };

  const restoreVirtualViewportAnchor = async (saved) => {
    if (!saved) return false;
    const chunk = chunks.find((candidate) => String(candidate.blockId) === String(saved.blockId)
      || candidate.blockIds?.some((blockId) => String(blockId) === String(saved.blockId)));
    if (!chunk) return false;
    await load(chunk);
    await new Promise((resolve) => requestAnimationFrame(() => requestAnimationFrame(resolve)));
    const element = loadedBlockElements(chunk).get(String(saved.blockId));
    if (element) {
      const rect = element.getBoundingClientRect();
      const top = rect.top + window.scrollY;
      window.scrollTo(0, Math.max(0, top + Number(saved.offset || 0) * Math.max(1, rect.height)));
      return true;
    }
    window.scrollTo(0, Math.max(0, chunkTop(chunk) + Number(saved.offset || 0)));
    return true;
  };

  const scrollHeading = async (index) => {
    const chunk = chunks.find((chunk) => chunk.headingStart !== null && index >= chunk.headingStart && index < chunk.headingEnd);
    if (!chunk) return;
    await load(chunk);
    document.getElementById(`md-heading-${index}`)?.scrollIntoView({ behavior: 'smooth', block: 'start' });
  };

  const unload = (chunk) => {
    if (!chunk.start || !chunk.end) return;
    const placeholder = placeholderFor(chunk);
    chunk.start.before(placeholder);
    let node = chunk.start;
    while (node) {
      const next = node.nextSibling;
      const finished = node === chunk.end;
      node.remove();
      if (finished) break;
      node = next;
    }
    chunk.start = null;
    chunk.end = null;
  };

  const discard = (chunk) => {
    chunk.placeholder?.remove();
    chunk.placeholder = null;
    if (!chunk.start || !chunk.end) return;
    let node = chunk.start;
    while (node) {
      const next = node.nextSibling;
      const finished = node === chunk.end;
      node.remove();
      if (finished) break;
      node = next;
    }
    chunk.start = null;
    chunk.end = null;
  };

  const nodeForChunk = (chunk) => chunk?.placeholder || chunk?.start || anchor;

  const refreshChunkAnchors = (chunk) => {
    if (!chunk.start || !chunk.end || !Array.isArray(chunk.sourceAnchors)) return;
    let sourceIndex = 0;
    let node = chunk.start;
    while (node) {
      if (node.nodeType === Node.COMMENT && /^md-editor-source:[0-9.]+$/.test(node.nodeValue || '')) {
        if (sourceIndex < chunk.sourceAnchors.length) {
          node.nodeValue = `md-editor-source:${chunk.sourceAnchors[sourceIndex]}`;
        }
        sourceIndex += 1;
      }
      if (node === chunk.end) break;
      node = node.nextSibling;
    }
  };

  const blockIdFromComment = (node) => {
    if (!node || node.nodeType !== Node.COMMENT_NODE) return null;
    const match = /^md-editor-block:(\d+)$/.exec(node.nodeValue || '');
    return match ? match[1] : null;
  };

  const nextElementSibling = (node, boundary) => {
    let current = node?.nextSibling || null;
    while (current && current !== boundary) {
      if (current.nodeType === Node.ELEMENT_NODE) return current;
      current = current.nextSibling;
    }
    return null;
  };

  const loadedBlockElements = (chunk) => {
    const elements = new Map();
    if (!chunk?.start || !chunk?.end) return elements;
    let node = chunk.start.nextSibling;
    while (node && node !== chunk.end) {
      const id = blockIdFromComment(node);
      if (id !== null) {
        const element = nextElementSibling(node, chunk.end);
        if (element) elements.set(id, element);
      }
      node = node.nextSibling;
    }
    return elements;
  };

  const templateBlockElements = (template) => {
    const elements = new Map();
    const walker = document.createTreeWalker(template.content, NodeFilter.SHOW_COMMENT);
    let node = walker.nextNode();
    while (node) {
      const id = blockIdFromComment(node);
      if (id !== null) {
        const element = nextElementSibling(node, null);
        if (element) elements.set(id, element);
      }
      node = walker.nextNode();
    }
    return elements;
  };

  const patchLoadedChunkBlocks = async (current, next) => {
    if (!current?.start || !current.end
        || !Array.isArray(current.blockIds) || !Array.isArray(next.blockIds)
        || !Array.isArray(current.blockHashes) || !Array.isArray(next.blockHashes)
        || current.blockIds.length !== next.blockIds.length
        || current.blockHashes.length !== next.blockHashes.length
        || current.blockIds.some((id, index) => String(id) !== String(next.blockIds[index]))) {
      return false;
    }
    const chunkUrl = new URL(`/chunk/${next.index}?revision=${encodeURIComponent(revision)}`, location.origin);
    const response = await fetch(chunkUrl, { cache: 'no-store' });
    if (!response.ok) return false;
    const template = document.createElement('template');
    template.innerHTML = await response.text();
    const oldElements = loadedBlockElements(current);
    const newElements = templateBlockElements(template);
    if (oldElements.size !== current.blockIds.length || newElements.size !== next.blockIds.length) {
      return false;
    }
    for (let index = 0; index < next.blockIds.length; index += 1) {
      if (String(current.blockHashes[index]) === String(next.blockHashes[index])) continue;
      const id = String(next.blockIds[index]);
      const oldElement = oldElements.get(id);
      const newElement = newElements.get(id);
      if (!oldElement || !newElement) return false;
      oldElement.replaceWith(newElement.cloneNode(true));
    }
    Object.assign(current, next);
    refreshChunkAnchors(current);
    if (window.__mdRenderMermaid) await window.__mdRenderMermaid(document);
    await new Promise((resolve) => requestAnimationFrame(resolve));
    current.height = Math.max(1, current.end.offsetTop - current.start.offsetTop);
    return true;
  };

  const load = (chunk) => {
    if (!chunk || chunk.start) return Promise.resolve();
    if (chunk.loading) return chunk.loading;
    chunk.loading = (async () => {
      try {
        const chunkUrl = new URL(`/chunk/${chunk.index}?revision=${encodeURIComponent(revision)}`, location.origin);
        const response = await fetch(chunkUrl, { cache: 'no-store' });
        if (!response.ok) return;
        const template = document.createElement('template');
        template.innerHTML = await response.text();
        const start = edge('start', chunk.index);
        const end = edge('end', chunk.index);
        const fragment = document.createDocumentFragment();
        fragment.append(start, template.content, end);
        chunk.placeholder.replaceWith(fragment);
        chunk.placeholder = null;
        chunk.start = start;
        chunk.end = end;
        if (window.__mdRenderMermaid) await window.__mdRenderMermaid(document);
        await new Promise((resolve) => requestAnimationFrame(resolve));
        chunk.height = Math.max(1, end.offsetTop - start.offsetTop);
        const saved = /^md-source:([0-9.]+)$/.exec(window.name || '');
        if (saved) {
          const source = Number(saved[1]);
          if (source >= chunk.sourceStart && source < chunk.sourceEnd) window.scrollTo(0, yForSource(source));
        }
      } finally {
        chunk.loading = null;
      }
    })();
    return chunk.loading;
  };

  const maintainWindow = () => {
    scheduled = false;
    const top = window.scrollY;
    const bottom = top + window.innerHeight;
    const loadMargin = window.innerHeight * 2.5;
    const keepMargin = window.innerHeight * 5;
    for (const chunk of chunks) {
      const chunkStart = chunkTop(chunk);
      const chunkEnd = chunkStart + chunk.height;
      if (chunkEnd >= top - loadMargin && chunkStart <= bottom + loadMargin) load(chunk);
      else if (chunk.start && (chunkEnd < top - keepMargin || chunkStart > bottom + keepMargin)) unload(chunk);
    }
  };

  const schedule = () => {
    if (!scheduled) {
      scheduled = true;
      requestAnimationFrame(maintainWindow);
    }
  };

  const scriptNode = document.currentScript;
  const anchor = document.createElement('span');
  anchor.id = 'md-virtual-anchor';
  anchor.hidden = true;
  manifestNode.before(anchor);
  for (const chunk of chunks) anchor.before(placeholderFor(chunk));
  manifestNode.remove();
  if (scriptNode) scriptNode.remove();

  const patch = async (nextRevision, savedSource, savedAnchor = null) => {
    const requestRevision = ++updateRevision;
    const manifestUrl = new URL(`/manifest?revision=${encodeURIComponent(nextRevision)}`, location.origin);
    const response = await fetch(manifestUrl, { cache: 'no-store' });
    if (!response.ok) throw new Error(`preview manifest request failed: ${response.status}`);
    const nextChunks = await response.json();
    if (requestRevision !== updateRevision) return false;
    revision = String(nextRevision || '');
    const sameLayout = nextChunks.length === chunks.length
      && nextChunks.every((next, index) => next.index === chunks[index].index
        && next.sourceAnchors.length === chunks[index].sourceAnchors.length);
    if (!sameLayout) {
      for (const chunk of chunks) discard(chunk);
      chunks = nextChunks;
      for (const chunk of chunks) anchor.before(placeholderFor(chunk));
    } else {
      const updated = [];
      for (let index = 0; index < nextChunks.length; index += 1) {
        const next = nextChunks[index];
        const current = chunks[index];
        if (String(next.blockId) === String(current.blockId)
            && String(next.contentHash) === String(current.contentHash)) {
          const measuredHeight = current.start ? current.height : next.height;
          Object.assign(current, next);
          current.height = measuredHeight;
          if (current.placeholder) current.placeholder.style.height = `${next.height}px`;
          refreshChunkAnchors(current);
          updated.push(current);
          continue;
        }
        if (await patchLoadedChunkBlocks(current, next)) {
          updated.push(current);
          continue;
        }
        const reference = nodeForChunk(chunks[index + 1]);
        discard(current);
        const replacement = { ...next };
        reference.before(placeholderFor(replacement));
        updated.push(replacement);
      }
      chunks = updated;
    }
    const restored = sameLayout && await restoreVirtualViewportAnchor(savedAnchor);
    if (!restored) {
      const target = findBySource(savedSource);
      if (target) await load(target);
      await new Promise((resolve) => requestAnimationFrame(() => requestAnimationFrame(resolve)));
      window.scrollTo(0, yForSource(savedSource));
    }
    schedule();
    return restored;
  };

  window.__mdVirtualPreview = {
    yForSource,
    sourceForY,
    scrollHeading,
    loadSource: (source) => load(findBySource(source)),
    captureViewportAnchor: captureVirtualViewportAnchor,
    restoreViewportAnchor: restoreVirtualViewportAnchor,
    patch,
  };
  window.addEventListener('scroll', schedule, { passive: true });
  window.addEventListener('resize', schedule, { passive: true });

  const boot = async () => {
    try {
      await Promise.all(chunks.slice(0, 1).map(load));
      await document.fonts.ready;
      await new Promise((resolve) => requestAnimationFrame(() => requestAnimationFrame(resolve)));
      const height = Math.max(document.body.scrollHeight, document.documentElement.scrollHeight);
      window.name = 'md-source:0';
      window.ipc.postMessage(`md-source:program:0`);
      window.ipc.postMessage(`md-ready:${height}:${window.innerHeight}:${document.body.getElementsByTagName('*').length}`);
      schedule();
    } catch (error) {
      window.ipc.postMessage(`md-ready-error:${error?.stack || error?.message || String(error)}`);
    }
  };
  if (document.readyState === 'complete') boot();
  else window.addEventListener('load', boot, { once: true });
})();
"#;

impl Default for BrowserPreview {
    fn default() -> Self {
        Self {
            webview: None,
            document_hash: 0,
            document_source: None,
            document_changed: false,
            bounds: None,
            visible: false,
            frozen_frame: None,
            scroll_bridge: Arc::new(Mutex::new(ScrollBridge::default())),
            document_payload: Arc::new(Mutex::new(None)),
            local_image_requests: Arc::new(AtomicUsize::new(0)),
            mermaid_runtime_requests: Arc::new(AtomicUsize::new(0)),
            font_asset_requests: Arc::new(std::array::from_fn(|_| AtomicUsize::new(0))),
            applied_font_size: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreviewApplyKind {
    Created,
    Patched,
    Navigated,
    Unchanged,
}

impl BrowserPreview {
    pub fn show(
        &mut self,
        frame: &eframe::Frame,
        ctx: &egui::Context,
        rect: egui::Rect,
        pixels_per_point: f32,
        document: &Arc<PreviewDocument>,
    ) -> Result<PreviewApplyKind, String> {
        let bounds = physical_bounds(rect, pixels_per_point);
        let source_changed = self
            .document_source
            .as_ref()
            .is_none_or(|current| !Arc::ptr_eq(current, document));
        let document_hash = document.hash;

        if self.webview.is_none() {
            self.reset_scroll_bridge_for_document();
            self.store_document(document);
            let window = frame
                .winit_window()
                .ok_or_else(|| "当前窗口后端不支持浏览器预览".to_string())?;
            let builder = WebViewBuilder::new();
            #[cfg(target_os = "windows")]
            let builder = builder
                .with_https_scheme(true)
                .with_browser_accelerator_keys(false)
                .with_scroll_bar_style(ScrollBarStyle::FluentOverlay);
            let scroll_bridge = Arc::clone(&self.scroll_bridge);
            let ipc_repaint_ctx = ctx.clone();
            let drop_bridge = Arc::clone(&self.scroll_bridge);
            let drop_repaint_ctx = ctx.clone();
            let document_payload = Arc::clone(&self.document_payload);
            let local_image_requests = Arc::clone(&self.local_image_requests);
            let mermaid_runtime_requests = Arc::clone(&self.mermaid_runtime_requests);
            let font_asset_requests = Arc::clone(&self.font_asset_requests);
            let navigation_ctx = ctx.clone();
            let webview = builder
                .with_custom_protocol("mdpreview".into(), move |_webview_id, request| {
                    preview_document_response(request, &document_payload)
                })
                .with_custom_protocol("mdfont".into(), move |_webview_id, request| {
                    let path = request.uri().path();
                    if path == "/mermaid.min.js" {
                        mermaid_runtime_requests.fetch_add(1, Ordering::Relaxed);
                    }
                    if let Some(index) = preview_font_asset_index(path) {
                        font_asset_requests[index].fetch_add(1, Ordering::Relaxed);
                    }
                    preview_asset_response(request)
                })
                .with_custom_protocol("mdfile".into(), move |_webview_id, request| {
                    local_image_requests.fetch_add(1, Ordering::Relaxed);
                    local_image_response(request)
                })
                .with_initialization_script(SCROLL_SYNC_SCRIPT)
                .with_ipc_handler(move |request| {
                    if let Some(ready) = parse_ready_message(request.body())
                        .or_else(|| parse_ready_error(request.body()))
                    {
                        if let Ok(mut bridge) = scroll_bridge.lock() {
                            bridge.ready = Some(ready);
                        }
                        ipc_repaint_ctx.request_repaint();
                    } else if let Some((anchor, user_initiated)) =
                        parse_anchor_message(request.body())
                    {
                        if let Ok(mut bridge) = scroll_bridge.lock() {
                            bridge.source_position = Some(anchor.source_position);
                            if user_initiated {
                                bridge.user_anchor = Some(anchor);
                            }
                        }
                        ipc_repaint_ctx.request_repaint();
                    } else if let Some((source_position, user_initiated)) =
                        parse_source_message(request.body())
                    {
                        if let Ok(mut bridge) = scroll_bridge.lock() {
                            bridge.source_position = Some(source_position);
                            if user_initiated {
                                bridge.user_source_position = Some(source_position);
                            }
                        }
                        ipc_repaint_ctx.request_repaint();
                    }
                })
                .with_drag_drop_handler(move |event| {
                    if let DragDropEvent::Drop { paths, .. } = event {
                        if let Ok(mut bridge) = drop_bridge.lock() {
                            bridge.dropped_paths.extend(paths);
                        }
                        drop_repaint_ctx.request_repaint();
                    }
                    // Prevent the browser engine from navigating to the dropped file.
                    true
                })
                .with_navigation_handler(move |url| {
                    let Ok(parsed) = url::Url::parse(&url) else {
                        return false;
                    };
                    match parsed.scheme() {
                        // Keep document and local image protocol requests inside
                        // the embedded preview. External links belong to the
                        // user's regular browser and must not replace the
                        // current document WebView.
                        "mdpreview" | "mdfile" => true,
                        "http" | "https" => {
                            navigation_ctx.open_url(egui::OpenUrl { url, new_tab: true });
                            false
                        }
                        _ => false,
                    }
                })
                .with_url(preview_document_url(document_hash))
                .with_bounds(to_wry_rect(bounds))
                .with_visible(true)
                .with_focused(false)
                .with_clipboard(true)
                .build_as_child(window)
                .map_err(|error| format!("无法创建浏览器预览：{error}"))?;
            self.webview = Some(webview);
            self.document_hash = document_hash;
            self.document_source = Some(Arc::clone(document));
            self.document_changed = true;
            self.bounds = Some(bounds);
            self.visible = true;
            return Ok(PreviewApplyKind::Created);
        }

        let body_patch = self
            .document_source
            .as_ref()
            .and_then(|current| current.body_patch_into(document));
        let patch_document = source_changed
            && self.document_source.as_ref().is_some_and(|current| {
                current.can_patch_into(document) || current.can_patch_virtual_into(document)
            });
        if source_changed {
            if !patch_document {
                self.reset_scroll_bridge_for_document();
            }
            self.store_document(document);
        }

        let webview = self.webview.as_ref().expect("webview 已初始化");
        if self.bounds != Some(bounds) {
            webview
                .set_bounds(to_wry_rect(bounds))
                .map_err(|error| format!("无法调整预览区域：{error}"))?;
            self.bounds = Some(bounds);
        }
        let apply_kind = if source_changed {
            let apply_kind = if patch_document {
                let block_ids = body_patch.as_ref().map(|patch| {
                    patch
                        .block_ids
                        .iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                });
                let patch_json = body_patch
                    .as_ref()
                    .map(|patch| {
                        serde_json::json!({
                            "start": patch.start,
                            "deleteCount": patch.delete_count,
                            "insertCount": patch.insert_count,
                            "oldBlockCount": patch.old_block_count,
                            "anchorMap": patch.anchor_map,
                            "blockIds": block_ids,
                        })
                    })
                    .map(|patch| patch.to_string())
                    .unwrap_or_else(|| "null".to_string());
                webview
                    .evaluate_script(&format!(
                        "window.__mdEditorPatchBody?.('{document_hash:016x}', {patch_json});"
                    ))
                    .map_err(|error| format!("无法更新浏览器预览内容：{error}"))?;
                PreviewApplyKind::Patched
            } else {
                webview
                    .load_url(&preview_document_url(document_hash))
                    .map_err(|error| format!("无法刷新浏览器预览：{error}"))?;
                PreviewApplyKind::Navigated
            };
            self.document_hash = document_hash;
            self.document_source = Some(Arc::clone(document));
            self.document_changed = true;
            apply_kind
        } else {
            PreviewApplyKind::Unchanged
        };
        if !self.visible {
            webview
                .set_visible(true)
                .map_err(|error| format!("无法显示浏览器预览：{error}"))?;
            self.visible = true;
        }
        Ok(apply_kind)
    }

    fn store_document(&self, document: &Arc<PreviewDocument>) {
        if let Ok(mut payload) = self.document_payload.lock() {
            *payload = Some(Arc::clone(document));
        }
    }

    fn reset_scroll_bridge_for_document(&self) {
        self.local_image_requests.store(0, Ordering::Relaxed);
        self.mermaid_runtime_requests.store(0, Ordering::Relaxed);
        for requests in self.font_asset_requests.iter() {
            requests.store(0, Ordering::Relaxed);
        }
        if let Ok(mut bridge) = self.scroll_bridge.lock() {
            bridge.source_position = None;
            bridge.user_source_position = None;
            bridge.user_anchor = None;
            bridge.ready = None;
        }
    }

    pub fn hide(&mut self) {
        if self.visible {
            if let Some(webview) = &self.webview {
                let _ = webview.set_visible(false);
                let _ = webview.focus_parent();
            }
            self.visible = false;
        }
    }

    /// Release the browser process group when preview is no longer part of the layout.
    /// `hide` remains separate because popup menus only need a temporary visual hide.
    pub fn close(&mut self) {
        self.hide();
        self.webview = None;
        self.document_hash = 0;
        self.document_source = None;
        self.document_changed = false;
        self.bounds = None;
        self.frozen_frame = None;
        self.applied_font_size = None;
        if let Ok(mut bridge) = self.scroll_bridge.lock() {
            *bridge = ScrollBridge::default();
        }
        if let Ok(mut payload) = self.document_payload.lock() {
            *payload = None;
        }
    }

    /// Native child WebViews always sit above egui's render surface on Windows.
    /// Freeze the current pixels before hiding the child so popup menus can be
    /// painted on top without turning the whole reading area blank.
    pub fn freeze_for_overlay(
        &mut self,
        frame: &eframe::Frame,
        ctx: &egui::Context,
        rect: egui::Rect,
        pixels_per_point: f32,
    ) {
        #[cfg(target_os = "windows")]
        {
            if self.frozen_frame.is_none()
                && self.visible
                && let Some(image) = capture_preview(frame, rect, pixels_per_point)
            {
                self.frozen_frame = Some(ctx.load_texture(
                    "browser-preview-frozen-frame",
                    image,
                    egui::TextureOptions::LINEAR,
                ));
            }
        }
        #[cfg(target_os = "macos")]
        let _ = (frame, ctx, rect, pixels_per_point);
        self.hide();
        #[cfg(target_os = "windows")]
        if let Some(texture) = &self.frozen_frame {
            ctx.layer_painter(egui::LayerId::new(
                egui::Order::Middle,
                egui::Id::new("browser-preview-frozen-layer"),
            ))
            .image(
                texture.id(),
                rect,
                egui::Rect::from_min_max(egui::Pos2::ZERO, egui::pos2(1.0, 1.0)),
                egui::Color32::WHITE,
            );
        }
    }

    pub fn discard_frozen_frame(&mut self) {
        self.frozen_frame = None;
    }

    pub fn focus_parent(&self) {
        if let Some(webview) = &self.webview {
            let _ = webview.focus_parent();
        }
    }

    pub fn take_document_changed(&mut self) -> bool {
        std::mem::take(&mut self.document_changed)
    }

    pub fn take_user_source_position(&mut self) -> Option<f32> {
        self.scroll_bridge
            .lock()
            .ok()
            .and_then(|mut bridge| bridge.user_source_position.take())
    }

    pub fn take_user_scroll_anchor(&mut self) -> Option<ScrollAnchor> {
        self.scroll_bridge
            .lock()
            .ok()
            .and_then(|mut bridge| bridge.user_anchor.take())
    }

    pub fn take_dropped_paths(&mut self) -> Vec<PathBuf> {
        self.scroll_bridge
            .lock()
            .map(|mut bridge| std::mem::take(&mut bridge.dropped_paths))
            .unwrap_or_default()
    }

    pub fn take_ready(&mut self) -> Option<WebViewReady> {
        self.scroll_bridge
            .lock()
            .ok()
            .and_then(|mut bridge| bridge.ready.take())
    }

    pub fn local_image_request_count(&self) -> usize {
        self.local_image_requests.load(Ordering::Relaxed)
    }

    pub fn mermaid_runtime_request_count(&self) -> usize {
        self.mermaid_runtime_requests.load(Ordering::Relaxed)
    }

    pub fn font_asset_request_counts(&self) -> [usize; 4] {
        std::array::from_fn(|index| self.font_asset_requests[index].load(Ordering::Relaxed))
    }

    pub fn font_asset_requested_bytes(&self) -> usize {
        let counts = self.font_asset_request_counts();
        counts
            .into_iter()
            .zip(preview_font_asset_sizes())
            .map(|(count, size)| count.saturating_mul(size))
            .sum()
    }

    pub fn source_position(&self) -> Option<f32> {
        self.scroll_bridge
            .lock()
            .ok()
            .and_then(|bridge| bridge.source_position)
    }

    pub fn scroll_to_source_position(
        &self,
        source_position: f32,
        smooth: bool,
    ) -> Result<(), String> {
        let Some(webview) = &self.webview else {
            return Err("浏览器预览尚未就绪".to_string());
        };
        webview
            .evaluate_script(&format!(
                "window.__mdEditorSetSourcePosition?.({:.8}, {});",
                source_position.max(0.0),
                if smooth { "true" } else { "false" }
            ))
            .map_err(|error| format!("无法同步预览滚动位置：{error}"))
    }

    pub fn scroll_to_block_anchor(
        &self,
        anchor: &ScrollAnchor,
        smooth: bool,
    ) -> Result<(), String> {
        let Some(webview) = &self.webview else {
            return Err("浏览器预览尚未就绪".to_string());
        };
        let block_id = serde_json::to_string(&anchor.block_id.to_string())
            .map_err(|error| format!("无法编码预览块 ID：{error}"))?;
        webview
            .evaluate_script(&format!(
                "window.__mdEditorSetBlockAnchor?.({}, {:.8}, {:.8}, {});",
                block_id,
                anchor.offset.clamp(0.0, 1.0),
                anchor.source_position.max(0.0),
                if smooth { "true" } else { "false" }
            ))
            .map_err(|error| format!("无法同步预览块位置：{error}"))
    }

    pub fn set_body_font_size(&mut self, size: f32, base_size: f32) -> Result<(), String> {
        let size = size.max(1.0);
        let base_size = base_size.max(1.0);
        let key = (size.to_bits(), base_size.to_bits());
        if self.applied_font_size == Some(key) {
            return Ok(());
        }
        let Some(webview) = &self.webview else {
            return Ok(());
        };
        webview
            .evaluate_script(&format!(
                "window.__mdEditorSetFontSize?.({:.4}, {:.4});",
                size, base_size
            ))
            .map_err(|error| format!("无法更新预览字号：{error}"))?;
        self.applied_font_size = Some(key);
        Ok(())
    }

    pub fn scroll_to_heading(&self, index: usize) -> Result<(), String> {
        let Some(webview) = &self.webview else {
            return Err("阅读预览尚未就绪".to_string());
        };
        webview
            .evaluate_script(&format!("window.__mdEditorScrollHeading?.({index});"))
            .map_err(|error| format!("无法定位章节：{error}"))
    }

    pub fn find_text(
        &self,
        query: &str,
        source_position: f32,
        backwards: bool,
    ) -> Result<(), String> {
        let Some(webview) = &self.webview else {
            return Err("浏览器预览尚未就绪".to_string());
        };
        let query =
            serde_json::to_string(query).map_err(|error| format!("无法编码查找文本：{error}"))?;
        webview
            .evaluate_script(&format!(
                "window.__mdEditorFindText?.({}, {:.8}, {});",
                query,
                source_position.max(0.0),
                if backwards { "true" } else { "false" }
            ))
            .map_err(|error| format!("无法查找预览文本：{error}"))
    }
}

fn parse_source_message(message: &str) -> Option<(f32, bool)> {
    let mut parts = message.split(':');
    if parts.next()? != "md-source" {
        return None;
    }
    let source = parts.next()?;
    let source_position = parts.next()?.parse::<f32>().ok()?.max(0.0);
    if parts.next().is_some() {
        return None;
    }
    match source {
        "user" => Some((source_position, true)),
        "program" => Some((source_position, false)),
        _ => None,
    }
}

fn parse_anchor_message(message: &str) -> Option<(ScrollAnchor, bool)> {
    let mut parts = message.split(':');
    if parts.next()? != "md-anchor" {
        return None;
    }
    let source = parts.next()?;
    if !matches!(source, "user" | "program") {
        return None;
    }
    let block_id = parts.next()?.parse::<u64>().ok()?;
    let offset = parts.next()?.parse::<f32>().ok()?.clamp(0.0, 1.0);
    let source_position = parts.next()?.parse::<f32>().ok()?.max(0.0);
    parts.next().is_none().then_some((
        ScrollAnchor {
            block_id,
            offset,
            source_position,
        },
        source == "user",
    ))
}

fn parse_ready_message(message: &str) -> Option<WebViewReady> {
    let mut parts = message.split(':');
    if parts.next()? != "md-ready" {
        return None;
    }
    let ready = WebViewReady {
        content_height: parts.next()?.parse::<f32>().ok()?.max(0.0),
        viewport_height: parts.next()?.parse::<f32>().ok()?.max(0.0),
        element_count: parts.next()?.parse::<usize>().ok()?,
        error: None,
    };
    parts.next().is_none().then_some(ready)
}

fn parse_ready_error(message: &str) -> Option<WebViewReady> {
    Some(WebViewReady {
        content_height: 0.0,
        viewport_height: 0.0,
        element_count: 0,
        error: Some(message.strip_prefix("md-ready-error:")?.to_string()),
    })
}

#[cfg(target_os = "windows")]
fn capture_preview(
    frame: &eframe::Frame,
    rect: egui::Rect,
    pixels_per_point: f32,
) -> Option<egui::ColorImage> {
    use windows_sys::Win32::Foundation::POINT;
    use windows_sys::Win32::Graphics::Gdi::{
        BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BitBlt, CAPTUREBLT, ClientToScreen,
        CreateCompatibleBitmap, CreateCompatibleDC, DIB_RGB_COLORS, DeleteDC, DeleteObject, GetDC,
        GetDIBits, HGDIOBJ, ReleaseDC, SRCCOPY, SelectObject,
    };

    let window = frame.winit_window()?;
    let raw = window.window_handle().ok()?.as_raw();
    let RawWindowHandle::Win32(handle) = raw else {
        return None;
    };
    let hwnd = handle.hwnd.get() as *mut core::ffi::c_void;
    let bounds = physical_bounds(rect, pixels_per_point);
    let width = bounds[2];
    let height = bounds[3];
    let mut origin = POINT {
        x: bounds[0],
        y: bounds[1],
    };

    // SAFETY: every GDI handle is checked and released before returning. The
    // destination buffer is sized to width * height * 4 and BITMAPINFO asks for
    // exactly a 32-bit top-down image.
    unsafe {
        if ClientToScreen(hwnd, &mut origin) == 0 {
            return None;
        }
        let screen_dc = GetDC(std::ptr::null_mut());
        if screen_dc.is_null() {
            return None;
        }
        let memory_dc = CreateCompatibleDC(screen_dc);
        if memory_dc.is_null() {
            ReleaseDC(std::ptr::null_mut(), screen_dc);
            return None;
        }
        let bitmap = CreateCompatibleBitmap(screen_dc, width, height);
        if bitmap.is_null() {
            DeleteDC(memory_dc);
            ReleaseDC(std::ptr::null_mut(), screen_dc);
            return None;
        }
        let previous = SelectObject(memory_dc, bitmap as HGDIOBJ);
        let copied = BitBlt(
            memory_dc,
            0,
            0,
            width,
            height,
            screen_dc,
            origin.x,
            origin.y,
            SRCCOPY | CAPTUREBLT,
        );

        let mut info: BITMAPINFO = std::mem::zeroed();
        info.bmiHeader = BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: width,
            biHeight: -height,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB,
            ..std::mem::zeroed()
        };
        let mut bgra = vec![0_u8; width as usize * height as usize * 4];
        let rows = if copied != 0 {
            GetDIBits(
                memory_dc,
                bitmap,
                0,
                height as u32,
                bgra.as_mut_ptr().cast(),
                &mut info,
                DIB_RGB_COLORS,
            )
        } else {
            0
        };

        SelectObject(memory_dc, previous);
        DeleteObject(bitmap as HGDIOBJ);
        DeleteDC(memory_dc);
        ReleaseDC(std::ptr::null_mut(), screen_dc);
        if rows == 0 {
            return None;
        }

        for pixel in bgra.chunks_exact_mut(4) {
            pixel.swap(0, 2);
            pixel[3] = 255;
        }
        Some(egui::ColorImage::from_rgba_unmultiplied(
            [width as usize, height as usize],
            &bgra,
        ))
    }
}

#[allow(dead_code)]
pub fn document(
    document: &crate::markdown::ParsedDocument,
    css: &str,
    base_directory: Option<&Path>,
    font_size_override: Option<f32>,
    dark_mode_css: Option<&str>,
) -> String {
    let block_ids = stable_top_level_block_ids(document);
    document_with_block_ids(
        document,
        css,
        base_directory,
        font_size_override,
        dark_mode_css,
        &block_ids,
    )
}

fn document_with_block_ids(
    document: &crate::markdown::ParsedDocument,
    css: &str,
    base_directory: Option<&Path>,
    font_size_override: Option<f32>,
    dark_mode_css: Option<&str>,
    block_ids: &[u64],
) -> String {
    let markdown = document.source();
    let line_starts = source_line_starts(markdown);
    let has_mermaid = document.has_mermaid();
    let mut heading_index = 0usize;
    let mut block_depth = 0usize;
    let mut block_index = 0usize;
    let mut events = vec![Event::Html(source_anchor(0.0).into())];
    for item in document.events() {
        let mut event = item.event.clone();
        let range = item.range.clone();
        if let Event::Start(Tag::Heading { id, .. }) = &mut event {
            *id = Some(format!("md-heading-{heading_index}").into());
            heading_index += 1;
        }
        let starts_block = matches!(&event, Event::Start(tag) if is_block_tag(tag));
        if starts_block {
            if block_depth == 0 {
                events.push(Event::Html(
                    source_anchor(source_line_at_byte(&line_starts, range.start)).into(),
                ));
                if let Some(block_id) = block_ids.get(block_index) {
                    events.push(Event::Html(block_anchor(*block_id).into()));
                }
                block_index += 1;
            }
            block_depth += 1;
        } else if block_depth == 0 && matches!(event, Event::Rule) {
            events.push(Event::Html(
                source_anchor(source_line_at_byte(&line_starts, range.start)).into(),
            ));
            if let Some(block_id) = block_ids.get(block_index) {
                events.push(Event::Html(block_anchor(*block_id).into()));
            }
            block_index += 1;
        }
        let ends_block = matches!(&event, Event::End(tag) if is_block_tag_end(*tag));
        events.push(rewrite_local_image_event(event, base_directory));
        if ends_block {
            block_depth = block_depth.saturating_sub(1);
        }
    }
    let source_end = line_starts.len() as f32;
    events.push(Event::Html(source_anchor(source_end).into()));
    let mut body = String::new();
    html::push_html(&mut body, events.into_iter());
    annotate_code_languages(&mut body);
    normalize_footnote_dom(&mut body);

    let base = base_directory
        .and_then(|path| url::Url::from_directory_path(path).ok())
        .map(|url| format!(r#"<base href="{}">"#, escape_attribute(url.as_str())))
        .unwrap_or_default();
    let font_override = font_size_override
        .map(|size| crate::theme::font_size_override_css(css, size))
        .unwrap_or_default();
    let font_runtime = crate::theme::font_size_runtime_css(css, font_size_override);
    let editor_font = editor_font_css();
    let dark_mode_css = dark_mode_css.unwrap_or_default();
    let asset_origin = custom_protocol_script_source("mdfont");
    let mermaid_scripts = if has_mermaid {
        format!(
            "<script defer src=\"{}\"></script>",
            custom_protocol_url("mdfont", "mermaid-init.js")
        )
    } else {
        String::new()
    };

    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><meta http-equiv=\"Content-Security-Policy\" content=\"script-src {asset_origin}; object-src 'none'; base-uri 'self' file:\">{base}<style>{STRUCTURAL_FALLBACK}</style><style>{css}</style><style>{editor_font}{MARKDOWN_DOM_COMPATIBILITY}{font_override}{font_runtime}{dark_mode_css}</style>{mermaid_scripts}</head><body>{body}</body></html>"
    )
}

pub fn preview_document(
    document: &crate::markdown::ParsedDocument,
    css: &str,
    base_directory: Option<&Path>,
    font_size_override: Option<f32>,
    dark_mode_css: Option<&str>,
) -> PreviewDocument {
    let block_ids = stable_top_level_block_ids(document);
    let html = document_with_block_ids(
        document,
        css,
        base_directory,
        font_size_override,
        dark_mode_css,
        &block_ids,
    );
    let mut preview = virtualize_document(html);
    preview.block_fingerprint = block_fingerprint(document);
    preview.block_ids = block_ids.into();
    preview.block_index = enriched_block_index(document, &preview);
    preview.update_stats = PreviewUpdateStats {
        replaced_blocks: document.blocks().len(),
        replaced_virtual_chunks: preview.virtual_chunk_count(),
    };
    preview
}

/// Lightweight document used while the first background render is running.
/// Keeping the shell themed avoids blocking the UI thread on a long parse or
/// HTML generation pass; the worker result replaces this document shortly
/// afterwards.
pub fn preview_document_placeholder(
    css: &str,
    font_size_override: Option<f32>,
    dark_mode_css: Option<&str>,
) -> PreviewDocument {
    let font_override = font_size_override
        .map(|size| crate::theme::font_size_override_css(css, size))
        .unwrap_or_default();
    let font_runtime = crate::theme::font_size_runtime_css(css, font_size_override);
    let editor_font = editor_font_css();
    let dark_mode_css = dark_mode_css.unwrap_or_default();
    let shell = format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><style>{STRUCTURAL_FALLBACK}</style><style>{css}</style><style>{editor_font}{MARKDOWN_DOM_COMPATIBILITY}{font_override}{font_runtime}{dark_mode_css}</style></head><body><div class=\"md-render-pending\">正在渲染…</div></body></html>"
    );
    let hash = hash(&shell);
    PreviewDocument {
        shell: shell.into(),
        body_range: None,
        virtual_manifest: None,
        blocks: Arc::from([]),
        chunks: Arc::from([]),
        hash,
        chrome_hash: hash,
        has_mermaid: false,
        total_bytes: 0,
        block_fingerprint: 0,
        block_ids: Arc::from([]),
        block_index: Arc::from([]),
        update_stats: PreviewUpdateStats::default(),
    }
}

pub fn preview_document_with_previous(
    previous: Option<&PreviewDocument>,
    document: &crate::markdown::ParsedDocument,
) -> Option<PreviewDocument> {
    let previous = previous?;
    if previous.virtual_manifest.is_some()
        || previous.block_fingerprint != block_fingerprint(document)
    {
        return None;
    }
    let anchors = document_source_anchors(document);
    if anchors.len() != previous.source_anchors().len() {
        return None;
    }
    let html = replace_source_markers(previous.shell.as_ref(), &anchors);
    let mut preview = virtualize_document(html);
    preview.block_fingerprint = block_fingerprint(document);
    preview.block_ids = reconcile_top_level_block_ids(previous, document).into();
    preview.block_index = enriched_block_index(document, &preview);
    preview.update_stats = PreviewUpdateStats::default();
    Some(preview)
}

/// Rebuild only changed simple top-level blocks while retaining the previous
/// document shell and unchanged block HTML. This keeps theme CSS, image bases,
/// and browser state stable; callers must provide the parsed document that
/// produced `previous` so block identity can be compared without rendering
/// every block again.
fn preview_document_incremental_variable(
    previous_preview: &PreviewDocument,
    previous_document: &crate::markdown::ParsedDocument,
    document: &crate::markdown::ParsedDocument,
    base_directory: Option<&Path>,
) -> Option<PreviewDocument> {
    let body_range = previous_preview.body_range?;
    if previous_preview.virtual_manifest.is_some()
        || previous_preview.has_mermaid != document.has_mermaid()
        || previous_preview.blocks.len() != previous_document.blocks().len() + 1
    {
        return None;
    }

    let old_blocks = previous_document.blocks();
    let new_blocks = document.blocks();
    if old_blocks == new_blocks {
        return preview_document_with_previous(Some(previous_preview), document);
    }

    let slices = top_level_event_slices(document)?;
    if slices.len() != new_blocks.len() {
        return None;
    }
    let anchors = document_source_anchors(document);
    if anchors.len() != new_blocks.len() + 2 {
        return None;
    }
    let block_ids = stable_top_level_block_ids(document);
    let mut body = String::new();
    let mut replaced_blocks = 0usize;
    body.push_str(&previous_preview.blocks.first()?.html);
    let old_by_id = previous_document
        .block_index()
        .iter()
        .enumerate()
        .filter_map(|(index, entry)| {
            previous_preview
                .blocks
                .get(index + 1)
                .map(|chunk| (entry.id, (index, &old_blocks[index], chunk.html.as_ref())))
        })
        .collect::<HashMap<_, _>>();
    let mut heading_index = 0usize;
    for index in 0..new_blocks.len() {
        let id = block_ids[index];
        if let Some((_, old_block, old_html)) = old_by_id.get(&id)
            && *old_block == &new_blocks[index]
        {
            body.push_str(&rewrite_heading_ids(old_html, heading_index));
        } else {
            replaced_blocks += 1;
            body.push_str(&render_incremental_block(
                document,
                index,
                &slices[index],
                anchors[index + 1],
                id,
                base_directory,
            )?);
        }
        heading_index += block_heading_count(&new_blocks[index]);
    }
    body.push_str(&source_anchor(*anchors.last()?));

    let mut html = String::with_capacity(previous_preview.shell.len() + body.len());
    html.push_str(&previous_preview.shell[..body_range.0]);
    html.push_str(&body);
    html.push_str(&previous_preview.shell[body_range.1..]);
    let html = replace_source_markers(&html, &anchors);
    if requires_virtualization(&html) {
        return None;
    }
    let mut preview = virtualize_document(html);
    preview.block_fingerprint = block_fingerprint(document);
    preview.block_ids = block_ids.into();
    preview.block_index = enriched_block_index(document, &preview);
    preview.update_stats = PreviewUpdateStats {
        replaced_blocks,
        replaced_virtual_chunks: 0,
    };
    Some(preview)
}

pub fn preview_document_incremental(
    previous_preview: Option<&PreviewDocument>,
    previous_document: Option<&crate::markdown::ParsedDocument>,
    document: &crate::markdown::ParsedDocument,
    base_directory: Option<&Path>,
) -> Option<PreviewDocument> {
    let previous_preview = previous_preview?;
    let previous_document = previous_document?;
    if previous_document.blocks().len() != document.blocks().len() {
        return preview_document_incremental_variable(
            previous_preview,
            previous_document,
            document,
            base_directory,
        );
    }
    let body_range = previous_preview.body_range?;
    if previous_preview.virtual_manifest.is_some()
        || previous_preview.has_mermaid != document.has_mermaid()
        || previous_document.blocks().len() != document.blocks().len()
        || previous_preview.blocks.len() != document.blocks().len() + 1
    {
        return None;
    }

    let changed = previous_document
        .blocks()
        .iter()
        .zip(document.blocks())
        .enumerate()
        .filter_map(|(index, (old, new))| (old != new).then_some(index))
        .collect::<Vec<_>>();
    if changed.is_empty() {
        return preview_document_with_previous(Some(previous_preview), document);
    }
    let slices = top_level_event_slices(document)?;
    if slices.len() != document.blocks().len() {
        return None;
    }
    let anchors = document_source_anchors(document);
    if anchors.len() != document.blocks().len() + 2 {
        return None;
    }

    let block_ids = stable_top_level_block_ids(document);
    let mut body = String::with_capacity(previous_preview.shell.len());
    body.push_str(&previous_preview.blocks.first()?.html);
    let mut heading_index = 0usize;
    for index in 0..document.blocks().len() {
        let old_chunk = previous_preview.blocks.get(index + 1)?;
        if changed.binary_search(&index).is_ok() {
            body.push_str(&render_incremental_block(
                document,
                index,
                &slices[index],
                anchors[index + 1],
                block_ids[index],
                base_directory,
            )?);
        } else {
            body.push_str(&rewrite_heading_ids(&old_chunk.html, heading_index));
        }
        heading_index += block_heading_count(&document.blocks()[index]);
    }
    body.push_str(&source_anchor(*anchors.last()?));
    let mut html = String::with_capacity(previous_preview.shell.len() + body.len());
    html.push_str(&previous_preview.shell[..body_range.0]);
    html.push_str(&body);
    html.push_str(&previous_preview.shell[body_range.1..]);
    let html = replace_source_markers(&html, &anchors);
    if requires_virtualization(&html) {
        return None;
    }
    let mut preview = virtualize_document(html);
    preview.block_fingerprint = block_fingerprint(document);
    preview.block_ids = block_ids.into();
    preview.block_index = enriched_block_index(document, &preview);
    preview.update_stats = PreviewUpdateStats {
        replaced_blocks: changed.len(),
        replaced_virtual_chunks: 0,
    };
    Some(preview)
}

/// Update only the virtual chunks containing changed simple blocks. The chunk
/// boundaries and source-marker count must remain stable; otherwise the caller
/// falls back to a complete virtual document render.
pub fn preview_document_virtual_incremental(
    previous_preview: Option<&PreviewDocument>,
    previous_document: Option<&crate::markdown::ParsedDocument>,
    document: &crate::markdown::ParsedDocument,
    base_directory: Option<&Path>,
) -> Option<PreviewDocument> {
    let previous_preview = previous_preview?;
    let previous_document = previous_document?;
    if previous_preview.virtual_manifest.is_none()
        || previous_preview.has_mermaid != document.has_mermaid()
        || previous_document.blocks().len() != document.blocks().len()
    {
        return None;
    }
    let changed = previous_document
        .blocks()
        .iter()
        .zip(document.blocks())
        .enumerate()
        .filter_map(|(index, (old, new))| (old != new).then_some(index))
        .collect::<Vec<_>>();
    if changed.is_empty() {
        return None;
    }
    let render_indices =
        heading_affected_indices(&changed, previous_document.blocks(), document.blocks());
    let slices = top_level_event_slices(document)?;
    if slices.len() != document.blocks().len() {
        return None;
    }
    let anchors = document_source_anchors(document);
    let block_ids = stable_top_level_block_ids(document);
    let mut marker_cursor = 0usize;
    let mut block_cursor = 0usize;
    let mut replaced_virtual_chunks = 0usize;
    let mut chunks = Vec::with_capacity(previous_preview.chunks.len());
    for (chunk_index, old_chunk) in previous_preview.chunks.iter().enumerate() {
        let marker_count = old_chunk.source_anchors.len();
        let marker_ranges = source_marker_ranges(&old_chunk.html);
        if marker_count == 0 || marker_ranges.len() != marker_count {
            return None;
        }
        let chunk_end = marker_cursor + marker_count;
        let mut replacements = render_indices
            .iter()
            .filter_map(|index| {
                let marker_index = index + 1;
                if marker_index >= marker_cursor && marker_index < chunk_end {
                    Some((marker_index - marker_cursor, *index))
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();
        if !replacements.is_empty() {
            replaced_virtual_chunks += 1;
        }
        replacements.sort_unstable_by_key(|replacement| std::cmp::Reverse(replacement.0));
        let mut html = old_chunk.html.to_string();
        for (local_marker, block_index) in replacements {
            let marker = &marker_ranges[local_marker];
            let content_end = marker_ranges
                .get(local_marker + 1)
                .map_or(html.len(), |next| next.start);
            let content = render_incremental_block_content(
                document,
                block_index,
                &slices[block_index],
                block_ids[block_index],
                base_directory,
            )?;
            html.replace_range(marker.end..content_end, &content);
        }
        let values = anchors.get(marker_cursor..chunk_end)?;
        let replaced = replace_source_markers_with_values(&html, values)?;
        html = replace_block_markers_with_ids(&replaced, block_cursor, &block_ids)?;
        if !virtual_chunk_boundary_stable(
            &old_chunk.html,
            &html,
            chunk_index + 1 < previous_preview.chunks.len(),
        ) {
            return None;
        }
        let source_start = values.first().copied()?;
        let source_end = anchors
            .get(chunk_end)
            .copied()
            .or_else(|| anchors.last().copied())?;
        if source_end < source_start {
            return None;
        }
        let image_count = image_tag_positions(&html).len();
        let estimated_height = ((source_end - source_start).max(1.0) * 34.0)
            .max(image_count as f32 * ESTIMATED_IMAGE_HEIGHT)
            .max(96.0);
        let heading_bounds = heading_bounds(&html);
        let source_anchors = source_markers(&html);
        let block_count = virtual_block_marker_ranges(&html).len();
        let block_id = block_id_in_html(&html).unwrap_or(old_chunk.block_id);
        chunks.push(PreviewChunk {
            block_id,
            content_hash: hash_source_markerless(&html),
            html: html.into(),
            source_start,
            source_end,
            source_anchors: source_anchors.into(),
            estimated_height,
            heading_start: heading_bounds.map(|bounds| bounds.0),
            heading_end: heading_bounds.map(|bounds| bounds.1),
        });
        marker_cursor = chunk_end;
        block_cursor = block_cursor.saturating_add(block_count);
    }
    if marker_cursor != anchors.len() {
        return None;
    }
    let manifest = virtual_manifest(&chunks);
    let shell = replace_virtual_manifest(previous_preview.shell.as_ref(), &manifest)?;
    let mut hash_input = shell.clone();
    for chunk in &chunks {
        hash_input.push_str(&chunk.block_id.to_string());
        hash_input.push_str(&chunk.content_hash.to_string());
    }
    let previous_body_bytes = previous_preview
        .chunks
        .iter()
        .map(|chunk| chunk.html.len())
        .sum::<usize>();
    let next_body_bytes = chunks.iter().map(|chunk| chunk.html.len()).sum::<usize>();
    let total_bytes = previous_preview
        .total_bytes
        .saturating_sub(previous_body_bytes)
        .saturating_add(next_body_bytes);
    let mut preview = PreviewDocument {
        shell: shell.into(),
        body_range: None,
        virtual_manifest: Some(manifest.into()),
        blocks: Arc::from([]),
        chunks: chunks.into(),
        hash: hash(&hash_input),
        chrome_hash: previous_preview.chrome_hash,
        has_mermaid: previous_preview.has_mermaid,
        total_bytes,
        block_fingerprint: block_fingerprint(document),
        block_ids: block_ids.into(),
        block_index: Arc::from([]),
        update_stats: PreviewUpdateStats {
            replaced_blocks: 0,
            replaced_virtual_chunks,
        },
    };
    preview.block_index = enriched_block_index(document, &preview);
    Some(preview)
}

fn enriched_block_index(
    document: &crate::markdown::ParsedDocument,
    preview: &PreviewDocument,
) -> Arc<[crate::markdown::BlockIndexEntry]> {
    let mut entries = document.block_index().to_vec();
    if preview.virtual_manifest.is_none() {
        for (entry, chunk) in entries.iter_mut().zip(preview.blocks.iter().skip(1)) {
            entry.rendered_height = chunk.estimated_height;
        }
    }
    entries.into()
}

fn heading_count(blocks: &[crate::markdown::Block]) -> usize {
    blocks
        .iter()
        .map(|block| match block {
            crate::markdown::Block::Heading { .. } => 1,
            crate::markdown::Block::List { items, .. } => {
                items.iter().map(|item| heading_count(item)).sum()
            }
            crate::markdown::Block::Quote(children) => heading_count(children),
            _ => 0,
        })
        .sum()
}

fn block_heading_count(block: &crate::markdown::Block) -> usize {
    heading_count(std::slice::from_ref(block))
}

fn heading_affected_indices(
    changed: &[usize],
    old_blocks: &[crate::markdown::Block],
    new_blocks: &[crate::markdown::Block],
) -> Vec<usize> {
    let heading_count_changed = heading_count(old_blocks) != heading_count(new_blocks);
    if !heading_count_changed {
        return changed.to_vec();
    }
    let Some(first_changed) = changed.first().copied() else {
        return Vec::new();
    };
    let mut indices = changed.to_vec();
    indices.extend((first_changed + 1..new_blocks.len()).filter(|index| {
        old_blocks
            .get(*index)
            .is_some_and(|block| block_heading_count(block) > 0)
            || new_blocks
                .get(*index)
                .is_some_and(|block| block_heading_count(block) > 0)
    }));
    indices.sort_unstable();
    indices.dedup();
    indices
}

#[derive(Debug, Clone, Copy)]
struct BlockSlice {
    event_start: usize,
    event_end: usize,
    source_start: usize,
    source_end: usize,
}

fn top_level_event_slices(document: &crate::markdown::ParsedDocument) -> Option<Vec<BlockSlice>> {
    let mut depth = 0usize;
    let mut start = None;
    let mut slices = Vec::new();
    for (index, item) in document.events().iter().enumerate() {
        match &item.event {
            Event::Start(tag) if is_block_tag(tag) => {
                if depth == 0 {
                    start = Some((index, item.range.start));
                }
                depth += 1;
            }
            Event::End(tag_end) if is_block_tag_end(*tag_end) => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    let (event_start, source_start) = start.take()?;
                    slices.push(BlockSlice {
                        event_start,
                        event_end: index + 1,
                        source_start,
                        source_end: item.range.end,
                    });
                }
            }
            Event::Rule if depth == 0 => {
                slices.push(BlockSlice {
                    event_start: index,
                    event_end: index + 1,
                    source_start: item.range.start,
                    source_end: item.range.end,
                });
            }
            _ => {}
        }
    }
    (depth == 0 && start.is_none()).then_some(slices)
}

fn render_incremental_block(
    document: &crate::markdown::ParsedDocument,
    index: usize,
    slice: &BlockSlice,
    source_start: f32,
    block_id: u64,
    base_directory: Option<&Path>,
) -> Option<String> {
    let mut html = String::new();
    html.push_str(&source_anchor(source_start));
    html.push_str(&render_incremental_block_content(
        document,
        index,
        slice,
        block_id,
        base_directory,
    )?);
    Some(html)
}

fn render_incremental_block_content(
    document: &crate::markdown::ParsedDocument,
    index: usize,
    slice: &BlockSlice,
    block_id: u64,
    base_directory: Option<&Path>,
) -> Option<String> {
    if slice.source_start > slice.source_end || slice.source_end > document.source().len() {
        return None;
    }
    let mut events = Vec::new();
    let mut heading_index = heading_count(&document.blocks()[..index]);
    for item in &document.events()[slice.event_start..slice.event_end] {
        let mut event = item.event.clone();
        if let Event::Start(Tag::Heading { id, .. }) = &mut event {
            *id = Some(format!("md-heading-{heading_index}").into());
            heading_index += 1;
        }
        events.push(rewrite_local_image_event(event, base_directory));
    }
    let mut html = String::new();
    html::push_html(&mut html, events.into_iter());
    // Keep the same boundary formatting as the full document renderer. The
    // Markdown HTML writer emits a leading newline for most block tags, but
    // starts tables directly after the marker. Mirror that small formatting
    // distinction so a local replacement is byte-for-byte compatible with a
    // full render.
    let separator = if matches!(
        document.blocks()[index],
        crate::markdown::Block::Table { .. } | crate::markdown::Block::Raw(_)
    ) {
        ""
    } else {
        "\n"
    };
    html.insert_str(0, &format!("{}{separator}", block_anchor(block_id)));
    annotate_code_languages(&mut html);
    Some(html)
}

fn source_marker_ranges(html: &str) -> Vec<Range<usize>> {
    let mut ranges = Vec::new();
    let mut cursor = 0usize;
    while let Some(relative_start) = html[cursor..].find(DOM_SOURCE_MARKER_PREFIX) {
        let start = cursor + relative_start;
        let Some(relative_end) = html[start..].find("-->") else {
            break;
        };
        let end = start + relative_end + "-->".len();
        ranges.push(start..end);
        cursor = end;
    }
    ranges
}

fn replace_source_markers_with_values(html: &str, values: &[f32]) -> Option<String> {
    let ranges = source_marker_ranges(html);
    if ranges.len() != values.len() {
        return None;
    }
    let mut output = String::with_capacity(html.len());
    let mut cursor = 0usize;
    for (range, value) in ranges.into_iter().zip(values) {
        output.push_str(&html[cursor..range.start]);
        output.push_str(&source_anchor(*value));
        cursor = range.end;
    }
    output.push_str(&html[cursor..]);
    Some(output)
}

fn replace_virtual_manifest(shell: &str, manifest: &str) -> Option<String> {
    const PREFIX: &str = "<script id=\"md-virtual-manifest\" type=\"application/json\">";
    let start = shell.find(PREFIX)?;
    let content_start = start + PREFIX.len();
    let content_end = content_start + shell[content_start..].find("</script>")?;
    let mut output = String::with_capacity(shell.len() + manifest.len());
    output.push_str(&shell[..content_start]);
    output.push_str(manifest);
    output.push_str(&shell[content_end..]);
    Some(output)
}

fn virtual_manifest(chunks: &[PreviewChunk]) -> String {
    let manifest = chunks
        .iter()
        .enumerate()
        .map(|(index, chunk)| {
            let (block_ids, block_hashes) = virtual_chunk_block_metadata(&chunk.html);
            let block_ids = block_ids
                .into_iter()
                .map(|id| id.to_string())
                .collect::<Vec<_>>();
            let block_hashes = block_hashes
                .into_iter()
                .map(|hash| hash.to_string())
                .collect::<Vec<_>>();
            format!(
                "{{\"index\":{index},\"blockId\":\"{}\",\"blockIds\":{},\"blockHashes\":{},\"contentHash\":\"{}\",\"sourceStart\":{},\"sourceEnd\":{},\"sourceAnchors\":{},\"height\":{},\"headingStart\":{},\"headingEnd\":{}}}",
                chunk.block_id,
                serde_json::to_string(&block_ids).unwrap_or_else(|_| "[]".to_string()),
                serde_json::to_string(&block_hashes).unwrap_or_else(|_| "[]".to_string()),
                chunk.content_hash,
                chunk.source_start,
                chunk.source_end,
                serde_json::to_string(&chunk.source_anchors[..])
                    .unwrap_or_else(|_| "[]".to_string()),
                chunk.estimated_height,
                chunk
                    .heading_start
                    .map_or("null".to_string(), |value| value.to_string()),
                chunk
                    .heading_end
                    .map_or("null".to_string(), |value| value.to_string()),
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    format!("[{manifest}]")
}

/// Return the stable top-level block IDs and markerless content hashes carried
/// by one virtual chunk.  The browser uses these parallel arrays to decide
/// whether a loaded chunk can be patched block-by-block instead of discarded.
fn virtual_chunk_block_metadata(html: &str) -> (Vec<u64>, Vec<u64>) {
    let mut markers = Vec::new();
    let mut cursor = 0usize;
    while let Some(relative) = html[cursor..].find(DOM_BLOCK_MARKER_PREFIX) {
        let start = cursor + relative;
        let id_start = start + DOM_BLOCK_MARKER_PREFIX.len();
        let Some(relative_end) = html[id_start..].find("-->") else {
            break;
        };
        let end = id_start + relative_end + "-->".len();
        if let Ok(id) = html[id_start..id_start + relative_end].parse::<u64>() {
            markers.push((start..end, id));
        }
        cursor = end;
    }

    let mut ids = Vec::with_capacity(markers.len());
    let mut hashes = Vec::with_capacity(markers.len());
    for (index, (range, id)) in markers.iter().enumerate() {
        let content_end = markers
            .get(index + 1)
            .map_or(html.len(), |(next, _)| next.start);
        ids.push(*id);
        hashes.push(hash_source_markerless(&html[range.end..content_end]));
    }
    (ids, hashes)
}

fn replace_block_markers_with_ids(
    html: &str,
    block_start: usize,
    block_ids: &[u64],
) -> Option<String> {
    let block_markers = virtual_block_marker_ranges(html);
    if block_markers.is_empty() {
        return Some(html.to_string());
    }
    let mut output = String::with_capacity(html.len());
    let mut cursor = 0usize;
    for (local_index, (range, _)) in block_markers.iter().enumerate() {
        let block_index = block_start.checked_add(local_index)?;
        let id = *block_ids.get(block_index)?;
        output.push_str(&html[cursor..range.start]);
        output.push_str(&block_anchor(id));
        cursor = range.end;
    }
    output.push_str(&html[cursor..]);
    Some(output)
}

fn virtual_block_marker_ranges(html: &str) -> Vec<(std::ops::Range<usize>, u64)> {
    let mut markers = Vec::new();
    let mut cursor = 0usize;
    while let Some(relative) = html[cursor..].find(DOM_BLOCK_MARKER_PREFIX) {
        let start = cursor + relative;
        let id_start = start + DOM_BLOCK_MARKER_PREFIX.len();
        let Some(relative_end) = html[id_start..].find("-->") else {
            break;
        };
        let end = id_start + relative_end + "-->".len();
        if let Ok(id) = html[id_start..id_start + relative_end].parse::<u64>() {
            markers.push((start..end, id));
        }
        cursor = end;
    }
    markers
}

/// Return false when a changed chunk can move the first marker that crosses a
/// virtual chunk threshold.  Keeping the old grouping in that case would make
/// the manifest's source ranges disagree with a full render, so the caller
/// falls back to rebuilding the virtual document.
fn virtual_chunk_boundary_stable(
    old_html: &str,
    new_html: &str,
    has_following_boundary: bool,
) -> bool {
    let old_markers = source_marker_ranges(old_html);
    let new_markers = source_marker_ranges(new_html);
    if old_markers.len() != new_markers.len() {
        return false;
    }
    let old_image_positions = image_tag_positions(old_html);
    let new_image_positions = image_tag_positions(new_html);
    for index in 1..old_markers.len() {
        let old_marker = &old_markers[index];
        let new_marker = &new_markers[index];
        let old_bytes = old_marker.start;
        let new_bytes = new_marker.start;
        let old_images =
            old_image_positions.partition_point(|image_position| *image_position < old_bytes);
        let new_images =
            new_image_positions.partition_point(|image_position| *image_position < new_bytes);
        let old_crosses =
            old_bytes >= VIRTUAL_CHUNK_TARGET_BYTES || old_images >= VIRTUAL_CHUNK_TARGET_IMAGES;
        let new_crosses =
            new_bytes >= VIRTUAL_CHUNK_TARGET_BYTES || new_images >= VIRTUAL_CHUNK_TARGET_IMAGES;
        if new_crosses != old_crosses {
            return false;
        }
    }
    if has_following_boundary {
        let old_images = old_image_positions.len();
        let new_images = new_image_positions.len();
        let old_crosses = old_html.len() >= VIRTUAL_CHUNK_TARGET_BYTES
            || old_images >= VIRTUAL_CHUNK_TARGET_IMAGES;
        let new_crosses = new_html.len() >= VIRTUAL_CHUNK_TARGET_BYTES
            || new_images >= VIRTUAL_CHUNK_TARGET_IMAGES;
        if new_crosses != old_crosses {
            return false;
        }
    }
    true
}

fn virtualize_document(html: String) -> PreviewDocument {
    let document_hash = hash(&html);
    let total_bytes = html.len();
    let Some(body_start_tag) = html.find("<body>") else {
        return PreviewDocument {
            shell: html.into(),
            body_range: None,
            virtual_manifest: None,
            blocks: Arc::from([]),
            chunks: Arc::from([]),
            hash: document_hash,
            chrome_hash: document_hash,
            has_mermaid: false,
            total_bytes,
            block_fingerprint: 0,
            block_ids: Arc::from([]),
            block_index: Arc::from([]),
            update_stats: PreviewUpdateStats::default(),
        };
    };
    let body_start = body_start_tag + "<body>".len();
    let Some(body_end) = html.rfind("</body>") else {
        return PreviewDocument {
            shell: html.into(),
            body_range: None,
            virtual_manifest: None,
            blocks: Arc::from([]),
            chunks: Arc::from([]),
            hash: document_hash,
            chrome_hash: document_hash,
            has_mermaid: false,
            total_bytes,
            block_fingerprint: 0,
            block_ids: Arc::from([]),
            block_index: Arc::from([]),
            update_stats: PreviewUpdateStats::default(),
        };
    };
    let body = &html[body_start..body_end];
    let has_mermaid = html.contains("mermaid-init.js");
    let chrome_hash = hash(&format!("{}{}", &html[..body_start], &html[body_end..]));
    let image_positions = image_tag_positions(body);
    if total_bytes < VIRTUALIZE_AT_BYTES && image_positions.len() <= VIRTUALIZE_AT_IMAGES {
        let blocks = split_preview_blocks(body);
        return PreviewDocument {
            shell: html.into(),
            body_range: Some((body_start, body_end)),
            virtual_manifest: None,
            blocks: blocks.into(),
            chunks: Arc::from([]),
            hash: document_hash,
            chrome_hash,
            has_mermaid,
            total_bytes,
            block_fingerprint: 0,
            block_ids: Arc::from([]),
            block_index: Arc::from([]),
            update_stats: PreviewUpdateStats::default(),
        };
    }
    let mut boundaries = vec![0usize];
    let mut search_from = 1usize;
    let mut chunk_start = 0usize;
    let mut image_cursor = 0usize;
    let mut chunk_image_start = 0usize;
    while let Some(relative) = body[search_from..].find(DOM_SOURCE_MARKER_PREFIX) {
        let position = search_from + relative;
        while image_positions
            .get(image_cursor)
            .is_some_and(|image_position| *image_position < position)
        {
            image_cursor += 1;
        }
        if position.saturating_sub(chunk_start) >= VIRTUAL_CHUNK_TARGET_BYTES
            || image_cursor.saturating_sub(chunk_image_start) >= VIRTUAL_CHUNK_TARGET_IMAGES
        {
            boundaries.push(position);
            chunk_start = position;
            chunk_image_start = image_cursor;
        }
        search_from = position + DOM_SOURCE_MARKER_PREFIX.len();
    }
    boundaries.push(body.len());

    let source_end =
        source_marker_at(body, body.rfind(DOM_SOURCE_MARKER_PREFIX).unwrap_or(0)).unwrap_or(1.0);
    let mut chunks = Vec::with_capacity(boundaries.len().saturating_sub(1));
    for (index, pair) in boundaries.windows(2).enumerate() {
        let start = pair[0];
        let end = pair[1];
        let source_start = source_marker_at(body, start).unwrap_or(0.0);
        let source_end_for_chunk = source_marker_at(body, end).unwrap_or(source_end);
        let image_count = image_positions.partition_point(|position| *position < end)
            - image_positions.partition_point(|position| *position < start);
        let estimated_height = ((source_end_for_chunk - source_start).max(1.0) * 34.0)
            .max(image_count as f32 * ESTIMATED_IMAGE_HEIGHT)
            .max(96.0);
        let heading_bounds = heading_bounds(&body[start..end]);
        let chunk_html = &body[start..end];
        chunks.push(PreviewChunk {
            html: Arc::from(chunk_html),
            // A virtual chunk is a transport boundary, not a document block.
            // Reuse the first stable top-level block marker it contains so
            // inserting/removing earlier content does not make every later
            // chunk appear to be a different block.  Marker-less chunks keep
            // the deterministic index fallback for compatibility.
            block_id: block_id_in_html(chunk_html).unwrap_or_else(|| stable_chunk_id(index)),
            content_hash: hash_source_markerless(chunk_html),
            source_start,
            source_end: source_end_for_chunk,
            source_anchors: source_markers(chunk_html).into(),
            estimated_height,
            heading_start: heading_bounds.map(|bounds| bounds.0),
            heading_end: heading_bounds.map(|bounds| bounds.1),
        });
    }

    let manifest = virtual_manifest(&chunks);
    let virtual_script = format!(
        "<script id=\"md-virtual-manifest\" type=\"application/json\">{manifest}</script><script defer src=\"{}\"></script>",
        custom_protocol_url("mdfont", "virtual-preview.js")
    );
    let mut shell = String::with_capacity(html.len() - body.len() + virtual_script.len());
    shell.push_str(&html[..body_start]);
    shell.push_str(&virtual_script);
    shell.push_str(&html[body_end..]);
    PreviewDocument {
        shell: shell.into(),
        body_range: None,
        virtual_manifest: Some(manifest.into()),
        blocks: Arc::from([]),
        chunks: chunks.into(),
        hash: document_hash,
        chrome_hash,
        has_mermaid,
        total_bytes,
        block_fingerprint: 0,
        block_ids: Arc::from([]),
        block_index: Arc::from([]),
        update_stats: PreviewUpdateStats::default(),
    }
}

fn requires_virtualization(html: &str) -> bool {
    if html.len() >= VIRTUALIZE_AT_BYTES {
        return true;
    }
    let Some(body_start_tag) = html.find("<body>") else {
        return false;
    };
    let body_start = body_start_tag + "<body>".len();
    let Some(body_end) = html.rfind("</body>") else {
        return false;
    };
    image_tag_positions(&html[body_start..body_end]).len() > VIRTUALIZE_AT_IMAGES
}

fn split_preview_blocks(body: &str) -> Vec<PreviewChunk> {
    let mut markers = Vec::new();
    let mut search_from = 0usize;
    while let Some(relative) = body[search_from..].find(DOM_SOURCE_MARKER_PREFIX) {
        let position = search_from + relative;
        markers.push(position);
        search_from = position + DOM_SOURCE_MARKER_PREFIX.len();
    }
    markers
        .windows(2)
        .enumerate()
        .filter_map(|(index, pair)| {
            let start = pair[0];
            let end = pair[1];
            (start < end).then(|| PreviewChunk {
                html: Arc::from(&body[start..end]),
                block_id: block_id_in_html(&body[start..end])
                    .unwrap_or_else(|| stable_chunk_id(index)),
                content_hash: hash_source_markerless(&body[start..end]),
                source_start: source_marker_at(body, start).unwrap_or(0.0),
                source_end: source_marker_at(body, end).unwrap_or(0.0),
                source_anchors: source_markers(&body[start..end]).into(),
                estimated_height: ((source_marker_at(body, end).unwrap_or(0.0)
                    - source_marker_at(body, start).unwrap_or(0.0))
                .max(1.0)
                    * 34.0)
                    .max(96.0),
                heading_start: heading_bounds(&body[start..end]).map(|bounds| bounds.0),
                heading_end: heading_bounds(&body[start..end]).map(|bounds| bounds.1),
            })
        })
        .collect()
}

fn preview_block_content(html: &str) -> &str {
    html.strip_prefix(DOM_SOURCE_MARKER_PREFIX)
        .and_then(|rest| rest.split_once("-->").map(|(_, content)| content))
        .unwrap_or(html)
}

fn block_id_in_html(html: &str) -> Option<u64> {
    let start = html.find(DOM_BLOCK_MARKER_PREFIX)? + DOM_BLOCK_MARKER_PREFIX.len();
    let end = html[start..].find("-->")? + start;
    html[start..end].parse().ok()
}

fn image_tag_positions(html: &str) -> Vec<usize> {
    let bytes = html.as_bytes();
    bytes
        .windows(4)
        .enumerate()
        .filter_map(|(index, window)| {
            (window[0] == b'<'
                && window[1..].eq_ignore_ascii_case(b"img")
                && bytes
                    .get(index + 4)
                    .is_some_and(|next| next.is_ascii_whitespace() || matches!(*next, b'/' | b'>')))
            .then_some(index)
        })
        .collect()
}

fn source_marker_at(body: &str, position: usize) -> Option<f32> {
    let marker = body
        .get(position..)?
        .strip_prefix(DOM_SOURCE_MARKER_PREFIX)?;
    marker.split("-->").next()?.parse().ok()
}

fn source_markers(html: &str) -> Vec<f32> {
    let mut markers = Vec::new();
    let mut search_from = 0;
    while let Some(relative) = html[search_from..].find(DOM_SOURCE_MARKER_PREFIX) {
        let position = search_from + relative;
        if let Some(value) = source_marker_at(html, position) {
            markers.push(value);
        }
        search_from = position + DOM_SOURCE_MARKER_PREFIX.len();
    }
    markers
}

fn document_source_anchors(document: &crate::markdown::ParsedDocument) -> Vec<f32> {
    let markdown = document.source();
    let line_starts = source_line_starts(markdown);
    let mut anchors = vec![0.0];
    let mut block_depth = 0usize;
    for item in document.events() {
        let starts_block = matches!(&item.event, Event::Start(tag) if is_block_tag(tag));
        if block_depth == 0 && (starts_block || matches!(item.event, Event::Rule)) {
            anchors.push(source_line_at_byte(&line_starts, item.range.start));
        }
        if starts_block {
            block_depth += 1;
        }
        if matches!(&item.event, Event::End(tag) if is_block_tag_end(*tag)) {
            block_depth = block_depth.saturating_sub(1);
        }
    }
    anchors.push(line_starts.len() as f32);
    anchors
}

fn block_fingerprint(document: &crate::markdown::ParsedDocument) -> u64 {
    let mut hasher = DefaultHasher::new();
    document.blocks().hash(&mut hasher);
    hasher.finish()
}

/// Read the IDs assigned by the shared parser block index. Keeping the ID
/// ownership in `ParsedDocument` means the editor and preview use one identity
/// source instead of independently hashing blocks.
fn stable_top_level_block_ids(document: &crate::markdown::ParsedDocument) -> Vec<u64> {
    document
        .block_index()
        .iter()
        .map(|entry| entry.id)
        .collect()
}

fn reconcile_top_level_block_ids(
    _previous: &PreviewDocument,
    document: &crate::markdown::ParsedDocument,
) -> Vec<u64> {
    // Identity reconciliation happens while producing ParsedDocument, where
    // both old and new source ranges and neighbouring blocks are available.
    // Never copy IDs by index here: that would relabel blocks after an
    // insertion or deletion when the total block count happens to match.
    stable_top_level_block_ids(document)
}

fn replace_source_markers(html: &str, values: &[f32]) -> String {
    let mut output = String::with_capacity(html.len());
    let mut cursor = 0;
    let mut index = 0;
    while let Some(relative) = html[cursor..].find(DOM_SOURCE_MARKER_PREFIX) {
        let start = cursor + relative;
        let Some(end_relative) = html[start..].find("-->") else {
            break;
        };
        let end = start + end_relative + "-->".len();
        output.push_str(&html[cursor..start]);
        let value = values.get(index).copied().unwrap_or(0.0);
        output.push_str(&source_anchor(value));
        index += 1;
        cursor = end;
    }
    output.push_str(&html[cursor..]);
    output
}

fn hash_source_markerless(html: &str) -> u64 {
    let mut normalized = String::with_capacity(html.len());
    let mut cursor = 0;
    while let Some(relative) = html[cursor..].find(DOM_SOURCE_MARKER_PREFIX) {
        let start = cursor + relative;
        let Some(end_relative) = html[start..].find("-->") else {
            break;
        };
        let end = start + end_relative + "-->".len();
        normalized.push_str(&html[cursor..start]);
        normalized.push_str("<!--md-editor-source-->");
        cursor = end;
    }
    normalized.push_str(&html[cursor..]);
    let mut markerless = String::with_capacity(normalized.len());
    let mut cursor = 0;
    while let Some(relative) = normalized[cursor..].find(DOM_BLOCK_MARKER_PREFIX) {
        let start = cursor + relative;
        markerless.push_str(&normalized[cursor..start]);
        markerless.push_str("<!--md-editor-block-->");
        let Some(end_relative) = normalized[start..].find("-->") else {
            cursor = start;
            break;
        };
        cursor = start + end_relative + "-->".len();
    }
    markerless.push_str(&normalized[cursor..]);

    let mut stable = String::with_capacity(markerless.len());
    let mut cursor = 0;
    for (start, end) in heading_id_ranges(&markerless) {
        stable.push_str(&markerless[cursor..start]);
        stable.push_str("id=\"md-heading\"");
        cursor = end;
    }
    stable.push_str(&markerless[cursor..]);
    hash(&stable)
}

fn stable_chunk_id(index: usize) -> u64 {
    hash(&format!("markdown-preview-chunk:{index}"))
}

fn block_anchor(id: u64) -> String {
    format!("{DOM_BLOCK_MARKER_PREFIX}{id}-->")
}

fn heading_bounds(html: &str) -> Option<(usize, usize)> {
    let marker = "id=\"md-heading-";
    let mut first = None;
    let mut last = None;
    for (start, end) in heading_id_ranges(html) {
        let digits_start = start + marker.len();
        let index = html[digits_start..end - 1].parse::<usize>().ok()?;
        first.get_or_insert(index);
        last = Some(index + 1);
    }
    first.zip(last)
}

fn rewrite_heading_ids(html: &str, mut heading_index: usize) -> String {
    let marker = "id=\"md-heading-";
    let mut output = String::with_capacity(html.len());
    let mut cursor = 0usize;
    for (start, end) in heading_id_ranges(html) {
        output.push_str(&html[cursor..start]);
        output.push_str(marker);
        output.push_str(&heading_index.to_string());
        output.push('"');
        heading_index += 1;
        cursor = end;
    }
    output.push_str(&html[cursor..]);
    output
}

/// Find generated heading IDs without touching arbitrary IDs in raw HTML.
fn heading_id_ranges(html: &str) -> Vec<(usize, usize)> {
    const MARKER: &str = "id=\"md-heading-";
    let mut ranges = Vec::new();
    let mut cursor = 0usize;
    while let Some(relative) = html[cursor..].find("<h") {
        let tag_start = cursor + relative;
        let bytes = html.as_bytes();
        if tag_start + 2 >= bytes.len() || !matches!(bytes[tag_start + 2], b'1'..=b'6') {
            cursor = tag_start + 2;
            continue;
        }
        let Some(tag_end_relative) = html[tag_start..].find('>') else {
            break;
        };
        let tag_end = tag_start + tag_end_relative;
        let tag = &html[tag_start..=tag_end];
        let Some(marker_relative) = tag.find(MARKER) else {
            cursor = tag_end + 1;
            continue;
        };
        let start = tag_start + marker_relative;
        let digits_start = start + MARKER.len();
        let Some(quote_relative) = html[digits_start..=tag_end].find('"') else {
            cursor = tag_end + 1;
            continue;
        };
        let end = digits_start + quote_relative + 1;
        if html[digits_start..end - 1]
            .chars()
            .all(|character| character.is_ascii_digit())
        {
            ranges.push((start, end));
        }
        cursor = tag_end + 1;
    }
    ranges
}

fn source_anchor(source_line: f32) -> String {
    format!("{DOM_SOURCE_MARKER_PREFIX}{source_line}-->")
}

fn source_line_starts(markdown: &str) -> Vec<usize> {
    let mut starts = Vec::with_capacity(markdown.len() / 48 + 1);
    starts.push(0);
    starts.extend(
        markdown
            .bytes()
            .enumerate()
            .filter_map(|(index, byte)| (byte == b'\n').then_some(index + 1)),
    );
    starts
}

fn source_line_at_byte(line_starts: &[usize], byte_offset: usize) -> f32 {
    line_starts
        .partition_point(|start| *start <= byte_offset)
        .saturating_sub(1) as f32
}

fn custom_protocol_url(scheme: &str, path: &str) -> String {
    let path = path.trim_start_matches('/');
    #[cfg(target_os = "windows")]
    {
        format!("https://{scheme}.localhost/{path}")
    }
    #[cfg(target_os = "macos")]
    {
        format!("{scheme}://localhost/{path}")
    }
}

fn custom_protocol_script_source(scheme: &str) -> String {
    #[cfg(target_os = "windows")]
    {
        format!("https://{scheme}.localhost")
    }
    #[cfg(target_os = "macos")]
    {
        format!("{scheme}:")
    }
}

fn rewrite_local_image_event<'a>(event: Event<'a>, base_directory: Option<&Path>) -> Event<'a> {
    match event {
        Event::Start(Tag::Image {
            link_type,
            dest_url,
            title,
            id,
        }) => {
            let dest_url = local_image_url(dest_url.as_ref(), base_directory)
                .map(CowStr::from)
                .unwrap_or(dest_url);
            Event::Start(Tag::Image {
                link_type,
                dest_url,
                title,
                id,
            })
        }
        Event::Html(fragment) => Event::Html(
            crate::html_image::rewrite_sources(fragment.as_ref(), |destination| {
                local_image_url(destination, base_directory)
            })
            .into(),
        ),
        Event::InlineHtml(fragment) => Event::InlineHtml(
            crate::html_image::rewrite_sources(fragment.as_ref(), |destination| {
                local_image_url(destination, base_directory)
            })
            .into(),
        ),
        event => event,
    }
}

fn local_image_url(destination: &str, base_directory: Option<&Path>) -> Option<String> {
    if destination.is_empty() || destination.starts_with('#') {
        return None;
    }

    let path = Path::new(destination);
    if !path.is_absolute() && url::Url::parse(destination).is_ok() {
        return None;
    }
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        base_directory?.join(path)
    };
    let file_url = url::Url::from_file_path(absolute).ok()?;
    Some(custom_protocol_url("mdfile", file_url.path()))
}

fn editor_font_css() -> String {
    format!(
        "@font-face{{font-family:'Markdown Editor Mono';src:url('{}') format('woff');font-style:normal;font-weight:400;font-display:block;}}\
         @font-face{{font-family:'Markdown Editor Mono';src:url('{}') format('woff');font-style:normal;font-weight:700;font-display:block;}}\
         @font-face{{font-family:'LXGW WenKai Lite';src:url('{}') format('woff');font-style:normal;font-weight:400;font-display:block;}}\
         @font-face{{font-family:'LXGW WenKai Lite';src:url('{}') format('woff');font-style:normal;font-weight:500;font-display:block;}}\
         body,pre,code,blockquote::before,blockquote::after{{font-family:'Markdown Editor Mono','LXGW WenKai Lite','SimHei','DengXian','SimSun','Microsoft YaHei',monospace!important;font-synthesis:weight;}}\
         strong,b{{font-weight:800!important;}}",
        custom_protocol_url("mdfont", "jetbrains-regular.woff"),
        custom_protocol_url("mdfont", "jetbrains-bold.woff"),
        custom_protocol_url("mdfont", "lxgw-regular.woff"),
        custom_protocol_url("mdfont", "lxgw-medium.woff")
    )
}

fn preview_document_url(document_hash: u64) -> String {
    format!(
        "{}?revision={document_hash:016x}",
        custom_protocol_url("mdpreview", "document")
    )
}

fn preview_document_response(
    request: Request<Vec<u8>>,
    document_payload: &Arc<Mutex<Option<Arc<PreviewDocument>>>>,
) -> Response<Cow<'static, [u8]>> {
    let payload = document_payload
        .lock()
        .ok()
        .and_then(|payload| payload.clone());
    let path = request.uri().path();
    let query_value = |key: &str| {
        request.uri().query().and_then(|query| {
            query.split('&').find_map(|part| {
                part.strip_prefix(key)
                    .and_then(|value| value.strip_prefix('='))
            })
        })
    };
    let requested_revision = query_value("revision");
    let revision_matches = payload.as_ref().is_some_and(|payload| {
        requested_revision.is_some_and(|revision| revision == format!("{:016x}", payload.hash))
    });
    if !revision_matches {
        return Response::builder()
            .status(409)
            .header(CONTENT_TYPE, "text/plain; charset=utf-8")
            .header(CACHE_CONTROL, "no-store")
            .body(Cow::Borrowed(&b"stale preview revision"[..]))
            .expect("valid stale preview response");
    }
    let bytes = payload
        .as_ref()
        .and_then(|payload| {
            if path == "/document" {
                Some(payload.shell.as_bytes().to_vec())
            } else if path == "/body" {
                payload
                    .body_range
                    .and_then(|(start, end)| payload.shell.get(start..end))
                    .map(|body| body.as_bytes().to_vec())
            } else if path == "/blocks" {
                (!payload.blocks.is_empty()).then(|| {
                    let requested_ids = query_value("ids").map(|value| {
                        value
                            .split(',')
                            .filter_map(|id| id.parse::<u64>().ok())
                            .collect::<Vec<_>>()
                    });
                    let blocks = if let Some(requested_ids) = requested_ids {
                        requested_ids
                            .iter()
                            .filter_map(|id| {
                                payload.blocks.iter().find(|block| block.block_id == *id)
                            })
                            .collect::<Vec<_>>()
                    } else {
                        let start = query_value("start")
                            .and_then(|value| value.parse::<usize>().ok())
                            .unwrap_or(0)
                            .min(payload.blocks.len());
                        let count = query_value("count")
                            .and_then(|value| value.parse::<usize>().ok())
                            .unwrap_or(payload.blocks.len().saturating_sub(start));
                        payload
                            .blocks
                            .iter()
                            .skip(start)
                            .take(count)
                            .collect::<Vec<_>>()
                    };
                    let blocks = blocks
                        .iter()
                        .map(|block| {
                            serde_json::json!({
                                "id": block.block_id,
                                "html": block.html.as_ref(),
                                "sourceStart": block.source_start,
                                "sourceEnd": block.source_end,
                            })
                        })
                        .collect::<Vec<_>>();
                    serde_json::to_vec(&serde_json::json!({
                        "blocks": blocks,
                        "sourceAnchors": payload.source_anchors(),
                    }))
                    .unwrap_or_default()
                })
            } else if path == "/manifest" {
                payload
                    .virtual_manifest
                    .as_ref()
                    .map(|manifest| manifest.as_bytes().to_vec())
            } else {
                path.strip_prefix("/chunk/")
                    .and_then(|index| index.parse::<usize>().ok())
                    .and_then(|index| payload.chunks.get(index))
                    .map(|chunk| chunk.html.as_bytes().to_vec())
            }
        })
        .unwrap_or_default();
    let status = if bytes.is_empty() { 404 } else { 200 };
    Response::builder()
        .status(status)
        .header(
            CONTENT_TYPE,
            if path == "/manifest" || path == "/blocks" {
                "application/json; charset=utf-8"
            } else {
                "text/html; charset=utf-8"
            },
        )
        .header("X-Content-Type-Options", "nosniff")
        .header(CACHE_CONTROL, "no-store")
        .body(Cow::Owned(bytes))
        .expect("valid preview document response")
}

fn preview_asset_response(request: Request<Vec<u8>>) -> Response<Cow<'static, [u8]>> {
    let (bytes, content_type, cache_control): (&'static [u8], &str, &str) =
        match request.uri().path() {
            "/jetbrains-regular.woff" => (
                JB_MONO_REGULAR_WOFF,
                "font/woff",
                "public, max-age=31536000, immutable",
            ),
            "/jetbrains-bold.woff" => (
                JB_MONO_BOLD_WOFF,
                "font/woff",
                "public, max-age=31536000, immutable",
            ),
            "/lxgw-regular.woff" => (
                LXGW_WENKAI_REGULAR_WOFF,
                "font/woff",
                "public, max-age=31536000, immutable",
            ),
            "/lxgw-medium.woff" => (
                LXGW_WENKAI_MEDIUM_WOFF,
                "font/woff",
                "public, max-age=31536000, immutable",
            ),
            "/mermaid.min.js" => (
                include_bytes!("../assets/mermaid-11.16.0.min.js"),
                "text/javascript; charset=utf-8",
                "public, max-age=31536000, immutable",
            ),
            "/mermaid-init.js" => (
                MERMAID_BOOTSTRAP.as_bytes(),
                "text/javascript; charset=utf-8",
                "no-cache",
            ),
            "/virtual-preview.js" => (
                VIRTUAL_PREVIEW_SCRIPT.as_bytes(),
                "text/javascript; charset=utf-8",
                "no-cache",
            ),
            _ => {
                return Response::builder()
                    .status(404)
                    .body(Cow::Borrowed(&[] as &[u8]))
                    .expect("有效的预览资源 404 响应");
            }
        };
    Response::builder()
        .header(CONTENT_TYPE, content_type)
        .header("X-Content-Type-Options", "nosniff")
        .header(ACCESS_CONTROL_ALLOW_ORIGIN, "*")
        .header(CACHE_CONTROL, cache_control)
        .body(Cow::Borrowed(bytes))
        .expect("有效的预览资源响应")
}

fn preview_font_asset_index(path: &str) -> Option<usize> {
    match path {
        "/jetbrains-regular.woff" => Some(0),
        "/jetbrains-bold.woff" => Some(1),
        "/lxgw-regular.woff" => Some(2),
        "/lxgw-medium.woff" => Some(3),
        _ => None,
    }
}

fn preview_font_asset_sizes() -> [usize; 4] {
    [
        JB_MONO_REGULAR_WOFF.len(),
        JB_MONO_BOLD_WOFF.len(),
        LXGW_WENKAI_REGULAR_WOFF.len(),
        LXGW_WENKAI_MEDIUM_WOFF.len(),
    ]
}

const JB_MONO_REGULAR_WOFF: &[u8] = include_bytes!("../fonts/web/JetBrainsMono-Regular.woff");
const JB_MONO_BOLD_WOFF: &[u8] = include_bytes!("../fonts/web/JetBrainsMono-Bold.woff");
const LXGW_WENKAI_REGULAR_WOFF: &[u8] = include_bytes!("../fonts/web/LXGWWenKaiLite-Regular.woff");
const LXGW_WENKAI_MEDIUM_WOFF: &[u8] = include_bytes!("../fonts/web/LXGWWenKaiLite-Medium.woff");

fn local_image_response(request: Request<Vec<u8>>) -> Response<Cow<'static, [u8]>> {
    let result = request
        .uri()
        .path()
        .strip_prefix('/')
        .and_then(|path| url::Url::parse(&format!("file:///{path}")).ok())
        .and_then(|url| url.to_file_path().ok())
        .filter(|path| supported_image_path(path))
        .and_then(|path| {
            let content_type = image_content_type(&path)?;
            let bytes = crate::io::read_file_limited(&path, crate::io::MAX_IMAGE_FILE_SIZE).ok()?;
            Some((bytes, content_type))
        });

    match result {
        Some((bytes, content_type)) => Response::builder()
            .header(CONTENT_TYPE, content_type)
            .header("X-Content-Type-Options", "nosniff")
            .header(CACHE_CONTROL, "no-cache")
            .body(Cow::Owned(bytes))
            .expect("valid local image response"),
        None => Response::builder()
            .status(404)
            .body(Cow::Borrowed(&[] as &[u8]))
            .expect("valid local image 404 response"),
    }
}

fn supported_image_path(path: &Path) -> bool {
    image_content_type(path).is_some()
}

fn image_content_type(path: &Path) -> Option<&'static str> {
    match path
        .extension()?
        .to_string_lossy()
        .to_ascii_lowercase()
        .as_str()
    {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        "bmp" => Some("image/bmp"),
        "svg" => Some("image/svg+xml"),
        _ => None,
    }
}

const MERMAID_BOOTSTRAP: &str = r#"
(() => {
    const bootstrapUrl = document.currentScript?.src;
    const runtimeUrl = bootstrapUrl ? new URL('mermaid.min.js', bootstrapUrl).href : null;
    let runtimePromise = null;
    let initialized = false;
    let nextId = 0;
    let renderQueue = Promise.resolve();

    const loadRuntime = () => {
      if (window.mermaid) return Promise.resolve(window.mermaid);
      if (runtimePromise) return runtimePromise;
      if (!runtimeUrl) return Promise.reject(new Error('Missing Mermaid runtime URL'));

      runtimePromise = new Promise((resolve, reject) => {
        const script = document.createElement('script');
        script.src = runtimeUrl;
        script.addEventListener('load', () => {
          if (window.mermaid) resolve(window.mermaid);
          else reject(new Error('Mermaid runtime did not initialize'));
        }, { once: true });
        script.addEventListener('error', () => {
          reject(new Error('Unable to load Mermaid runtime'));
        }, { once: true });
        document.head.append(script);
      });
      return runtimePromise;
    };

    const renderMermaid = async (root) => {
      const blocks = Array.from(root.querySelectorAll('pre > code.language-mermaid'));
      if (blocks.length === 0) return;
      const mermaid = await loadRuntime();
      if (!initialized) {
        mermaid.initialize({
          startOnLoad: false,
          securityLevel: 'strict',
          suppressErrorRendering: true,
          fontFamily: "'Markdown Editor Mono', 'LXGW WenKai Lite', monospace"
        });
        initialized = true;
      }
      for (const code of blocks) {
        const source = code.textContent || '';
        const pre = code.parentElement;
        const diagram = document.createElement('div');
        diagram.className = 'mermaid-diagram';
        diagram.setAttribute('role', 'img');
        diagram.setAttribute('aria-label', 'Mermaid diagram');

        try {
            const rendered = await mermaid.render(`markdown-editor-mermaid-${nextId++}`, source);
            diagram.innerHTML = rendered.svg;
            pre.replaceWith(diagram);
            if (rendered.bindFunctions) rendered.bindFunctions(diagram);
        } catch (error) {
            pre.classList.add('mermaid-error');
            pre.setAttribute('data-language', 'Mermaid error');
            const message = document.createElement('div');
            message.className = 'mermaid-error-message';
            message.textContent = `Mermaid 图表语法错误：${error?.message || String(error)}`;
            pre.after(message);
        }
      }
    };

    window.__mdRenderMermaid = (root = document) => {
      renderQueue = renderQueue.catch(() => {}).then(() => renderMermaid(root));
      return renderQueue;
    };
    window.__mdRenderMermaid(document).catch((error) => console.error(error));
})();
"#;

/// 只为浏览器默认没有可用排版的 Markdown 结构提供底线样式。
/// 放在主题 CSS 之前，所以主题只要声明同一属性就会自然覆盖这里。
const STRUCTURAL_FALLBACK: &str = r#"
table { width: 100%; border-collapse: collapse; border-spacing: 0; margin: 0 0 20px; }
th, td { padding: 8px 12px; border: 1px solid rgba(127, 127, 127, .22); text-align: left; }
th { font-weight: 700; }
tbody tr:nth-child(even) { background: rgba(127, 127, 127, .055); }
"#;

/// pulldown-cmark 用 `<pre><code>` 表示代码块，而这类传统 Web 主题通常按
/// `<pre>` 单层结构编写。这里只消除内部 `code` 包装造成的重复样式。
const MARKDOWN_DOM_COMPATIBILITY: &str = r#"
pre > code { color: inherit; background: transparent; border-radius: 0; font-family: inherit; padding: 0; font-size: inherit; }
pre[data-language] { position: relative; }
pre[data-language] > code { display: block; padding-right: 5.5em; }
pre[data-language]::before {
    content: attr(data-language);
    position: absolute;
    top: 8px;
    right: 12px;
    color: #aaa;
    font-size: 10px;
    font-weight: 400;
    line-height: 1;
    letter-spacing: .08em;
    text-transform: uppercase;
    pointer-events: none;
}
.mermaid-diagram {
    display: flex;
    justify-content: center;
    width: 100%;
    margin: 1.5em 0;
    overflow-x: auto;
}
.mermaid-diagram svg { display: block; max-width: 100%; height: auto; }
pre.mermaid-error { border-color: rgba(220, 70, 70, .45); }
.mermaid-error-message {
    margin: -.75em 0 1.5em;
    color: #b42318;
    font-size: .85em;
    white-space: pre-wrap;
}
ol:not(#footnotes), ul {
    padding-inline-start: clamp(1.5em, 3vw, 2.25em) !important;
}
ol:not(#footnotes) > li::marker {
    font-variant-numeric: tabular-nums;
}
html { scrollbar-width: none; }
::-webkit-scrollbar { width: 0; height: 0; }
@media (max-width: 700px) {
    body > h1:first-child { margin-top: 0; margin-bottom: 10px; }
    body > h1:first-child + p { margin-bottom: 12px; }
    body > h1:first-child + p + ul,
    body > h1:first-child + p + ol { margin-top: 0; }
    li { margin-top: .45em; margin-bottom: .45em; }
}
"#;

fn physical_bounds(rect: egui::Rect, pixels_per_point: f32) -> [i32; 4] {
    let scale = pixels_per_point.max(0.1);
    [
        (rect.min.x * scale).round() as i32,
        (rect.min.y * scale).round() as i32,
        (rect.width() * scale).round().max(1.0) as i32,
        (rect.height() * scale).round().max(1.0) as i32,
    ]
}

fn to_wry_rect(bounds: [i32; 4]) -> Rect {
    Rect {
        position: PhysicalPosition::new(bounds[0], bounds[1]).into(),
        size: PhysicalSize::new(bounds[2] as u32, bounds[3] as u32).into(),
    }
}

fn hash(value: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    value.hash(&mut hasher);
    hasher.finish()
}

fn escape_attribute(value: &str) -> String {
    value.replace('&', "&amp;").replace('"', "&quot;")
}

/// 把围栏代码的 `language-*` 类同步到 `<pre data-language>`，供右上角标签显示。
fn annotate_code_languages(body: &mut String) {
    const OPEN: &str = "<pre><code class=\"language-";
    let source = body.as_str();
    let mut output = String::with_capacity(source.len() + source.len() / 64);
    let mut search_from = 0;
    while let Some(relative_start) = source[search_from..].find(OPEN) {
        let pre_start = search_from + relative_start;
        output.push_str(&source[search_from..pre_start]);
        let language_start = pre_start + OPEN.len();
        let Some(language_end_relative) = source[language_start..].find('"') else {
            output.push_str(&source[pre_start..]);
            search_from = source.len();
            break;
        };
        let language_end = language_start + language_end_relative;
        let language = source[language_start..language_end].trim();
        output.push_str("<pre");
        if !language.is_empty() {
            output.push_str(" data-language=\"");
            output.push_str(language);
            output.push('"');
        }
        output.push_str(&source[pre_start + "<pre".len()..language_end + 1]);
        search_from = language_end + 1;
    }
    output.push_str(&source[search_from..]);
    *body = output;
}

/// pulldown-cmark 输出 `.footnote-definition`，而传统 Markdown Web 主题通常
/// 使用 `ol#footnotes > li`。转换结构后，主题原有选择器可以直接生效。
fn normalize_footnote_dom(body: &mut String) {
    const OPEN: &str = "<div class=\"footnote-definition\" id=\"";
    const LABEL: &str = "<sup class=\"footnote-definition-label\">";
    let mut items = Vec::new();

    while let Some(start) = body.find(OPEN) {
        let id_start = start + OPEN.len();
        let Some(id_end_rel) = body[id_start..].find("\">") else {
            break;
        };
        let id_end = id_start + id_end_rel;
        let content_start = id_end + 2;
        let Some(close_rel) = body[content_start..].find("</div>") else {
            break;
        };
        let close_start = content_start + close_rel;
        let close_end = close_start + "</div>".len();

        let id = body[id_start..id_end].to_string();
        let mut content = body[content_start..close_start].to_string();
        if content.starts_with(LABEL)
            && let Some(label_end) = content.find("</sup>")
        {
            content.replace_range(..label_end + "</sup>".len(), "");
            content = content.trim_start_matches(['\r', '\n']).to_string();
        }
        items.push(format!("<li id=\"{id}\">{content}</li>"));
        body.replace_range(start..close_end, "");
    }

    if !items.is_empty() {
        body.push_str("<ol id=\"footnotes\">\n");
        for item in items {
            body.push_str(&item);
            body.push('\n');
        }
        body.push_str("</ol>\n");
    }
}

#[cfg(test)]
mod tests {
    use super::{
        JB_MONO_BOLD_WOFF, JB_MONO_REGULAR_WOFF, LXGW_WENKAI_MEDIUM_WOFF, LXGW_WENKAI_REGULAR_WOFF,
        MERMAID_BOOTSTRAP, SCROLL_SYNC_SCRIPT, VIRTUAL_PREVIEW_SCRIPT,
        custom_protocol_script_source, custom_protocol_url, document, local_image_response,
        local_image_url, parse_anchor_message, parse_ready_message, parse_source_message,
        preview_asset_response, source_line_at_byte, source_line_starts,
    };
    use wry::http::Request;

    #[test]
    fn closing_preview_releases_retained_document_payload() {
        let parsed = crate::markdown::parse_document("# 标题\n\n正文\n");
        let document = std::sync::Arc::new(super::preview_document(&parsed, "", None, None, None));
        let mut preview = super::BrowserPreview {
            document_source: Some(std::sync::Arc::clone(&document)),
            ..Default::default()
        };
        *preview.document_payload.lock().expect("document payload") =
            Some(std::sync::Arc::clone(&document));

        assert_eq!(std::sync::Arc::strong_count(&document), 3);
        preview.close();

        assert_eq!(std::sync::Arc::strong_count(&document), 1);
    }

    fn render_document(
        markdown: &str,
        css: &str,
        base_directory: Option<&std::path::Path>,
        font_size_override: Option<f32>,
    ) -> String {
        let parsed = crate::markdown::parse_document(markdown);
        document(&parsed, css, base_directory, font_size_override, None)
    }

    #[test]
    fn changing_documents_discards_stale_scroll_messages() {
        let preview = super::BrowserPreview::default();
        {
            let mut bridge = preview.scroll_bridge.lock().expect("scroll bridge");
            bridge.source_position = Some(18.0);
            bridge.user_source_position = Some(18.0);
        }

        preview.reset_scroll_bridge_for_document();

        let bridge = preview.scroll_bridge.lock().expect("scroll bridge");
        assert_eq!(bridge.source_position, None);
        assert_eq!(bridge.user_source_position, None);
        assert_eq!(bridge.user_anchor, None);
    }

    #[test]
    fn changing_documents_resets_the_local_image_request_counter() {
        let preview = super::BrowserPreview::default();
        preview
            .local_image_requests
            .store(7, std::sync::atomic::Ordering::Relaxed);

        preview.reset_scroll_bridge_for_document();

        assert_eq!(preview.local_image_request_count(), 0);
    }

    #[test]
    fn changing_documents_resets_the_mermaid_runtime_request_counter() {
        let preview = super::BrowserPreview::default();
        preview
            .mermaid_runtime_requests
            .store(3, std::sync::atomic::Ordering::Relaxed);

        preview.reset_scroll_bridge_for_document();

        assert_eq!(preview.mermaid_runtime_request_count(), 0);
    }

    #[test]
    fn changing_documents_resets_font_asset_request_counters() {
        let preview = super::BrowserPreview::default();
        for requests in preview.font_asset_requests.iter() {
            requests.store(2, std::sync::atomic::Ordering::Relaxed);
        }

        preview.reset_scroll_bridge_for_document();

        assert_eq!(preview.font_asset_request_counts(), [0; 4]);
        assert_eq!(preview.font_asset_requested_bytes(), 0);
    }

    #[test]
    fn source_line_index_maps_byte_offsets_without_rescanning_the_document() {
        let source = "第一行\nsecond\n第三行";
        let starts = source_line_starts(source);
        assert_eq!(starts, vec![0, 10, 17]);
        assert_eq!(source_line_at_byte(&starts, 0), 0.0);
        assert_eq!(source_line_at_byte(&starts, 9), 0.0);
        assert_eq!(source_line_at_byte(&starts, 10), 1.0);
        assert_eq!(source_line_at_byte(&starts, source.len()), 2.0);
    }

    #[test]
    fn large_preview_is_split_into_virtual_chunks_without_wrapping_theme_nodes() {
        let markdown = "## 章节\n\n正文段落。\n\n".repeat(30_000);
        let parsed = crate::markdown::parse_document(&markdown);
        let preview =
            super::preview_document(&parsed, "body > h2 { color: red; }", None, None, None);
        assert!(preview.chunks.len() > 2);
        assert!(preview.shell.contains("md-virtual-manifest"));
        assert!(preview.shell.contains("virtual-preview.js"));
        assert!(!preview.shell.contains("正文段落"));
        assert!(preview.chunks[0].html.contains("<h2 id=\"md-heading-0\""));
        assert_eq!(preview.chunks[0].heading_start, Some(0));
        assert!(preview.chunks[0].heading_end.unwrap() > 0);
        assert!(VIRTUAL_PREVIEW_SCRIPT.contains("scrollHeading"));
        assert!(VIRTUAL_PREVIEW_SCRIPT.contains("window.__mdRenderMermaid"));
        assert!(VIRTUAL_PREVIEW_SCRIPT.contains("unload(chunk)"));
        assert!(
            preview
                .virtual_manifest
                .as_ref()
                .unwrap()
                .contains("blockId")
        );
        assert!(
            preview
                .virtual_manifest
                .as_ref()
                .unwrap()
                .contains("sourceAnchors")
        );
    }

    #[test]
    fn image_dense_preview_is_virtualized_even_when_markdown_is_small() {
        let markdown = (1..=20)
            .map(|index| format!("![图片 {index}](images/{index}.png)\n\n"))
            .collect::<String>();
        let parsed = crate::markdown::parse_document(&markdown);
        let preview = super::preview_document(&parsed, "", None, None, None);

        assert!(preview.total_bytes < 512 * 1024);
        assert!(preview.chunks.len() > 1);
        assert!(preview.shell.contains("md-virtual-manifest"));
        assert!(preview.chunks[0].html.matches("<img").count() <= 4);
        assert!(preview.chunks[0].estimated_height >= 4.0 * 480.0);
    }

    #[test]
    fn a_few_images_keep_the_direct_preview_path() {
        let parsed =
            crate::markdown::parse_document("![图片 1](images/1.png)\n\n![图片 2](images/2.png)\n");
        let preview = super::preview_document(&parsed, "", None, None, None);

        assert!(preview.chunks.is_empty());
        assert!(!preview.shell.contains("md-virtual-manifest"));
    }

    #[test]
    fn ordinary_documents_can_update_body_without_navigation() {
        let first = crate::markdown::parse_document("# 第一版\n\n正文");
        let second = crate::markdown::parse_document("# 第二版\n\n正文");
        let first = super::preview_document(&first, "body { color: black; }", None, None, None);
        let second = super::preview_document(&second, "body { color: black; }", None, None, None);

        assert!(first.body_range.is_some());
        assert!(second.body_range.is_some());
        assert!(first.can_patch_body_into(&second));
        assert!(first.can_patch_into(&second));
        let patch = first.body_patch_into(&second).expect("body patch");
        assert_eq!(patch.start, 1);
        assert_eq!(patch.insert_count, 1);
        assert!(SCROLL_SYNC_SCRIPT.contains("window.__mdEditorPatchBody"));
        assert!(SCROLL_SYNC_SCRIPT.contains("/blocks?revision="));
        assert!(SCROLL_SYNC_SCRIPT.contains("captureViewportAnchor"));
        assert!(SCROLL_SYNC_SCRIPT.contains("restoreViewportAnchor"));
        assert!(SCROLL_SYNC_SCRIPT.contains("blockPatched && restoreViewportAnchor"));
        assert!(SCROLL_SYNC_SCRIPT.contains("patch.anchorMap"));
        assert!(SCROLL_SYNC_SCRIPT.contains("const isBoundaryComment"));
        assert!(SCROLL_SYNC_SCRIPT.contains("range.setStartAfter(marker)"));
    }

    #[test]
    fn source_only_shifts_refresh_anchors_without_replacing_unchanged_blocks() {
        let first = crate::markdown::parse_document("# 标题\n\n第一段\n\n第二段");
        let second = crate::markdown::parse_document("\n# 标题\n\n第一段\n\n第二段");
        let first = super::preview_document(&first, "", None, None, None);
        let second = super::preview_document(&second, "", None, None, None);

        let patch = first
            .body_patch_into(&second)
            .expect("ordinary documents should support an in-place patch");

        assert_eq!(patch.start, first.blocks.len());
        assert_eq!(patch.delete_count, 0);
        assert_eq!(patch.insert_count, 0);
        assert_ne!(first.source_anchors(), second.source_anchors());
    }

    #[test]
    fn source_only_edits_reuse_rendered_html_without_full_html_generation() {
        let first = crate::markdown::parse_document("# 标题\n\n第一段\n\n第二段");
        let second = crate::markdown::parse_document("\n# 标题\n\n第一段\n\n第二段");
        let first = super::preview_document(&first, "", None, None, None);
        let reused = super::preview_document_with_previous(Some(&first), &second);

        assert!(reused.is_some());
        assert_eq!(reused.unwrap().source_anchors(), {
            let parsed = crate::markdown::parse_document("\n# 标题\n\n第一段\n\n第二段");
            super::preview_document(&parsed, "", None, None, None).source_anchors()
        });
    }

    #[test]
    fn incremental_preview_rebuilds_only_changed_simple_block() {
        let first_document =
            crate::markdown::parse_document("# 标题\n\n第一段\n\n```rust\nlet a = 1;\n```\n");
        let next_document =
            crate::markdown::parse_document("# 标题\n\n修改后的段落\n\n```rust\nlet a = 1;\n```\n");
        let first_preview = super::preview_document(&first_document, "", None, None, None);
        let incremental = super::preview_document_incremental(
            Some(&first_preview),
            Some(&first_document),
            &next_document,
            None,
        )
        .expect("simple block edit should reuse the preview shell");
        let full = super::preview_document(&next_document, "", None, None, None);

        assert_eq!(incremental.shell, full.shell);
        assert_eq!(incremental.source_anchors(), full.source_anchors());
    }

    #[test]
    fn incremental_preview_supports_inserting_a_top_level_block() {
        let first_document = crate::markdown::parse_document("# 一\n\n甲\n\n乙\n");
        let next_document = crate::markdown::parse_document("# 一\n\n甲\n\n新增\n\n乙\n");
        let first_preview = super::preview_document(&first_document, "", None, None, None);

        let incremental = super::preview_document_incremental(
            Some(&first_preview),
            Some(&first_document),
            &next_document,
            None,
        )
        .expect("inserting a paragraph should patch the preview locally");
        let full = super::preview_document(&next_document, "", None, None, None);

        assert_eq!(incremental.shell, full.shell);
        assert_eq!(incremental.source_anchors(), full.source_anchors());
    }

    #[test]
    fn incremental_preview_supports_deleting_a_top_level_block() {
        let first_document = crate::markdown::parse_document("# 一\n\n甲\n\n删除\n\n乙\n");
        let next_document = crate::markdown::parse_document("# 一\n\n甲\n\n乙\n");
        let first_preview = super::preview_document(&first_document, "", None, None, None);

        let incremental = super::preview_document_incremental(
            Some(&first_preview),
            Some(&first_document),
            &next_document,
            None,
        )
        .expect("deleting a paragraph should patch the preview locally");
        let full = super::preview_document(&next_document, "", None, None, None);

        assert_eq!(incremental.shell, full.shell);
        assert_eq!(incremental.source_anchors(), full.source_anchors());
    }

    #[test]
    fn incremental_preview_supports_deleting_the_last_block() {
        let first_document = crate::markdown::parse_document("最后一段\n");
        let next_document = crate::markdown::parse_document("");
        let first_preview = super::preview_document(&first_document, "", None, None, None);

        let incremental = super::preview_document_incremental(
            Some(&first_preview),
            Some(&first_document),
            &next_document,
            None,
        )
        .expect("deleting the last block should patch the preview body locally");
        let full = super::preview_document(&next_document, "", None, None, None);

        assert_eq!(incremental.shell, full.shell);
        assert_eq!(incremental.source_anchors(), full.source_anchors());
    }

    #[test]
    fn incremental_preview_rebuilds_changed_table_block_without_navigation() {
        let first_document = crate::markdown::parse_document(
            "# 标题\n\n| 字段 | 值 |\n| --- | --- |\n| 一 | 旧 |\n",
        );
        let next_document = crate::markdown::parse_document(
            "# 标题\n\n| 字段 | 值 |\n| --- | --- |\n| 一 | 新 |\n",
        );
        let first_preview = super::preview_document(&first_document, "", None, None, None);
        let incremental = super::preview_document_incremental(
            Some(&first_preview),
            Some(&first_document),
            &next_document,
            None,
        )
        .expect("table edit should reuse the preview shell");
        let full = super::preview_document(&next_document, "", None, None, None);

        assert!(incremental.shell.contains("<td>新</td>"));
        assert!(incremental.body_patch_into(&full).is_some());
        assert_eq!(incremental.source_anchors(), full.source_anchors());
    }

    #[test]
    fn incremental_preview_rebuilds_changed_list_block_without_navigation() {
        let first_document = crate::markdown::parse_document("# 标题\n\n- 第一项\n- 旧内容\n");
        let next_document = crate::markdown::parse_document("# 标题\n\n- 第一项\n- 新内容\n");
        let first_preview = super::preview_document(&first_document, "", None, None, None);
        let incremental = super::preview_document_incremental(
            Some(&first_preview),
            Some(&first_document),
            &next_document,
            None,
        )
        .expect("list edit should reuse the preview shell");
        let full = super::preview_document(&next_document, "", None, None, None);

        assert_eq!(incremental.shell, full.shell);
        assert_eq!(incremental.source_anchors(), full.source_anchors());
    }

    #[test]
    fn incremental_preview_supports_paragraph_to_list_conversion() {
        let first_document = crate::markdown::parse_document("前文\n\n原始段落\n\n后文\n");
        let next_document = crate::markdown::parse_document("前文\n\n- 第一项\n- 第二项\n\n后文\n");
        let first_preview = super::preview_document(&first_document, "", None, None, None);

        let incremental = super::preview_document_incremental(
            Some(&first_preview),
            Some(&first_document),
            &next_document,
            None,
        )
        .expect("paragraph-to-list conversion should patch the changed block locally");
        let full = super::preview_document(&next_document, "", None, None, None);

        assert_eq!(incremental.shell, full.shell);
        assert_eq!(incremental.source_anchors(), full.source_anchors());
    }

    #[test]
    fn incremental_preview_supports_paragraph_to_quote_conversion() {
        let first_document = crate::markdown::parse_document("前文\n\n原始段落\n\n后文\n");
        let next_document = crate::markdown::parse_document("前文\n\n> 引用段落\n\n后文\n");
        let first_preview = super::preview_document(&first_document, "", None, None, None);

        let incremental = super::preview_document_incremental(
            Some(&first_preview),
            Some(&first_document),
            &next_document,
            None,
        )
        .expect("paragraph-to-quote conversion should patch the changed block locally");
        let full = super::preview_document(&next_document, "", None, None, None);

        assert_eq!(incremental.shell, full.shell);
        assert_eq!(incremental.source_anchors(), full.source_anchors());
    }

    #[test]
    fn incremental_preview_supports_paragraph_to_table_conversion() {
        let first_document = crate::markdown::parse_document("前文\n\n原始段落\n\n后文\n");
        let next_document = crate::markdown::parse_document(
            "前文\n\n| 字段 | 值 |\n| --- | --- |\n| 一 | 新 |\n\n后文\n",
        );
        let first_preview = super::preview_document(&first_document, "", None, None, None);

        let incremental = super::preview_document_incremental(
            Some(&first_preview),
            Some(&first_document),
            &next_document,
            None,
        )
        .expect("paragraph-to-table conversion should patch the changed block locally");
        let full = super::preview_document(&next_document, "", None, None, None);

        assert_eq!(incremental.shell, full.shell);
        assert_eq!(incremental.source_anchors(), full.source_anchors());
    }

    #[test]
    fn incremental_preview_rebuilds_raw_html_block_conversion() {
        let first_document = crate::markdown::parse_document("<div>旧内容</div>\n\n后文\n");
        let next_document =
            crate::markdown::parse_document("<div><span>新内容</span></div>\n\n后文\n");
        let first_preview = super::preview_document(&first_document, "", None, None, None);

        let incremental = super::preview_document_incremental(
            Some(&first_preview),
            Some(&first_document),
            &next_document,
            None,
        )
        .expect("raw HTML block edits should remain local");
        let full = super::preview_document(&next_document, "", None, None, None);

        assert_eq!(incremental.shell, full.shell);
        assert_eq!(incremental.source_anchors(), full.source_anchors());
    }

    #[test]
    fn incremental_preview_preserves_raw_html_ids_when_reusing_unchanged_block() {
        let first_document = crate::markdown::parse_document(
            "<div id=\"md-heading-99\">内容</div>\n\n# 标题\n\n旧段落\n",
        );
        let next_document = crate::markdown::parse_document(
            "<div id=\"md-heading-99\">内容</div>\n\n# 标题\n\n新段落\n",
        );
        let first_preview = super::preview_document(&first_document, "", None, None, None);
        let incremental = super::preview_document_incremental(
            Some(&first_preview),
            Some(&first_document),
            &next_document,
            None,
        )
        .expect("unrelated raw HTML should be safely reused");
        let full = super::preview_document(&next_document, "", None, None, None);

        assert_eq!(incremental.shell, full.shell);
    }

    #[test]
    fn incremental_preview_falls_back_for_footnote_definition_edits() {
        let first_document = crate::markdown::parse_document("正文[^1]\n\n[^1]: 旧脚注\n\n后文\n");
        let next_document = crate::markdown::parse_document("正文[^1]\n\n[^1]: 新脚注\n\n后文\n");
        let first_preview = super::preview_document(&first_document, "", None, None, None);

        assert!(
            super::preview_document_incremental(
                Some(&first_preview),
                Some(&first_document),
                &next_document,
                None,
            )
            .is_none()
        );
    }

    #[test]
    fn incremental_preview_supports_paragraph_to_heading_conversion_and_updates_following_ids() {
        let first_document =
            crate::markdown::parse_document("前文\n\n原始段落\n\n## 后续章节\n\n后文\n");
        let next_document =
            crate::markdown::parse_document("前文\n\n# 新章节\n\n## 后续章节\n\n后文\n");
        let first_preview = super::preview_document(&first_document, "", None, None, None);

        let incremental = super::preview_document_incremental(
            Some(&first_preview),
            Some(&first_document),
            &next_document,
            None,
        )
        .expect("paragraph-to-heading conversion should patch affected heading blocks locally");
        let full = super::preview_document(&next_document, "", None, None, None);

        assert_eq!(incremental.shell, full.shell);
        assert_eq!(incremental.source_anchors(), full.source_anchors());
    }

    #[test]
    fn incremental_preview_supports_inserting_a_heading_block() {
        let first_document = crate::markdown::parse_document("前文\n\n## 后续章节\n\n后文\n");
        let next_document =
            crate::markdown::parse_document("前文\n\n# 新章节\n\n## 后续章节\n\n后文\n");
        let first_preview = super::preview_document(&first_document, "", None, None, None);

        let incremental = super::preview_document_incremental(
            Some(&first_preview),
            Some(&first_document),
            &next_document,
            None,
        )
        .expect("heading insertion should render the affected suffix locally");
        let full = super::preview_document(&next_document, "", None, None, None);

        assert_eq!(incremental.shell, full.shell);
        assert_eq!(incremental.source_anchors(), full.source_anchors());
    }

    #[test]
    fn incremental_preview_supports_deleting_a_heading_block() {
        let first_document =
            crate::markdown::parse_document("前文\n\n# 已删除章节\n\n## 后续章节\n\n后文\n");
        let next_document = crate::markdown::parse_document("前文\n\n## 后续章节\n\n后文\n");
        let first_preview = super::preview_document(&first_document, "", None, None, None);

        let incremental = super::preview_document_incremental(
            Some(&first_preview),
            Some(&first_document),
            &next_document,
            None,
        )
        .expect("heading deletion should render the affected suffix locally");
        let full = super::preview_document(&next_document, "", None, None, None);

        assert_eq!(incremental.shell, full.shell);
        assert_eq!(incremental.source_anchors(), full.source_anchors());
    }

    #[test]
    fn nested_heading_count_change_updates_the_parent_block_incrementally() {
        let first_document = crate::markdown::parse_document("> # 嵌套标题\n");
        let next_document = crate::markdown::parse_document("> 正文\n");
        let first_preview = super::preview_document(&first_document, "", None, None, None);

        let incremental = super::preview_document_incremental(
            Some(&first_preview),
            Some(&first_document),
            &next_document,
            None,
        )
        .expect("nested heading changes should patch the complete parent block");
        let full = super::preview_document(&next_document, "", None, None, None);

        assert_eq!(incremental.shell, full.shell);
        assert_eq!(incremental.source_anchors(), full.source_anchors());
    }

    #[test]
    fn virtual_incremental_preview_rebuilds_changed_chunk() {
        let first_source = "## 章节\n\n正文段落。\n\n".repeat(15_000);
        let next_source = first_source.replacen("正文段落。", "修改后的正文段落。", 1);
        let first_document = crate::markdown::parse_document(&first_source);
        let next_document = crate::markdown::parse_document(&next_source);
        let first_preview = super::preview_document(&first_document, "", None, None, None);
        assert!(first_preview.virtual_manifest.is_some());

        let incremental = super::preview_document_virtual_incremental(
            Some(&first_preview),
            Some(&first_document),
            &next_document,
            None,
        )
        .expect("simple virtual chunk edit should be incremental");
        let full = super::preview_document(&next_document, "", None, None, None);

        assert_eq!(incremental.chunks.len(), full.chunks.len());
        let incremental_ids = incremental
            .chunks
            .iter()
            .map(|chunk| chunk.block_id)
            .collect::<Vec<_>>();
        let full_ids = full
            .chunks
            .iter()
            .map(|chunk| chunk.block_id)
            .collect::<Vec<_>>();
        let first_difference = incremental_ids
            .iter()
            .zip(&full_ids)
            .position(|(left, right)| left != right);
        assert_eq!(
            incremental_ids, full_ids,
            "first differing chunk: {first_difference:?}"
        );
        for chunk in full.chunks.iter() {
            assert_eq!(
                Some(chunk.block_id),
                super::block_id_in_html(&chunk.html),
                "virtual chunk IDs must inherit a stable document block marker"
            );
        }
        assert_eq!(incremental.virtual_manifest, full.virtual_manifest);
    }

    #[test]
    fn virtual_incremental_preview_updates_following_heading_ids() {
        let first_source = "## 章节\n\n正文段落。\n\n".repeat(15_000);
        let next_source = first_source.replacen("正文段落。", "# 新标题", 1);
        let first_document = crate::markdown::parse_document(&first_source);
        let next_document = crate::markdown::parse_document(&next_source);
        let first_preview = super::preview_document(&first_document, "", None, None, None);
        assert!(first_preview.virtual_manifest.is_some());

        let incremental = super::preview_document_virtual_incremental(
            Some(&first_preview),
            Some(&first_document),
            &next_document,
            None,
        )
        .expect("heading count changes should patch affected virtual chunks locally");
        let full = super::preview_document(&next_document, "", None, None, None);

        assert_eq!(incremental.virtual_manifest, full.virtual_manifest);
        assert_eq!(incremental.chunks.len(), full.chunks.len());
        for (incremental_chunk, full_chunk) in incremental.chunks.iter().zip(full.chunks.iter()) {
            assert_eq!(incremental_chunk.html, full_chunk.html);
        }
    }

    #[test]
    fn virtual_chunk_boundary_drift_forces_full_rebuild() {
        let old = format!(
            "<!--md-editor-source:0-->{}<!--md-editor-source:1-->tail",
            "a".repeat(95 * 1024)
        );
        let new = format!(
            "<!--md-editor-source:0-->{}<!--md-editor-source:1-->tail",
            "a".repeat(100 * 1024)
        );

        assert!(!super::virtual_chunk_boundary_stable(&old, &new, true));
    }

    #[test]
    fn incremental_preview_falls_back_when_edit_crosses_virtualization_threshold() {
        let first_source = format!("{}\n", "a".repeat(500 * 1024));
        let next_source = format!("{}{}\n", "a".repeat(500 * 1024), "b".repeat(32 * 1024));
        let first_document = crate::markdown::parse_document(&first_source);
        let next_document = crate::markdown::parse_document(&next_source);
        let first_preview = super::preview_document(&first_document, "", None, None, None);

        assert!(first_preview.virtual_manifest.is_none());
        assert!(
            super::preview_document_incremental(
                Some(&first_preview),
                Some(&first_document),
                &next_document,
                None,
            )
            .is_none()
        );
    }

    #[test]
    fn generated_source_anchor_sequence_matches_rendered_blocks() {
        for markdown in [
            "# 标题\n\n正文\n",
            "---\n\n段落\n",
            "> 引用\n\n- 一\n- 二\n",
            "```rust\nfn main() {}\n```\n",
        ] {
            let parsed = crate::markdown::parse_document(markdown);
            let preview = super::preview_document(&parsed, "", None, None, None);
            assert_eq!(
                preview.source_anchors(),
                super::document_source_anchors(&parsed),
                "source anchors for {markdown:?}"
            );
        }
    }

    #[test]
    fn top_level_block_ids_survive_unrelated_insertion() {
        let first = crate::markdown::parse_document("# A\n\n正文\n");
        let next = crate::markdown::parse_document("# 新章节\n\n# A\n\n正文\n");
        let first_ids = super::stable_top_level_block_ids(&first);
        let next_ids = super::stable_top_level_block_ids(&next);

        assert_eq!(first_ids[0], next_ids[1]);
        assert_eq!(first_ids[1], next_ids[2]);
    }

    #[test]
    fn rendered_top_level_blocks_include_stable_identity_markers() {
        let document = crate::markdown::parse_document("# 标题\n\n正文\n\n---\n");
        let ids = super::stable_top_level_block_ids(&document);
        let html = super::document(&document, "", None, None, None);
        let preview = super::preview_document(&document, "", None, None, None);

        assert_eq!(ids.len(), 3);
        assert_eq!(preview.top_level_block_ids(), ids.as_slice());
        assert_eq!(
            preview
                .blocks
                .iter()
                .skip(1)
                .map(|block| block.block_id)
                .collect::<Vec<_>>(),
            ids
        );
        for id in ids {
            assert!(html.contains(&format!("<!--md-editor-block:{id}-->")));
        }
    }

    #[test]
    fn raw_html_comments_cannot_be_confused_with_dom_bridge_markers() {
        let document = crate::markdown::parse_document("<!--md-source:42-->\n\n正文\n");
        let html = super::document(&document, "", None, None, None);

        assert!(html.contains("<!--md-source:42-->"));
        assert_eq!(
            super::source_marker_ranges(&html).len(),
            super::document_source_anchors(&document).len()
        );
        assert!(!html.contains("<!--md-editor-block:42-->"));
    }

    #[test]
    fn block_identity_markers_do_not_change_content_hash() {
        assert_eq!(
            super::hash_source_markerless(
                "<!--md-editor-source:1--><!--md-editor-block:1--><p>x</p>"
            ),
            super::hash_source_markerless(
                "<!--md-editor-source:1--><!--md-editor-block:2--><p>x</p>"
            ),
        );
    }

    #[test]
    fn structural_edits_fall_back_to_full_preview_rendering() {
        let first = crate::markdown::parse_document("# 标题\n\n第一段");
        let second = crate::markdown::parse_document("# 标题\n\n第二段");
        let first = super::preview_document(&first, "", None, None, None);
        assert!(super::preview_document_with_previous(Some(&first), &second).is_none());
    }

    #[test]
    fn inserted_blocks_patch_only_the_changed_middle_range() {
        let first = crate::markdown::parse_document("# 标题\n\n第一段\n\n第三段");
        let second = crate::markdown::parse_document("# 标题\n\n第一段\n\n第二段\n\n第三段");
        let first = super::preview_document(&first, "", None, None, None);
        let second = super::preview_document(&second, "", None, None, None);

        let patch = first
            .body_patch_into(&second)
            .expect("ordinary documents should support an in-place patch");

        assert_eq!(patch.delete_count, 0);
        assert_eq!(patch.insert_count, 1);
        assert!(patch.start > 0);
        assert!(patch.start < second.blocks.len());
    }

    #[test]
    fn body_patch_maps_unchanged_blocks_when_their_order_changes() {
        let first = crate::markdown::parse_document("# 一\n\n甲\n\n# 二\n\n乙");
        let second = crate::markdown::parse_document("# 二\n\n乙\n\n# 一\n\n甲");
        let first = super::preview_document(&first, "", None, None, None);
        let second = super::preview_document(&second, "", None, None, None);

        let patch = first.body_patch_into(&second).expect("body patch");
        assert_eq!(patch.anchor_map[1], Some(3));
        assert_eq!(patch.anchor_map[3], Some(1));
    }

    #[test]
    fn virtual_documents_can_update_their_manifest_without_navigation() {
        let first = crate::markdown::parse_document(&"## 第一版\n\n正文。\n\n".repeat(30_000));
        let second = crate::markdown::parse_document(&"## 第二版\n\n正文。\n\n".repeat(30_000));
        let first = super::preview_document(&first, "body { color: black; }", None, None, None);
        let second = super::preview_document(&second, "body { color: black; }", None, None, None);

        assert!(first.virtual_manifest.is_some());
        assert!(second.virtual_manifest.is_some());
        assert!(first.can_patch_virtual_into(&second));
        assert!(first.can_patch_into(&second));
        assert!(VIRTUAL_PREVIEW_SCRIPT.contains("/manifest?revision="));
        assert!(VIRTUAL_PREVIEW_SCRIPT.contains("const patch = async"));
        assert!(VIRTUAL_PREVIEW_SCRIPT.contains("refreshChunkAnchors"));
        assert!(VIRTUAL_PREVIEW_SCRIPT.contains("patchLoadedChunkBlocks"));
        assert!(VIRTUAL_PREVIEW_SCRIPT.contains("loadedBlockElements"));
        assert!(VIRTUAL_PREVIEW_SCRIPT.contains("candidate.blockIds?.some"));
        assert!(VIRTUAL_PREVIEW_SCRIPT.contains("rect.height"));
        assert!(VIRTUAL_PREVIEW_SCRIPT.contains("blockHashes"));
        assert!(VIRTUAL_PREVIEW_SCRIPT.contains("blockId"));
        assert!(
            VIRTUAL_PREVIEW_SCRIPT.contains("captureViewportAnchor: captureVirtualViewportAnchor")
        );
        assert!(VIRTUAL_PREVIEW_SCRIPT.contains("restoreVirtualViewportAnchor"));
        assert!(first.virtual_manifest.as_deref().is_some_and(|manifest| {
            manifest.contains("\"blockIds\"") && manifest.contains("\"blockHashes\"")
        }));
    }

    #[test]
    fn virtual_chunk_manifest_exposes_stable_block_metadata() {
        let html = "<!--md-editor-source:0--><!--md-editor-block:11--><p>A</p><!--md-editor-source:1--><!--md-editor-block:22--><p>B</p>";
        let (ids, hashes) = super::virtual_chunk_block_metadata(html);

        assert_eq!(ids, vec![11, 22]);
        assert_eq!(hashes.len(), ids.len());
        assert_ne!(hashes[0], hashes[1]);
        assert_eq!(
            hashes[0],
            super::hash_source_markerless("<p>A</p><!--md-editor-source:1-->")
        );
    }

    #[test]
    fn theme_changes_still_require_a_full_navigation() {
        let parsed = crate::markdown::parse_document("# 标题\n\n正文");
        let first = super::preview_document(&parsed, "body { color: black; }", None, None, None);
        let second = super::preview_document(&parsed, "body { color: white; }", None, None, None);

        assert!(!first.can_patch_into(&second));
    }

    #[test]
    fn preview_protocol_rejects_stale_virtual_chunk_revisions() {
        let parsed = crate::markdown::parse_document(&"## 章节\n\n正文。\n\n".repeat(30_000));
        let document = std::sync::Arc::new(super::preview_document(
            &parsed,
            "body { color: black; }",
            None,
            None,
            None,
        ));
        let payload = std::sync::Arc::new(std::sync::Mutex::new(Some(document.clone())));
        let stale_request = Request::builder()
            .uri("https://mdpreview.localhost/chunk/0?revision=0000000000000000")
            .body(Vec::new())
            .expect("stale chunk request");
        let current_request = Request::builder()
            .uri(format!(
                "https://mdpreview.localhost/manifest?revision={:016x}",
                document.hash
            ))
            .body(Vec::new())
            .expect("current manifest request");
        let blocks_request = Request::builder()
            .uri(format!(
                "https://mdpreview.localhost/blocks?revision={:016x}",
                document.hash
            ))
            .body(Vec::new())
            .expect("current blocks request");

        let stale = super::preview_document_response(stale_request, &payload);
        let current = super::preview_document_response(current_request, &payload);
        let blocks = super::preview_document_response(blocks_request, &payload);

        assert_eq!(stale.status(), 409);
        assert_eq!(current.status(), 200);
        assert_eq!(
            current.headers().get("content-type").unwrap(),
            "application/json; charset=utf-8"
        );
        assert_eq!(blocks.status(), 404);
    }

    #[test]
    fn preview_protocol_returns_only_requested_blocks_and_all_source_anchors() {
        let parsed = crate::markdown::parse_document("# 一\n\n甲\n\n# 二\n\n乙\n\n# 三\n\n丙");
        let document = std::sync::Arc::new(super::preview_document(&parsed, "", None, None, None));
        let payload = std::sync::Arc::new(std::sync::Mutex::new(Some(document.clone())));
        let request = Request::builder()
            .uri(format!(
                "https://mdpreview.localhost/blocks?revision={:016x}&start=1&count=1",
                document.hash
            ))
            .body(Vec::new())
            .expect("sliced blocks request");

        let response = super::preview_document_response(request, &payload);
        assert_eq!(response.status(), 200);
        let body: serde_json::Value = serde_json::from_slice(response.body()).expect("blocks JSON");
        assert_eq!(body["blocks"].as_array().unwrap().len(), 1);
        assert_eq!(
            body["sourceAnchors"].as_array().unwrap().len(),
            document.blocks.len() + 1
        );

        let requested_id = parsed.block_index()[2].id;
        let request = Request::builder()
            .uri(format!(
                "https://mdpreview.localhost/blocks?revision={:016x}&ids={requested_id}",
                document.hash
            ))
            .body(Vec::new())
            .expect("ID blocks request");
        let response = super::preview_document_response(request, &payload);
        assert_eq!(response.status(), 200);
        let body: serde_json::Value = serde_json::from_slice(response.body()).expect("blocks JSON");
        assert_eq!(body["blocks"][0]["id"].as_u64(), Some(requested_id));
    }

    #[test]
    fn preview_search_loads_virtual_chunk_and_uses_browser_find() {
        assert!(SCROLL_SYNC_SCRIPT.contains("window.__mdEditorFindText"));
        assert!(SCROLL_SYNC_SCRIPT.contains("loadSource(sourcePosition)"));
        assert!(SCROLL_SYNC_SCRIPT.contains("placeSearchAnchor(sourcePosition, backwards)"));
        assert!(SCROLL_SYNC_SCRIPT.contains("requestRevision !== searchRevision"));
        assert!(SCROLL_SYNC_SCRIPT.contains("window.find(needle"));
    }

    #[test]
    fn concurrent_virtual_chunk_load_waits_for_the_inflight_request() {
        assert!(
            VIRTUAL_PREVIEW_SCRIPT.contains("if (chunk.loading) return chunk.loading;"),
            "a directory jump must await a chunk already being loaded by viewport maintenance"
        );
        assert!(VIRTUAL_PREVIEW_SCRIPT.contains("chunk.loading = (async () =>"));
        assert!(!VIRTUAL_PREVIEW_SCRIPT.contains("chunk.loading || chunk.start"));
    }

    #[test]
    fn heading_navigation_cancels_source_sync_animation_before_scrolling() {
        assert!(
            SCROLL_SYNC_SCRIPT.contains("window.__mdEditorScrollHeading = async (index) =>"),
            "directory navigation needs one scroll owner that can stop source-sync animation"
        );
        let heading_scroll = SCROLL_SYNC_SCRIPT
            .split("window.__mdEditorScrollHeading = async (index) =>")
            .nth(1)
            .expect("heading scroll implementation")
            .split("};")
            .next()
            .expect("heading scroll body");
        let cancel = heading_scroll
            .find("cancelSourceAnimation();")
            .expect("heading navigation must cancel the previous source animation");
        let target = heading_scroll
            .find("scrollHeading(index)")
            .or_else(|| heading_scroll.find("scrollIntoView"))
            .expect("heading navigation must still target the requested heading");
        assert!(
            cancel < target,
            "cancellation must happen before heading scroll"
        );
    }

    #[test]
    fn heading_navigation_invalidates_deferred_initial_scroll_restoration() {
        assert!(SCROLL_SYNC_SCRIPT.contains("let navigationRevision = 0;"));
        assert!(SCROLL_SYNC_SCRIPT.contains("const restoreRevision = navigationRevision;"));
        assert!(SCROLL_SYNC_SCRIPT.contains("navigationRevision += 1;"));
        assert!(SCROLL_SYNC_SCRIPT.contains("if (restoreRevision !== navigationRevision) return;"));
    }

    #[test]
    fn heading_layout_does_not_use_approximate_offscreen_block_sizes() {
        assert!(
            !super::MARKDOWN_DOM_COMPATIBILITY.contains("content-visibility: auto"),
            "approximate offscreen block sizes move a distant heading after scrollIntoView"
        );
        assert!(!super::MARKDOWN_DOM_COMPATIBILITY.contains("contain-intrinsic-block-size"));
    }

    #[test]
    fn browser_scroll_messages_distinguish_user_and_programmatic_updates() {
        assert_eq!(
            parse_source_message("md-source:user:625.5"),
            Some((625.5, true))
        );
        assert_eq!(
            parse_source_message("md-source:program:14.25"),
            Some((14.25, false))
        );
        assert_eq!(parse_source_message("other:user:0.5"), None);
        assert_eq!(
            parse_anchor_message("md-anchor:user:42:0.375:625.5"),
            Some((
                super::ScrollAnchor {
                    block_id: 42,
                    offset: 0.375,
                    source_position: 625.5,
                },
                true,
            ))
        );
        assert!(SCROLL_SYNC_SCRIPT.contains("__mdEditorSetSourcePosition"));
        assert!(SCROLL_SYNC_SCRIPT.contains("__mdEditorSetFontSize"));
        assert!(SCROLL_SYNC_SCRIPT.contains("--md-body-font-size"));
        assert!(SCROLL_SYNC_SCRIPT.contains("--md-font-scale"));
        assert!(SCROLL_SYNC_SCRIPT.contains("sourceForY"));
        assert!(SCROLL_SYNC_SCRIPT.contains("distance * 0.38"));
        assert!(SCROLL_SYNC_SCRIPT.contains("cancelAnimationFrame"));
        assert!(SCROLL_SYNC_SCRIPT.contains("requestAnimationFrame(report)"));
        assert!(SCROLL_SYNC_SCRIPT.contains("__mdEditorReplaceBlock"));
        assert!(
            SCROLL_SYNC_SCRIPT.contains("const replaceBlock = (id, html, sourceStart, sourceEnd)")
        );
        assert!(SCROLL_SYNC_SCRIPT.contains("__mdEditorSetBlockAnchor"));
        let ready = parse_ready_message("md-ready:8400.5:720:1234").unwrap();
        assert_eq!(ready.content_height, 8400.5);
        assert_eq!(ready.viewport_height, 720.0);
        assert_eq!(ready.element_count, 1234);
        assert!(parse_ready_message("md-ready:broken").is_none());
    }

    #[test]
    fn strong_markup_with_trailing_space_remains_literal() {
        let html = render_document("1. **结构层： **训练一个统一的纹样 LoRA。", "", None, None);
        assert!(html.contains("**结构层： **训练一个统一的纹样 LoRA。"));
        assert!(html.contains("font-weight:500;font-display:block"));
        assert!(html.contains("strong,b{font-weight:800!important;}"));
    }

    #[test]
    fn strong_markup_adjacent_to_chinese_text_remains_literal() {
        let html = render_document(
            "- **识别与生成：**区分植物、动物、几何和复合纹样；",
            "",
            None,
            None,
        );
        assert!(
            html.contains("**识别与生成：**区分植物"),
            "generated HTML: {html}"
        );
    }

    #[test]
    fn relative_chinese_image_path_uses_local_resource_protocol() {
        let base = std::env::temp_dir().join(format!(
            "markdown-editor-local-image-test-{}",
            std::process::id()
        ));
        let image_dir = base.join("纹样讲稿图片");
        std::fs::create_dir_all(&image_dir).unwrap();
        let image_path = image_dir.join("01-莲花纹.jpg");
        let bytes = b"test-jpeg-payload";
        std::fs::write(&image_path, bytes).unwrap();

        let url = local_image_url("纹样讲稿图片/01-莲花纹.jpg", Some(&base)).unwrap();
        assert!(url.starts_with(&custom_protocol_url("mdfile", "")));
        assert!(url.contains("%E7%BA%B9%E6%A0%B7"));

        let html = render_document(
            "![莲花纹](纹样讲稿图片/01-莲花纹.jpg)",
            "",
            Some(&base),
            None,
        );
        assert!(html.contains(&format!("src=\"{url}\"")));

        let request = Request::builder().uri(&url).body(Vec::new()).unwrap();
        let response = local_image_response(request);
        assert_eq!(response.status(), 200);
        assert_eq!(response.headers()["content-type"], "image/jpeg");
        assert_eq!(response.body().as_ref(), bytes);

        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn raw_html_img_relative_path_uses_local_resource_protocol() {
        let base = std::env::temp_dir().join(format!(
            "markdown-editor-html-image-test-{}",
            std::process::id()
        ));
        let image_dir = base.join("无人机动物检测讲解_assets");
        std::fs::create_dir_all(&image_dir).unwrap();
        let image_path = image_dir.join("image7.png");
        std::fs::write(&image_path, b"test-png-payload").unwrap();

        let expected_url =
            local_image_url("./无人机动物检测讲解_assets/image7.png", Some(&base)).unwrap();
        let html = render_document(
            r#"<img src="./无人机动物检测讲解_assets/image7.png" alt="从无人机原始 JPG 中裁出的羊群正样本" width="720">"#,
            "img { width: 100%; }",
            Some(&base),
            None,
        );

        assert!(
            html.contains(&format!(r#"src="{expected_url}""#)),
            "raw HTML image should use the local resource protocol: {html}"
        );
        assert!(html.contains(r#"width="720""#));
        assert!(
            html.contains(r#"style="width:720px;max-width:100%""#),
            "width attribute must outrank the theme's img width rule: {html}"
        );
        assert!(html.contains("从无人机原始 JPG 中裁出的羊群正样本"));
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn ordinary_documents_do_not_load_mermaid_runtime() {
        let html = render_document("# 标题\n\n普通正文", "", None, None);
        assert!(!html.contains("mermaid.min.js"));
        assert!(!html.contains("mermaid-init.js"));
    }

    #[test]
    fn markdown与css原样进入浏览器文档() {
        let html = render_document(
            "# 标题\n\n`代码`",
            "h1 { color: #f00; } code::before { content: '>'; }",
            None,
            None,
        );
        assert!(html.contains("<h1 id=\"md-heading-0\">标题</h1>"));
        assert!(html.contains("<code>代码</code>"));
        assert!(html.contains("code::before { content: '>'; }"));
        assert!(html.contains("pre > code { color: inherit"));
        assert!(html.find("table { width: 100%").unwrap() < html.find("h1 { color").unwrap());
    }

    #[test]
    fn 用户字号覆盖位于主题之后() {
        let html = render_document("正文", "body { font-size: 15px; }", None, Some(18.0));
        let theme = html.find("font-size: 15px").unwrap();
        let override_rule = html.find("font-size: 18.00px !important").unwrap();
        assert!(override_rule > theme);
    }

    #[test]
    fn fixed_theme_font_sizes_scale_with_the_user_body_size() {
        let html = render_document(
            "正文\n\n```text\n代码\n```",
            crate::theme::BUILT_IN_SSPAI_CSS,
            None,
            Some(18.0),
        );

        assert!(html.contains("pre { font-size: 15.60px !important; }"));
    }

    #[test]
    fn 窄分栏使用响应式阅读节奏() {
        let html = render_document("# 标题\n\n正文\n\n- 一\n- 二", "", None, None);
        assert!(html.contains("@media (max-width: 700px)"));
        assert!(html.contains("body > h1:first-child { margin-top: 0; margin-bottom: 10px; }"));
        assert!(html.contains("li { margin-top: .45em; margin-bottom: .45em; }"));
    }

    #[test]
    fn 普通列表使用稳定缩进且不影响脚注() {
        let html = render_document(
            "1. 第一项\n2. 第二项",
            crate::theme::BUILT_IN_SSPAI_CSS,
            None,
            None,
        );
        assert!(html.contains(
            "ol:not(#footnotes), ul {\n    padding-inline-start: clamp(1.5em, 3vw, 2.25em) !important;"
        ));
        assert!(html.contains("font-variant-numeric: tabular-nums"));
    }

    #[test]
    fn 预览使用编辑区内置字体() {
        let html = render_document(
            "# 标题\n\n正文 `代码`",
            "body { font-family: serif; }",
            None,
            None,
        );
        assert!(html.contains("@font-face{font-family:'Markdown Editor Mono'"));
        assert!(html.contains("font-family:'LXGW WenKai Lite'"));
        assert!(html.contains(&custom_protocol_url("mdfont", "lxgw-regular.woff")));
        assert!(html.contains("format('woff')"));
        assert!(html.contains(
            "body,pre,code,blockquote::before,blockquote::after{font-family:'Markdown Editor Mono','LXGW WenKai Lite'"
        ));
        assert!(html.find("font-family: serif").unwrap() < html.find("body,pre,code").unwrap());
    }

    #[test]
    fn 本地协议提供四种_webview_专用字体() {
        for (path, expected) in [
            ("jetbrains-regular.woff", JB_MONO_REGULAR_WOFF),
            ("jetbrains-bold.woff", JB_MONO_BOLD_WOFF),
            ("lxgw-regular.woff", LXGW_WENKAI_REGULAR_WOFF),
            ("lxgw-medium.woff", LXGW_WENKAI_MEDIUM_WOFF),
        ] {
            let request = Request::builder()
                .uri(format!("mdfont://localhost/{path}"))
                .body(Vec::new())
                .unwrap();
            let response = preview_asset_response(request);
            assert_eq!(response.status(), 200);
            assert_eq!(response.headers()["content-type"], "font/woff");
            assert_eq!(response.body().len(), expected.len());
        }

        let web_font_bytes = super::preview_font_asset_sizes().into_iter().sum::<usize>();
        let source_font_bytes = crate::export::jetbrains_mono_regular_bytes().len()
            + crate::export::jetbrains_mono_bold_bytes().len()
            + crate::export::lxgw_wenkai_regular_bytes().len()
            + crate::export::lxgw_wenkai_medium_bytes().len();
        assert!(web_font_bytes < source_font_bytes);
    }

    #[test]
    fn 围栏代码在右上角标注语言() {
        let html = render_document("```rust\nfn main() {}\n```", "", None, None);
        assert!(html.contains("<pre data-language=\"rust\"><code class=\"language-rust\">"));
        assert!(html.contains("content: attr(data-language)"));
        assert!(html.contains("text-transform: uppercase"));
    }

    #[test]
    fn mermaid_代码块加载内置渲染器() {
        let html = render_document(
            "```mermaid\nstateDiagram-v2\n    [*] --> Standby\n```",
            "",
            None,
            None,
        );
        assert!(html.contains("<pre data-language=\"mermaid\"><code class=\"language-mermaid\">"));
        assert!(html.contains(&custom_protocol_url("mdfont", "mermaid-init.js")));
        assert!(!html.contains(&custom_protocol_url("mdfont", "mermaid.min.js")));
        assert!(html.contains(&format!(
            "script-src {}",
            custom_protocol_script_source("mdfont")
        )));
        assert!(!html.contains("script-src 'unsafe-inline'"));
    }

    #[test]
    fn mermaid_启动脚本按需加载运行库并串行渲染() {
        assert!(MERMAID_BOOTSTRAP.contains("document.createElement('script')"));
        assert!(MERMAID_BOOTSTRAP.contains("new URL('mermaid.min.js', bootstrapUrl)"));
        assert!(MERMAID_BOOTSTRAP.contains("let runtimePromise = null"));
        assert!(MERMAID_BOOTSTRAP.contains("let renderQueue = Promise.resolve()"));
        assert!(MERMAID_BOOTSTRAP.contains("if (blocks.length === 0) return"));
    }

    #[test]
    fn 本地协议提供_mermaid_运行库和启动脚本() {
        for path in ["mermaid.min.js", "mermaid-init.js"] {
            let request = Request::builder()
                .uri(format!("mdfont://localhost/{path}"))
                .body(Vec::new())
                .unwrap();
            let response = preview_asset_response(request);
            assert_eq!(response.status(), 200);
            assert_eq!(
                response.headers()["content-type"],
                "text/javascript; charset=utf-8"
            );
            assert!(!response.body().is_empty());
        }
    }

    #[test]
    fn 未声明语言的代码块不显示标签() {
        let html = render_document("```\nplain\n```", "", None, None);
        assert!(html.contains("<pre><code>plain"));
        assert!(!html.contains("<pre data-language="));
    }

    #[test]
    fn 少数派二级标题保留粉色边线() {
        let html = render_document("## 小结", crate::theme::BUILT_IN_SSPAI_CSS, None, None);
        assert!(html.contains("<h2 id=\"md-heading-0\">小结</h2>"));
        assert!(html.contains("border-left: 6px solid #ff7e79"));
    }

    #[test]
    fn 深色覆盖层位于浏览器兼容层之后() {
        let parsed = crate::markdown::parse_document("# 深色\n\n正文");
        let html = document(
            &parsed,
            "body { color: white; }",
            None,
            None,
            Some(":root{color-scheme:dark;}"),
        );
        let raw_theme = html.find("body { color: white; }").unwrap();
        let compatibility = html.find("body,pre,code").unwrap();
        let dark = html.find(":root{color-scheme:dark;}").unwrap();
        assert!(raw_theme < compatibility && compatibility < dark);
    }

    #[test]
    fn 审计markdown生成的主题选择器结构() {
        let markdown = "# 一级\n\n## 二级\n\n> 引用\n\n行内 `代码`。\n\n```rust\nfn main() {}\n```\n\n![图片](image.png)\n\n| 功能 | 状态 |\n| --- | --- |\n| 编辑 | 可用 |\n\n脚注[^1]\n\n[^1]: 脚注内容\n";
        let html = render_document(markdown, crate::theme::BUILT_IN_SSPAI_CSS, None, None);
        assert!(html.contains("<h1 id=\"md-heading-0\">一级</h1>"));
        assert!(html.contains("<h2 id=\"md-heading-1\">二级</h2>"));
        assert!(html.contains("<blockquote>"));
        assert!(html.contains("<pre data-language=\"rust\"><code class=\"language-rust\">"));
        assert!(html.contains("<img src=\"image.png\" alt=\"图片\""));
        assert!(html.contains("<table>"));
        assert!(html.contains("<ol id=\"footnotes\">"));
        assert!(html.contains("<li id=\"1\"><p>脚注内容</p>"));
        assert!(!html.contains("class=\"footnote-definition\""));
    }

    #[test]
    fn 每个章节生成稳定且唯一的定位锚点() {
        let html = render_document("# 相同标题\n\n## 相同标题\n\n### 末章", "", None, None);
        assert!(html.contains("<h1 id=\"md-heading-0\">相同标题</h1>"));
        assert!(html.contains("<h2 id=\"md-heading-1\">相同标题</h2>"));
        assert!(html.contains("<h3 id=\"md-heading-2\">末章</h3>"));
    }

    #[test]
    fn 每个顶层markdown块生成源码行锚点() {
        let html = render_document(
            "# 标题\n\n第一段\n\n- 项目一\n- 项目二\n\n## 结尾",
            "",
            None,
            None,
        );
        for source_line in [0, 2, 4, 7] {
            assert!(
                html.contains(&format!("<!--md-editor-source:{source_line}-->")),
                "missing source line {source_line}: {html}"
            );
        }
    }
}
