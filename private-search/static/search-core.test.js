import { test } from "node:test";
import assert from "node:assert/strict";
import {
  escapeHtml,
  safeUrl,
  unwrapPayload,
  getQueryParam,
  skeletonsNeeded,
  canLoadNextPage,
  isWithinPreloadRange,
  shouldAutoContinue,
  retryDelayMs,
  SkeletonQueue,
  SSEParser,
  StreamStateReducer,
} from "./search-core.js";

const BASE = "https://example.com/search";

test("escapeHtml escapes the five HTML-significant characters", () => {
  assert.equal(
    escapeHtml(`<script>alert("hi")</script> & 'ok'`),
    "&lt;script&gt;alert(&quot;hi&quot;)&lt;/script&gt; &amp; 'ok'"
  );
});

test("escapeHtml passes non-ASCII text through unchanged", () => {
  assert.equal(escapeHtml("café ☕ 日本語 مرحبا"), "café ☕ 日本語 مرحبا");
});

test("escapeHtml coerces non-string input", () => {
  assert.equal(escapeHtml(42), "42");
  assert.equal(escapeHtml(null), "null");
});

test("safeUrl allows http(s) urls and escapes them for attribute use", () => {
  assert.equal(safeUrl("https://example.com/a?b=1&c=2", BASE), "https://example.com/a?b=1&amp;c=2");
  assert.equal(safeUrl("http://example.com", BASE), "http://example.com/");
});

test("safeUrl blocks javascript:, data:, and other non-http(s) schemes", () => {
  assert.equal(safeUrl("javascript:alert(1)", BASE), "#");
  assert.equal(safeUrl("data:text/html,<script>alert(1)</script>", BASE), "#");
  assert.equal(safeUrl("ftp://example.com/file", BASE), "#");
});

test("safeUrl blocks input that fails to parse as a URL at all", () => {
  assert.equal(safeUrl("http://example.com:not-a-port/", BASE), "#");
});

test("safeUrl resolves a bare path/relative link against the base, like a browser would", () => {
  assert.equal(safeUrl("not a url", BASE), "https://example.com/not%20a%20url");
});

test("safeUrl preserves non-ASCII paths", () => {
  assert.equal(safeUrl("https://example.com/日本語", BASE), "https://example.com/%E6%97%A5%E6%9C%AC%E8%AA%9E");
});

test("unwrapPayload extracts the General variant", () => {
  const result = unwrapPayload({
    General: { results: [{ url: "https://a.com" }], engines: [{ engine: "Brave" }], hasMore: true },
  });
  assert.deepEqual(result, {
    results: [{ url: "https://a.com" }],
    engines: [{ engine: "Brave" }],
    hasMore: true,
  });
});

test("unwrapPayload extracts the Images variant", () => {
  const result = unwrapPayload({ Images: { results: [], engines: [], hasMore: false } });
  assert.deepEqual(result, { results: [], engines: [], hasMore: false });
});

test("unwrapPayload defaults missing fields safely", () => {
  const result = unwrapPayload({ General: {} });
  assert.deepEqual(result, { results: [], engines: [], hasMore: false });
});

test("unwrapPayload returns an empty default for malformed input", () => {
  const empty = { results: [], engines: [], hasMore: false };
  assert.deepEqual(unwrapPayload(null), empty);
  assert.deepEqual(unwrapPayload(undefined), empty);
  assert.deepEqual(unwrapPayload("not an object"), empty);
  assert.deepEqual(unwrapPayload({ SomethingElse: {} }), empty);
});

test("getQueryParam reads a param from a location.search-shaped string", () => {
  assert.equal(getQueryParam("?q=rust+async&t=general", "q"), "rust async");
  assert.equal(getQueryParam("?q=rust+async&t=general", "t"), "general");
});

test("getQueryParam defaults to an empty string when absent", () => {
  assert.equal(getQueryParam("?t=general", "q"), "");
  assert.equal(getQueryParam("", "q"), "");
});

test("retryDelayMs exponentially backs off ordinary failures", () => {
  assert.equal(retryDelayMs(0, 1), 1000);
  assert.equal(retryDelayMs(500, 2), 2000);
  assert.equal(retryDelayMs(500, 6), 16000);
});

test("retryDelayMs gives cooldown and rate-limit responses more time", () => {
  assert.equal(retryDelayMs(503, 1), 3000);
  assert.equal(retryDelayMs(503, 5), 30000);
  assert.equal(retryDelayMs(429, 1), 5000);
  assert.equal(retryDelayMs(429, 5), 30000);
});

