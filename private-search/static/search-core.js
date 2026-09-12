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

export function shouldFlushFirstPaint({ elapsedMs, holdMs, isComplete, bufferedCount, pageSize }) {
  return isComplete || elapsedMs >= holdMs || bufferedCount >= pageSize;
}

export function sortBufferedByScore(entries) {
  return entries.slice().sort((a, b) => (b.result.score ?? 0) - (a.result.score ?? 0));
}

export function canLoadNextPage({ batchLoading, polling, hasMoreResults }) {
  return !batchLoading && !polling && hasMoreResults;
}

// Failed engine requests used to retry every second forever. When all engines
// were cooling down, one search page could therefore consume the entire local
// per-minute request budget by itself. Back off exponentially, with longer
// starting delays for overload/cooldown responses.
export function retryDelayMs(status, consecutiveFailures) {
  const base = status === 429 ? 5000 : status === 502 || status === 503 ? 3000 : 1000;
  const exponent = Math.max(0, Math.min(consecutiveFailures - 1, 4));
  return Math.min(30000, base * (2 ** exponent));
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

// Incremental SSE parser accepting arbitrary decoded text chunks.
// Handles CRLF/LF line endings, multiline `data:` fields, `event:`, `id:`,
// comments, boundaries split across chunks. Returns complete frames as
// they become available (no browser globals, pure Node-testable).
//
// Usage:
//   const parser = new SSEParser();
//   const frames = [];
//   parser.on('frame', (frame) => frames.push(frame));
//   parser.feed('event: results\ndata: {');
//   parser.feed('...}\n\n');  // frame emitted here
export class SSEParser {
  constructor() {
    this._buffer = "";
    this._currentFrame = {};
    this._listeners = new Map();
  }

  on(event, callback) {
    if (!this._listeners.has(event)) {
      this._listeners.set(event, []);
    }
    this._listeners.get(event).push(callback);
  }

  _emit(event, data) {
    if (this._listeners.has(event)) {
      for (const callback of this._listeners.get(event)) {
        callback(data);
      }
    }
  }

  feed(text) {
    this._buffer += text;
    this._processBuffer();
  }

  end() {
    // Flush any remaining partial frame by adding a final newline
    if (this._buffer.length > 0) {
      this._buffer += "\n";
      this._processBuffer();
    }
    if (Object.keys(this._currentFrame).length > 0) {
      this._emitFrame();
    }
  }

  _processBuffer() {
    // Process complete lines (ending with \n or \r\n)
    let lineEnd;
    while ((lineEnd = this._buffer.search(/\r?\n/)) !== -1) {
      const line = this._buffer.slice(0, lineEnd);
      this._buffer = this._buffer.slice(lineEnd + (this._buffer[lineEnd] === "\r" ? 2 : 1));

      if (line.length === 0) {
        // Empty line = frame boundary
        if (Object.keys(this._currentFrame).length > 0) {
          this._emitFrame();
        }
      } else if (line.startsWith(":")) {
        // Comment line — ignore
        continue;
      } else if (line.includes(":")) {
        const colonIndex = line.indexOf(":");
        const field = line.slice(0, colonIndex);
        let value = line.slice(colonIndex + 1);
        // Remove leading space if present
        if (value.startsWith(" ")) {
          value = value.slice(1);
        }

        if (field === "data") {
          // Accumulate data lines
          if (!this._currentFrame.data) {
            this._currentFrame.data = value;
          } else {
            this._currentFrame.data += "\n" + value;
          }
        } else if (field === "event") {
          this._currentFrame.event = value;
        } else if (field === "id") {
          this._currentFrame.id = value;
        }
        // Ignore other fields (e.g. retry)
      }
    }
  }

  _emitFrame() {
    const frame = { ...this._currentFrame };
    if (frame.data) {
      try {
        frame.data = JSON.parse(frame.data);
      } catch (e) {
        // Leave as raw string if not valid JSON
      }
    }
    this._emit("frame", frame);
    this._currentFrame = {};
  }
}

// Stream-state reducer managing named event payloads from the server.
// Detects replays via monotonic numeric IDs, deduplicates URL entries and
// engine/attribution updates idempotently, and tracks order tokens + cursors.
export class StreamStateReducer {
  constructor() {
    this.orderId = null;
    this.canonical = false; // boolean flag from meta
    this.cached = false;
    this.canonicalOrderId = null; // ID only from done
    this.activeOrderToken = null;
    this.serverCursor = 0;
    this.renderedCount = 0; // distinct URLs appended
    this.nextCursor = 0;
    this.hasMore = false;
    this.lastEventId = null;
    this.lastNumericEventId = -1; // replay detection
    this.seenUrls = new Set();
    this.attributionByUrl = new Map(); // url -> Set of engine names
    this.engineReports = new Map(); // engine name -> last report
    this.isComplete = false;
    this.error = null;
    this.actions = [];
  }

  // Process frame, detecting replays via numeric IDs.
  processFrame(frame) {
    this.actions = [];

    // Detect replays: numeric ID <= lastNumericEventId = exact replay.
    // Check before updating lastEventId so an older replay cannot move the
    // reconnect cursor backwards.
    if (frame.id && /^\d+$/.test(frame.id)) {
      const numId = parseInt(frame.id, 10);
      if (numId <= this.lastNumericEventId) {
        return; // No actions, no mutations
      }
      this.lastNumericEventId = numId;
    }
    this.lastEventId = frame.id || this.lastEventId;

    if (!frame.event || frame.data === undefined) {
      return;
    }

    const data = frame.data;
    if (frame.event !== "error" && typeof data !== "object") {
      return;
    }

    switch (frame.event) {
      case "meta":
        this._handleMeta(data);
        break;
      case "results":
        this._handleResults(data);
        break;
      case "attribution":
        this._handleAttribution(data);
        break;
      case "engine":
        this._handleEngine(data);
        break;
      case "done":
        this._handleDone(data);
        break;
      case "error":
        this._handleError(data);
        break;
    }
  }

  _handleMeta(data) {
    // { orderId, canonical (bool), start, count (page size) }
    if (data.orderId !== undefined) {
      this.orderId = data.orderId;
      this.activeOrderToken = data.orderId;
    }
    if (data.canonical !== undefined) {
      this.canonical = !!data.canonical;
    }
    if (data.cached !== undefined) {
      this.cached = !!data.cached;
    }
    if (data.start !== undefined) {
      this.serverCursor = data.start;
    }
    // count is page size, not rendered count
  }

  _handleResults(data) {
    // Support single { position, result, cached } or batches { entries: [...] } / { results: [...] }
    const entries = data.entries || data.results || (data.position !== undefined ? [data] : []);

    for (const entry of entries) {
      if (entry.position === undefined || !entry.result) {
        continue;
      }

      const url = entry.result.url || entry.result.href;
      if (!url) {
        continue; // Reject entries with no URL
      }

      const isNew = !this.seenUrls.has(url);
      this.seenUrls.add(url);

      // Always advance serverCursor based on position, even for duplicates
      if (entry.position !== undefined) {
        this.serverCursor = Math.max(this.serverCursor, entry.position + 1);
      }

      if (isNew) {
        this.actions.push({
          type: "append",
          position: entry.position,
          result: entry.result,
          cached: !!(entry.result.cached || entry.cached),
        });
        this.renderedCount++;
      }
    }
  }

  _handleAttribution(data) {
    // Support two formats:
    // 1. Full set: { url, engines: [...] } — definitive set from server
    // 2. Singular: { url, engine, ... } — add to existing set (backward compat)
    if (!data.url) {
      return;
    }

    if (!this.attributionByUrl.has(data.url)) {
      this.attributionByUrl.set(data.url, new Set());
    }

    const currentEngines = this.attributionByUrl.get(data.url);
    let newEnginesSet;

    if (Array.isArray(data.engines)) {
      // Full array format: use as definitive set
      newEnginesSet = new Set(data.engines.filter(engine => typeof engine === "string"));
    } else if (typeof data.engine === "string" && data.engine) {
      // Singular format: add to existing set
      newEnginesSet = new Set(currentEngines);
      newEnginesSet.add(data.engine);
    } else {
      return;
    }

    // Check if the normalized set has changed
    const hasChanged = newEnginesSet.size !== currentEngines.size ||
      Array.from(newEnginesSet).some(e => !currentEngines.has(e));

    if (hasChanged) {
      // Update with full new set
      this.attributionByUrl.set(data.url, newEnginesSet);
      this.actions.push({
        type: "updateAttribution",
        url: data.url,
        engines: Array.from(newEnginesSet),
      });
    }
  }

  _handleEngine(data) {
    // { name (or engine), status, ...report } — no redundant updates
    const name = data.name || data.engine;
    if (!name) {
      return;
    }

    const lastReport = this.engineReports.get(name);
    const reportStr = JSON.stringify(data);
    const lastReportStr = lastReport ? JSON.stringify(lastReport) : null;

    if (reportStr !== lastReportStr) {
      this.engineReports.set(name, { ...data });
      this.actions.push({
        type: "updateEngine",
        name: name,
        report: { ...data },
      });
    }
  }

  _handleDone(data) {
    // { activeOrderId, canonicalOrderId, nextCursor, hasMore }
    if (data.activeOrderId !== undefined) {
      this.activeOrderToken = data.activeOrderId;
    }
    if (data.canonicalOrderId !== undefined) {
      this.canonicalOrderId = data.canonicalOrderId;
    }
    if (data.nextCursor !== undefined) {
      this.nextCursor = data.nextCursor;
      this.serverCursor = data.nextCursor;
    }
    if (data.hasMore !== undefined) {
      this.hasMore = !!data.hasMore;
    }

    this.isComplete = true;
    this.actions.push({
      type: "done",
      activeOrderId: data.activeOrderId,
      canonicalOrderId: data.canonicalOrderId,
      nextCursor: data.nextCursor,
      hasMore: this.hasMore,
    });
  }

  _handleError(data) {
    if (typeof data === "string") {
      this.error = data;
    } else if (typeof data === "object" && data !== null) {
      this.error = data.message || "Unknown error";
    } else {
      this.error = "Unknown error";
    }
    this.isComplete = true;
    this.actions.push({
      type: "error",
      message: this.error,
    });
  }

  snapshot() {
    return {
      orderId: this.orderId,
      canonical: this.canonical,
      cached: this.cached,
      canonicalOrderId: this.canonicalOrderId,
      activeOrderToken: this.activeOrderToken,
      serverCursor: this.serverCursor,
      renderedCount: this.renderedCount,
      nextCursor: this.nextCursor,
      hasMore: this.hasMore,
      lastEventId: this.lastEventId,
      isComplete: this.isComplete,
      error: this.error,
      seenUrlsCount: this.seenUrls.size,
    };
  }
}
