import { test } from "node:test";
import assert from "node:assert/strict";
import {
  escapeHtml,
  safeUrl,
  unwrapPayload,
  getQueryParam,
  skeletonsNeeded,
  canLoadNextPage,
  SkeletonQueue,
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