test("skeletonsNeeded caps outstanding placeholders at one page", () => {
  assert.equal(skeletonsNeeded(0, 10), 10);
  assert.equal(skeletonsNeeded(4, 10), 6);
  assert.equal(skeletonsNeeded(10, 10), 0);
  assert.equal(skeletonsNeeded(14, 10), 0);
});

test("canLoadNextPage blocks scroll loads while any request is active", () => {
  assert.equal(
    canLoadNextPage({ batchLoading: false, polling: true, hasMoreResults: true }),
    false,
  );
  assert.equal(
    canLoadNextPage({ batchLoading: true, polling: false, hasMoreResults: true }),
    false,
  );
  assert.equal(
    canLoadNextPage({ batchLoading: false, polling: false, hasMoreResults: false }),
    false,
  );
  assert.equal(
    canLoadNextPage({ batchLoading: false, polling: false, hasMoreResults: true }),
    true,
  );
});

test("isWithinPreloadRange triggers before the marker actually reaches the viewport", () => {
  // Viewport is 800px tall, preload margin 500px: anything whose top is at or
  // above 1300px counts as "the end of the list is coming up".
  assert.equal(isWithinPreloadRange(1400, 800, 500), false);
  assert.equal(isWithinPreloadRange(1300, 800, 500), true);
  assert.equal(isWithinPreloadRange(200, 800, 500), true);
});

test("isWithinPreloadRange counts a marker scrolled above the viewport", () => {
  // Negative top = the end of the list is already behind the user; that must
  // still count, or a page loaded while parked at the bottom never continues.
  assert.equal(isWithinPreloadRange(-50, 800, 500), true);
});

test("shouldAutoContinue keeps loading while the end of the list stays in view", () => {
  assert.equal(
    shouldAutoContinue({
      lastBatchSize: 10,
      inPreloadRange: true,
      batchLoading: false,
      polling: false,
      hasMoreResults: true,
    }),
    true,
  );
});

test("shouldAutoContinue stops once the results push the end marker out of range", () => {
  assert.equal(
    shouldAutoContinue({
      lastBatchSize: 10,
      inPreloadRange: false,
      batchLoading: false,
      polling: false,
      hasMoreResults: true,
    }),
    false,
  );
});

test("shouldAutoContinue refuses to spin on a batch that came back empty", () => {
  // `hasMore` can stay true while a page yields nothing (e.g. an engine is
  // failing); re-requesting it on a timer would just hammer the server.
  assert.equal(
    shouldAutoContinue({
      lastBatchSize: 0,
      inPreloadRange: true,
      batchLoading: false,
      polling: false,
      hasMoreResults: true,
    }),
    false,
  );
});

test("shouldAutoContinue defers to canLoadNextPage's guards", () => {
  const base = { lastBatchSize: 10, inPreloadRange: true };
  assert.equal(
    shouldAutoContinue({ ...base, batchLoading: true, polling: false, hasMoreResults: true }),
    false,
  );
  assert.equal(
    shouldAutoContinue({ ...base, batchLoading: false, polling: true, hasMoreResults: true }),
    false,
  );
  assert.equal(
    shouldAutoContinue({ ...base, batchLoading: false, polling: false, hasMoreResults: false }),
    false,
  );
});

test("SkeletonQueue serves pushed items before falling back to creating new ones", () => {
  const queue = new SkeletonQueue();
  queue.push("a");
  queue.push("b");

  assert.equal(queue.length, 2);
  assert.equal(queue.next(() => "fallback"), "a");
  assert.equal(queue.next(() => "fallback"), "b");
  assert.equal(queue.length, 0);
  assert.equal(queue.next(() => "fallback"), "fallback");
});

test("SkeletonQueue.drain empties the queue and returns what was left", () => {
  const queue = new SkeletonQueue();
  queue.push("a");
  queue.push("b");
  queue.push("c");

  queue.next(() => "fallback"); // consume "a", leaving b/c

  const leftover = queue.drain();
  assert.deepEqual(leftover, ["b", "c"]);
  assert.equal(queue.length, 0);
  assert.deepEqual(queue.drain(), []);
});

test("SkeletonQueue never returns the same item twice", () => {
  const queue = new SkeletonQueue();
  queue.push("only");

  const seen = [queue.next(() => "new"), queue.next(() => "new"), queue.next(() => "new")];
  assert.deepEqual(seen, ["only", "new", "new"]);
});

