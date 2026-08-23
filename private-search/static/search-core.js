// Pure, DOM-free logic pulled out of search.js so it can be unit tested with
// plain `node:test` — no browser/jsdom needed.

export function escapeHtml(str) {
  return String(str)
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;");
}

// Result urls come from scraped, untrusted third-party HTML. Only allow
// http(s) links (blocks `javascript:`/`data:` etc.) and escape the rest so
// it's safe to drop into an href/src attribute. `base` is required (rather
// than defaulting to the browser's `location`) so this stays callable from
// Node tests with no DOM.
export function safeUrl(url, base) {
  try {
    const parsed = new URL(String(url), base);
    if (parsed.protocol === "http:" || parsed.protocol === "https:") {
      return escapeHtml(parsed.href);
    }
  } catch (e) {
    // not a valid URL — fall through to blocking it
  }
  return "#";
}

export function unwrapPayload(obj) {
  const empty = { results: [], engines: [], hasMore: false };
  if (!obj || typeof obj !== "object") return empty;

  const payload = obj.General || obj.Images;
  if (!payload) {
    console.warn("Unknown response variant:", obj);
    return empty;
  }

  return {
    results: payload.results || [],
    engines: payload.engines || [],
    hasMore: !!payload.hasMore,
  };
}

// `search` is a `location.search`-shaped string (e.g. "?q=rust&t=general"),
// passed explicitly rather than read from `location` so this is callable
// from Node tests with no DOM.
export function getQueryParam(search, name) {
  return new URLSearchParams(search).get(name) || "";
}

// Keep at most one page of unfilled placeholders in the document. A partial
// response can leave some skeletons queued; the next request should top that
// page back up instead of appending another complete page beneath it.
export function skeletonsNeeded(queueLength, pageSize) {
  return Math.max(0, pageSize - queueLength);
}

export function canLoadNextPage({ batchLoading, polling, hasMoreResults }) {
  return !batchLoading && !polling && hasMoreResults;
}

// True once the end-of-results marker has come within `margin` px of the
// bottom of the viewport, so the next page starts loading slightly before the
// user actually reaches the end of the list. `sentinelTop` is the marker's
// viewport-relative top (i.e. `getBoundingClientRect().top`).
export function isWithinPreloadRange(sentinelTop, viewportHeight, margin) {
  return sentinelTop <= viewportHeight + margin;
}

// Whether to keep loading immediately after a batch lands, rather than waiting
// for the user to scroll again.
//
// This is what makes "scroll for more" work at all in the common case: a user
// who is already parked at the bottom of the page generates no further scroll
// events (there is nowhere left to scroll), and an IntersectionObserver whose
// target is *already* intersecting reports nothing new either. Without an
// explicit re-check after each batch, the end of page 1 was simply the end of
// the results.
//
// `lastBatchSize === 0` stops the re-check from spinning on a page that keeps
// coming back empty while the server still reports `hasMore` — that case waits
// for a real scroll instead.
export function shouldAutoContinue({
  lastBatchSize,
  inPreloadRange,
  batchLoading,
  polling,
  hasMoreResults,
}) {
  return (
    lastBatchSize > 0 &&
    inPreloadRange &&
    canLoadNextPage({ batchLoading, polling, hasMoreResults })
  );
}

// A small FIFO of not-yet-filled placeholder elements. Replaces matching
// results to skeletons by a computed numeric id (which could drift out of
// sync whenever a poll returns a different number of results than were
// pre-allocated skeletons for, e.g. two engines each contributing up to
// `count` distinct results merges into more than `count` total) — instead,
// whichever skeleton is next in line gets filled next, full stop.
export class SkeletonQueue {
  constructor() {
    this._items = [];
  }

  push(item) {
    this._items.push(item);
  }

  get length() {
    return this._items.length;
  }

  // Returns the next unfilled item, or the result of `createFn()` if the
  // queue is currently empty (never returns the same item twice).
  next(createFn) {
    if (this._items.length > 0) {
      return this._items.shift();
    }
    return createFn();
  }

  // Empties the queue and returns whatever was left in it — used once
  // polling ends (no more results coming) to find any pre-allocated
  // skeletons that will now never be filled, so the caller can remove them
  // instead of leaving permanent loading placeholders on the page.
  drain() {
    const leftover = this._items;
    this._items = [];
    return leftover;
  }
}