test("SSEParser parses a complete single-line event", () => {
  const parser = new SSEParser();
  const frames = [];
  parser.on("frame", (frame) => frames.push(frame));

  parser.feed('event: results\ndata: {"id":1}\n\n');
  assert.equal(frames.length, 1);
  assert.equal(frames[0].event, "results");
  assert.deepEqual(frames[0].data, { id: 1 });
});

test("SSEParser handles CRLF line endings", () => {
  const parser = new SSEParser();
  const frames = [];
  parser.on("frame", (frame) => frames.push(frame));

  parser.feed('event: test\r\ndata: {"x":1}\r\n\r\n');
  assert.equal(frames.length, 1);
  assert.equal(frames[0].event, "test");
  assert.deepEqual(frames[0].data, { x: 1 });
});

test("SSEParser handles mixed LF and CRLF", () => {
  const parser = new SSEParser();
  const frames = [];
  parser.on("frame", (frame) => frames.push(frame));

  parser.feed('event: test\r\ndata: 123\nid: abc\r\n\r\n');
  assert.equal(frames.length, 1);
  assert.equal(frames[0].event, "test");
  assert.equal(frames[0].id, "abc");
});

test("SSEParser handles multiline data fields", () => {
  const parser = new SSEParser();
  const frames = [];
  parser.on("frame", (frame) => frames.push(frame));

  parser.feed('event: test\ndata: line1\ndata: line2\ndata: line3\n\n');
  assert.equal(frames.length, 1);
  assert.equal(frames[0].event, "test");
  assert.equal(frames[0].data, "line1\nline2\nline3");
});

test("SSEParser handles multiline JSON data", () => {
  const parser = new SSEParser();
  const frames = [];
  parser.on("frame", (frame) => frames.push(frame));

  parser.feed('event: results\ndata: {\ndata:   "url": "test.com",\ndata:   "title": "Test"\ndata: }\n\n');
  assert.equal(frames.length, 1);
  assert.deepEqual(frames[0].data, { url: "test.com", title: "Test" });
});

test("SSEParser ignores comment lines", () => {
  const parser = new SSEParser();
  const frames = [];
  parser.on("frame", (frame) => frames.push(frame));

  parser.feed(': heartbeat\nevent: test\ndata: 1\n: another comment\n\n');
  assert.equal(frames.length, 1);
  assert.equal(frames[0].event, "test");
});

test("SSEParser handles field values with spaces after colon", () => {
  const parser = new SSEParser();
  const frames = [];
  parser.on("frame", (frame) => frames.push(frame));

  parser.feed('event: results\ndata:  {"x": 1}\n\n');
  assert.equal(frames.length, 1);
  assert.deepEqual(frames[0].data, { x: 1 });
});

test("SSEParser handles field values without spaces after colon", () => {
  const parser = new SSEParser();
  const frames = [];
  parser.on("frame", (frame) => frames.push(frame));

  parser.feed('event:results\ndata:{"x":1}\n\n');
  assert.equal(frames.length, 1);
  assert.equal(frames[0].event, "results");
});

test("SSEParser handles chunk boundaries within a field name", () => {
  const parser = new SSEParser();
  const frames = [];
  parser.on("frame", (frame) => frames.push(frame));

  parser.feed('ev');
  parser.feed('ent: test\ndata: 1\n\n');
  assert.equal(frames.length, 1);
  assert.equal(frames[0].event, "test");
});

test("SSEParser handles chunk boundaries within multiline data", () => {
  const parser = new SSEParser();
  const frames = [];
  parser.on("frame", (frame) => frames.push(frame));

  parser.feed('event: test\ndata: line1\nda');
  parser.feed('ta: line2\n\n');
  assert.equal(frames.length, 1);
  assert.equal(frames[0].data, "line1\nline2");
});

test("SSEParser handles chunk boundaries within JSON data", () => {
  const parser = new SSEParser();
  const frames = [];
  parser.on("frame", (frame) => frames.push(frame));

  parser.feed('event: results\ndata: {"ur');
  parser.feed('l": "test.com"}\n\n');
  assert.equal(frames.length, 1);
  assert.deepEqual(frames[0].data, { url: "test.com" });
});

test("SSEParser handles chunk boundaries at frame boundaries", () => {
  const parser = new SSEParser();
  const frames = [];
  parser.on("frame", (frame) => frames.push(frame));

  parser.feed('event: frame1\ndata: 1\n');
  parser.feed('\nevent: frame2\ndata: 2\n\n');
  assert.equal(frames.length, 2);
  assert.equal(frames[0].event, "frame1");
  assert.equal(frames[1].event, "frame2");
});

test("SSEParser handles multiple complete frames in one feed", () => {
  const parser = new SSEParser();
  const frames = [];
  parser.on("frame", (frame) => frames.push(frame));

  parser.feed('event: a\ndata: 1\n\nevent: b\ndata: 2\n\nevent: c\ndata: 3\n\n');
  assert.equal(frames.length, 3);
  assert.equal(frames[0].event, "a");
  assert.equal(frames[1].event, "b");
  assert.equal(frames[2].event, "c");
});

test("SSEParser handles end() to flush incomplete frame", () => {
  const parser = new SSEParser();
  const frames = [];
  parser.on("frame", (frame) => frames.push(frame));

  parser.feed('event: test\ndata: value');
  assert.equal(frames.length, 0);

  parser.end();
  assert.equal(frames.length, 1);
  assert.equal(frames[0].event, "test");
  assert.equal(frames[0].data, "value");
});

test("SSEParser handles malformed JSON by keeping it as a string", () => {
  const parser = new SSEParser();
  const frames = [];
  parser.on("frame", (frame) => frames.push(frame));

  parser.feed('event: test\ndata: {not valid json}\n\n');
  assert.equal(frames.length, 1);
  assert.equal(typeof frames[0].data, "string");
  assert.equal(frames[0].data, "{not valid json}");
});

test("SSEParser collects id field", () => {
  const parser = new SSEParser();
  const frames = [];
  parser.on("frame", (frame) => frames.push(frame));

  parser.feed('id: 42\nevent: test\ndata: value\n\n');
  assert.equal(frames[0].id, "42");
});

test("StreamStateReducer initializes with empty state", () => {
  const reducer = new StreamStateReducer();
  const snap = reducer.snapshot();

  assert.equal(snap.orderId, null);
  assert.equal(snap.canonical, false);
  assert.equal(snap.canonicalOrderId, null);
  assert.equal(snap.activeOrderToken, null);
  assert.equal(snap.serverCursor, 0);
  assert.equal(snap.renderedCount, 0);
  assert.equal(snap.nextCursor, 0);
  assert.equal(snap.hasMore, false);
  assert.equal(snap.isComplete, false);
  assert.equal(snap.error, null);
  assert.equal(snap.seenUrlsCount, 0);
});

test("StreamStateReducer processes meta frame", () => {
  const reducer = new StreamStateReducer();
  reducer.processFrame({
    event: "meta",
    id: "1",
    data: { orderId: "order123", canonical: true, start: 0, count: 20 },
  });

  const snap = reducer.snapshot();
  assert.equal(snap.orderId, "order123");
  assert.equal(snap.canonical, true);
  assert.equal(snap.canonicalOrderId, null); // Not set by meta
  assert.equal(snap.serverCursor, 0);
  assert.equal(snap.renderedCount, 0); // count is page size, not rendered count
  assert.equal(snap.lastEventId, "1");
});

test("StreamStateReducer processes result frame and tracks seen URLs", () => {
  const reducer = new StreamStateReducer();
  reducer.processFrame({
    event: "results",
    id: "2",
    data: { position: 0, result: { url: "https://a.com", title: "A" } },
  });

  const snap = reducer.snapshot();
  assert.equal(snap.renderedCount, 1);
  assert.equal(snap.seenUrlsCount, 1);
  assert.equal(reducer.actions.length, 1);
  assert.equal(reducer.actions[0].type, "append");
  assert.deepEqual(reducer.actions[0].result, { url: "https://a.com", title: "A" });
});

test("StreamStateReducer ignores duplicate result URLs and advances cursor", () => {
  const reducer = new StreamStateReducer();
  reducer.processFrame({
    event: "results",
    data: { position: 0, result: { url: "https://a.com", title: "A" } },
  });

  assert.equal(reducer.renderedCount, 1);
  assert.equal(reducer.serverCursor, 1);

  reducer.processFrame({
    event: "results",
    data: { position: 1, result: { url: "https://a.com", title: "Different" } },
  });

  assert.equal(reducer.renderedCount, 1); // No increment
  assert.equal(reducer.serverCursor, 2); // Cursor still advances
  assert.equal(reducer.actions.length, 0); // No action for duplicate
});

test("StreamStateReducer processes attribution frame", () => {
  const reducer = new StreamStateReducer();
  reducer.processFrame({
    event: "attribution",
    data: { url: "https://a.com", engine: "DuckDuckGo", attribution: "from cache" },
  });

  assert.equal(reducer.actions.length, 1);
  assert.equal(reducer.actions[0].type, "updateAttribution");
  assert.deepEqual(reducer.actions[0].engines, ["DuckDuckGo"]);
});

test("StreamStateReducer processes engine frame with name field", () => {
  const reducer = new StreamStateReducer();
  reducer.processFrame({
    event: "engine",
    data: { name: "Google", status: "success", resultCount: 10 },
  });

  assert.equal(reducer.actions.length, 1);
  assert.equal(reducer.actions[0].type, "updateEngine");
  assert.equal(reducer.actions[0].name, "Google");
  assert.equal(reducer.engineReports.get("Google").status, "success");
});

test("StreamStateReducer processes engine frame with engine field", () => {
  const reducer = new StreamStateReducer();
  reducer.processFrame({
    event: "engine",
    data: { engine: "DuckDuckGo", status: "timeout" },
  });

  assert.equal(reducer.actions.length, 1);
  assert.equal(reducer.actions[0].name, "DuckDuckGo");
});

test("StreamStateReducer processes done frame", () => {
  const reducer = new StreamStateReducer();
  reducer.processFrame({
    event: "done",
    data: {
      activeOrderId: "order123",
      canonicalOrderId: "can456",
      nextCursor: 50,
      hasMore: false,
    },
  });

  const snap = reducer.snapshot();
  assert.equal(snap.isComplete, true);
  assert.equal(snap.activeOrderToken, "order123");
  assert.equal(snap.canonicalOrderId, "can456");
  assert.equal(snap.nextCursor, 50);
  assert.equal(snap.hasMore, false);
  assert.equal(reducer.actions.length, 1);
  assert.equal(reducer.actions[0].type, "done");
});

test("StreamStateReducer processes error frame", () => {
  const reducer = new StreamStateReducer();
  reducer.processFrame({
    event: "error",
    data: { message: "Query too broad" },
  });

  const snap = reducer.snapshot();
  assert.equal(snap.isComplete, true);
  assert.equal(snap.error, "Query too broad");
  assert.equal(reducer.actions.length, 1);
  assert.equal(reducer.actions[0].type, "error");
});

test("StreamStateReducer handles error as a string", () => {
  const reducer = new StreamStateReducer();
  reducer.processFrame({
    event: "error",
    data: "Something went wrong",
  });

  const snap = reducer.snapshot();
  assert.equal(snap.error, "Something went wrong");
});

test("StreamStateReducer tracks lastEventId across frames", () => {
  const reducer = new StreamStateReducer();

  reducer.processFrame({ event: "meta", id: "1", data: {} });
  assert.equal(reducer.lastEventId, "1");

  reducer.processFrame({ event: "results", id: "2", data: { position: 0, result: { url: "x" } } });
  assert.equal(reducer.lastEventId, "2");

  reducer.processFrame({ event: "engine", data: { name: "G" } });
  assert.equal(reducer.lastEventId, "2"); // Unchanged if no id
});

test("StreamStateReducer handles complex stream sequence", () => {
  const reducer = new StreamStateReducer();

  reducer.processFrame({
    event: "meta",
    id: "1",
    data: { orderId: "order1", canonical: false, start: 0, count: 20 },
  });

  reducer.processFrame({
    event: "results",
    id: "2",
    data: { position: 0, result: { url: "https://a.com", title: "A" } },
  });

  reducer.processFrame({
    event: "results",
    id: "3",
    data: { position: 1, result: { url: "https://b.com", title: "B" } },
  });

  reducer.processFrame({
    event: "engine",
    id: "4",
    data: { name: "DuckDuckGo", status: "success" },
  });

  reducer.processFrame({
    event: "done",
    id: "5",
    data: {
      activeOrderId: "order1",
      canonicalOrderId: "can1",
      nextCursor: 20,
      hasMore: false,
    },
  });

  const snap = reducer.snapshot();
  assert.equal(snap.renderedCount, 2);
  assert.equal(snap.seenUrlsCount, 2);
  assert.equal(snap.canonical, false);
  assert.equal(snap.canonicalOrderId, "can1"); // Set by done
  assert.equal(snap.isComplete, true);
  assert.equal(snap.lastEventId, "5");
});

test("StreamStateReducer rejects numeric replay by event ID", () => {
  const reducer = new StreamStateReducer();

  reducer.processFrame({
    event: "results",
    id: "1",
    data: { position: 0, result: { url: "https://a.com", title: "A" } },
  });
  assert.equal(reducer.renderedCount, 1);

  reducer.processFrame({
    event: "results",
    id: "1", // Same numeric ID — exact replay
    data: { position: 0, result: { url: "https://b.com", title: "B" } },
  });
  assert.equal(reducer.renderedCount, 1); // No change
  assert.equal(reducer.actions.length, 0); // No action

  reducer.processFrame({
    event: "results",
    id: "0", // Lower numeric ID — also a replay
    data: { position: 0, result: { url: "https://c.com", title: "C" } },
  });
  assert.equal(reducer.renderedCount, 1); // Still no change
  assert.equal(reducer.lastEventId, "1"); // Reconnect cursor never regresses
});

test("StreamStateReducer allows non-numeric IDs (does not treat as replay)", () => {
  const reducer = new StreamStateReducer();

  reducer.processFrame({
    event: "results",
    id: "abc",
    data: { position: 0, result: { url: "https://a.com", title: "A" } },
  });
  assert.equal(reducer.renderedCount, 1);

  reducer.processFrame({
    event: "results",
    id: "abc", // Same non-numeric ID — not treated as replay
    data: { position: 0, result: { url: "https://b.com", title: "B" } },
  });
  assert.equal(reducer.renderedCount, 2); // New URL processed
});

test("StreamStateReducer rejects result entries with no URL", () => {
  const reducer = new StreamStateReducer();

  reducer.processFrame({
    event: "results",
    data: { position: 0, result: { title: "No URL" } }, // No url or href
  });

  assert.equal(reducer.renderedCount, 0);
  assert.equal(reducer.actions.length, 0);
  assert.equal(reducer.seenUrls.has(undefined), false);
});

test("StreamStateReducer supports batched results with entries array", () => {
  const reducer = new StreamStateReducer();

  reducer.processFrame({
    event: "results",
    data: {
      entries: [
        { position: 0, result: { url: "https://a.com", title: "A" } },
        { position: 1, result: { url: "https://b.com", title: "B" } },
      ],
    },
  });

  assert.equal(reducer.renderedCount, 2);
  assert.equal(reducer.actions.length, 2);
  assert.equal(reducer.serverCursor, 2);
});

test("StreamStateReducer supports batched results with results array", () => {
  const reducer = new StreamStateReducer();

  reducer.processFrame({
    event: "results",
    data: {
      results: [{ position: 0, result: { url: "https://x.com" } }],
    },
  });

  assert.equal(reducer.renderedCount, 1);
});

test("StreamStateReducer attribution tracks engines by URL (singular format)", () => {
  const reducer = new StreamStateReducer();

  reducer.processFrame({
    event: "attribution",
    data: { url: "https://a.com", engine: "Google", attribution: "web" },
  });
  assert.equal(reducer.actions.length, 1); // First engine
  assert.deepEqual(reducer.actions[0].engines, ["Google"]);

  reducer.processFrame({
    event: "attribution",
    data: { url: "https://a.com", engine: "Bing", attribution: "web" },
  });
  assert.equal(reducer.actions.length, 1); // Action emitted for new engine added
  assert.deepEqual(reducer.actions[0].engines, ["Google", "Bing"]);

  reducer.processFrame({
    event: "attribution",
    data: { url: "https://a.com", engine: "Google", attribution: "web" },
  });
  assert.equal(reducer.actions.length, 0); // Duplicate engine, no action
});

test("StreamStateReducer attribution handles full-array format", () => {
  const reducer = new StreamStateReducer();

  reducer.processFrame({
    event: "attribution",
    data: { url: "https://a.com", engines: ["Google", "Bing"] },
  });
  assert.equal(reducer.actions.length, 1);
  assert.deepEqual(reducer.actions[0].engines, ["Google", "Bing"]);

  // Same array again - should be no-op
  reducer.processFrame({
    event: "attribution",
    data: { url: "https://a.com", engines: ["Google", "Bing"] },
  });
  assert.equal(reducer.actions.length, 0); // No change, no action

  // Different order but same set - should be no-op
  reducer.processFrame({
    event: "attribution",
    data: { url: "https://a.com", engines: ["Bing", "Google"] },
  });
  assert.equal(reducer.actions.length, 0); // Same set, no action

  // New engine added
  reducer.processFrame({
    event: "attribution",
    data: { url: "https://a.com", engines: ["Google", "Bing", "DuckDuckGo"] },
  });
  assert.equal(reducer.actions.length, 1); // New set with added engine
  assert.deepEqual(reducer.actions[0].engines, ["Google", "Bing", "DuckDuckGo"]);
});

test("StreamStateReducer attribution mixes singular and array formats", () => {
  const reducer = new StreamStateReducer();

  // Start with singular
  reducer.processFrame({
    event: "attribution",
    data: { url: "https://a.com", engine: "Google" },
  });
  assert.equal(reducer.actions.length, 1);
  assert.deepEqual(reducer.actions[0].engines, ["Google"]);

  // Update with array format containing same engine (this is a no-op)
  reducer.processFrame({
    event: "attribution",
    data: { url: "https://a.com", engines: ["Google"] },
  });
  assert.equal(reducer.actions.length, 0); // Same set, no action in this frame

  // Update with array containing new engines
  reducer.processFrame({
    event: "attribution",
    data: { url: "https://a.com", engines: ["Google", "Bing"] },
  });
  assert.equal(reducer.actions.length, 1); // New set, action emitted in this frame
  assert.deepEqual(reducer.actions[0].engines, ["Google", "Bing"]);
});

test("StreamStateReducer engine reports skip redundant updates", () => {
  const reducer = new StreamStateReducer();

  reducer.processFrame({
    event: "engine",
    data: { name: "Google", status: "success", count: 10 },
  });
  assert.equal(reducer.actions.length, 1);

  reducer.processFrame({
    event: "engine",
    data: { name: "Google", status: "success", count: 10 },
  });
  assert.equal(reducer.actions.length, 0); // Same report, no action

  reducer.processFrame({
    event: "engine",
    data: { name: "Google", status: "timeout" },
  });
  assert.equal(reducer.actions.length, 1); // Different report, action
});

test("StreamStateReducer handles token/cursor transitions", () => {
  const reducer = new StreamStateReducer();

  // Initial state with order1
  reducer.processFrame({
    event: "meta",
    data: { orderId: "order1", canonical: null, start: 0, count: 10 },
  });
  assert.equal(reducer.activeOrderToken, "order1");

  // Transition to a different order
  reducer.processFrame({
    event: "done",
    data: {
      activeOrderId: "order2",
      canonicalOrderId: "canon1",
      nextCursor: 20,
      hasMore: true,
    },
  });

  const snap = reducer.snapshot();
  assert.equal(snap.activeOrderToken, "order2");
  assert.equal(snap.canonicalOrderId, "canon1");
  assert.equal(snap.nextCursor, 20);
});

test("StreamStateReducer ignores frames with missing critical fields", () => {
  const reducer = new StreamStateReducer();

  reducer.processFrame({ event: "results", data: { result: { url: "x" } } }); // missing position
  assert.equal(reducer.actions.length, 0);

  reducer.processFrame({ event: "results", data: { position: 0 } }); // missing result
  assert.equal(reducer.actions.length, 0);

  reducer.processFrame({ event: "engine", data: { status: "ok" } }); // missing name
  assert.equal(reducer.actions.length, 0);
});

test("StreamStateReducer handles results with alternate URL field name (href)", () => {
  const reducer = new StreamStateReducer();

  reducer.processFrame({
    event: "results",
    data: { position: 0, result: { href: "https://a.com", title: "A" } },
  });

  assert.equal(reducer.seenUrls.has("https://a.com"), true);
  assert.equal(reducer.actions.length, 1);
});

test("StreamStateReducer handles identical attribution replays as no-op", () => {
  const reducer = new StreamStateReducer();

  // First update
  reducer.processFrame({
    event: "attribution",
    data: { url: "https://a.com", engines: ["Google", "Bing", "DuckDuckGo"] },
  });
  assert.equal(reducer.actions.length, 1);

  // Identical replay (same order)
  reducer.processFrame({
    event: "attribution",
    data: { url: "https://a.com", engines: ["Google", "Bing", "DuckDuckGo"] },
  });
  assert.equal(reducer.actions.length, 0); // No new action

  // Identical replay (different order, same set)
  reducer.processFrame({
    event: "attribution",
    data: { url: "https://a.com", engines: ["DuckDuckGo", "Google", "Bing"] },
  });
  assert.equal(reducer.actions.length, 0); // Still no new action (set is the same)
});

test("StreamStateReducer detects attribution changes despite order", () => {
  const reducer = new StreamStateReducer();

  // Initial state
  reducer.processFrame({
    event: "attribution",
    data: { url: "https://a.com", engines: ["Google", "Bing"] },
  });
  assert.equal(reducer.actions.length, 1);

  // Change set (add engine) - different order too
  reducer.processFrame({
    event: "attribution",
    data: { url: "https://a.com", engines: ["Bing", "Google", "DuckDuckGo"] },
  });
  assert.equal(reducer.actions.length, 1); // New action (set changed) in this frame

  // Change set (remove engine)
  reducer.processFrame({
    event: "attribution",
    data: { url: "https://a.com", engines: ["Google"] },
  });
  assert.equal(reducer.actions.length, 1); // New action (set changed) in this frame
});

test("Integration: SSEParser and StreamStateReducer together", () => {
  const parser = new SSEParser();
  const reducer = new StreamStateReducer();
  const actions = [];

  parser.on("frame", (frame) => {
    reducer.processFrame(frame);
    actions.push(...reducer.actions);
  });

  const sse = `event: meta
id: 1
data: {"orderId": "order1", "canonical": false, "start": 0, "count": 20}

event: results
id: 2
data: {"position": 0, "result": {"url": "https://a.com", "title": "A"}}

event: results
id: 3
data: {"position": 1, "result": {"url": "https://b.com", "title": "B"}}

event: done
id: 4
data: {"activeOrderId": "order1", "canonicalOrderId": "order1", "nextCursor": 20, "hasMore": false}

`;

  parser.feed(sse);

  const snap = reducer.snapshot();
  assert.equal(snap.isComplete, true);
  assert.equal(snap.renderedCount, 2);
  assert.equal(snap.seenUrlsCount, 2);
  assert.equal(snap.canonical, false);

  const appendActions = actions.filter((a) => a.type === "append");
  assert.equal(appendActions.length, 2);
  assert.equal(appendActions[0].result.url, "https://a.com");
  assert.equal(appendActions[1].result.url, "https://b.com");
});

test("Integration: SSEParser and StreamStateReducer with attribution", () => {
  const parser = new SSEParser();
  const reducer = new StreamStateReducer();
  const actions = [];

  parser.on("frame", (frame) => {
    reducer.processFrame(frame);
    actions.push(...reducer.actions);
  });

  const sse = `event: meta
id: 1
data: {"orderId": "order1", "canonical": false, "start": 0, "count": 20}

event: results
id: 2
data: {"position": 0, "result": {"url": "https://a.com", "title": "A"}}

event: attribution
id: 3
data: {"url": "https://a.com", "engines": ["Google", "Bing"]}

event: attribution
id: 4
data: {"url": "https://a.com", "engines": ["Google", "Bing", "DuckDuckGo"]}

event: done
id: 5
data: {"activeOrderId": "order1", "canonicalOrderId": "order1", "nextCursor": 10, "hasMore": true}

`;

  parser.feed(sse);

  const snap = reducer.snapshot();
  assert.equal(snap.isComplete, true);
  assert.equal(snap.renderedCount, 1);

  const attributionActions = actions.filter((a) => a.type === "updateAttribution");
  assert.equal(attributionActions.length, 2);
  assert.deepEqual(attributionActions[0].engines, ["Google", "Bing"]);
  assert.deepEqual(attributionActions[1].engines, ["Google", "Bing", "DuckDuckGo"]);
});

test("StreamStateReducer reads cached state from the result payload", () => {
  const reducer = new StreamStateReducer();

  reducer.processFrame({
    event: "results",
    data: {
      entries: [{
        position: 0,
        result: { url: "https://cached.example", cached: true },
      }],
    },
  });

  assert.equal(reducer.actions.length, 1);
  assert.equal(reducer.actions[0].cached, true);
});

test("StreamStateReducer ignores malformed attribution engine collections", () => {
  const reducer = new StreamStateReducer();

  reducer.processFrame({
    event: "attribution",
    data: { url: "https://a.com", engines: "Google" },
  });

  assert.equal(reducer.actions.length, 0);
  assert.equal(reducer.attributionByUrl.get("https://a.com").size, 0);
});
