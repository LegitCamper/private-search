import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import vm from "node:vm";
import * as core from "./search-core.js";

const source = readFileSync(new URL("./search.js", import.meta.url), "utf8")
  .replace(/^import\s*\{[\s\S]*?\}\s*from\s*"\.\/search-core.js";/, "");

function harness(fetch) {
  const timers = new Map();
  const elements = new Map();
  let nextTimer = 0;
  let now = Date.now();
  const element = () => ({
    hidden: true, disabled: false, textContent: "", innerHTML: "", dataset: {},
    classList: { add() {}, remove() {} },
    appendChild() {}, remove() {}, setAttribute() {}, addEventListener() {},
    querySelector() { return element(); }, querySelectorAll() { return []; },
    getBoundingClientRect() { return { top: 0 }; },
  });
  const lookup = (id) => {
    if (!elements.has(id)) elements.set(id, element());
    return elements.get(id);
  };
  const context = vm.createContext({
    ...core, fetch, URLSearchParams, TextDecoder, AbortController,
    Date: class extends Date { static now() { return now; } },
    console: { warn() {} },
    location: { search: "?q=testing", href: "https://example.com/search?q=testing" },
    window: { innerHeight: 800 },
    document: {
      getElementById: lookup, querySelector: lookup,
      querySelectorAll: () => [], createElement: element,
    },
    addEventListener() {}, setInterval() {},
    setTimeout(callback, delay) { timers.set(++nextTimer, { callback, delay }); return nextTimer; },
    clearTimeout(timer) { timers.delete(timer); },
    IntersectionObserver: class { observe() {} disconnect() {} },
  });
  vm.runInContext(`${source}\n globalThis.app = { startPolling, loadNextPage,
    state: () => ({ batchLoading, polling, hasMoreResults, consecutiveFailures }) };`, context);
  return {
    app: context.app, elements, timers,
    async tick() {
      const [id, timer] = timers.entries().next().value;
      timers.delete(id);
      now += timer.delay;
      timer.callback();
      await new Promise(setImmediate);
      return timer.delay;
    },
  };
}

function response(frames, onCancel = () => {}) {
  return {
    ok: true,
    body: new ReadableStream({
      start(controller) {
        controller.enqueue(new TextEncoder().encode(frames.map(([event, data]) =>
          `event: ${event}\ndata: ${JSON.stringify(data)}\n\n`).join("")));
      },
      cancel: onCancel,
    }),
  };
}

test("metadata and failed-engine frames do not reset retry backoff", async () => {
  let cancelled = 0;
  const page = harness(async () => response([
    ["meta", { orderId: 1 }],
    ["engine", { engine: "Brave", status: { status: "failed", detail: "blocked" } }],
    ["error", { message: "sources unavailable" }],
  ], () => cancelled++));
  await page.app.startPolling("testing");
  assert.equal(await page.tick(), 3000);
  assert.equal(await page.tick(), 6000);
  assert.equal(page.app.state().consecutiveFailures, 3);
  assert.equal(page.elements.get("query-error-banner").hidden, false);
  assert.equal(cancelled, 3);
});

test("done releases loading before continuation without waiting for transport EOF", async () => {
  let calls = 0;
  const page = harness(async () => {
    calls++;
    return response([
      ["results", { entries: [{ position: calls - 1, result: { url: `https://example.com/${calls}`, title: "Result" } }] }],
      ["done", { activeOrderId: 1, nextCursor: calls, hasMore: calls < 3 }],
    ]);
  });
  await page.app.startPolling("testing");
  assert.equal(page.app.state().batchLoading, false);
  await page.tick();
  assert.equal(calls, 2);
  await page.tick();
  assert.equal(calls, 3);
  assert.equal(page.app.state().hasMoreResults, false);
  assert.equal(page.app.state().batchLoading, false);
});

test("expired order stops retries and offers restart instead", async () => {
  const page = harness(async () => ({
    ok: false, status: 400, headers: new Headers(),
    json: async () => ({ error: "unknown or expired order token" }),
  }));
  await page.app.startPolling("testing");
  assert.equal(page.timers.size, 0);
  assert.equal(page.app.state().polling, false);
  assert.match(page.elements.get("query-error-banner").textContent, /Restart search/);
});

test("typed upstream deadline survives terminal error", async () => {
  const page = harness(async () => response([
    ["error", { message: "paused", retryAfterMs: 120000 }],
  ]));
  await page.app.startPolling("testing");
  assert.equal([...page.timers.values()][0].delay, 120000);
});

test("HTTP Retry-After takes precedence over exponential backoff", async () => {
  const page = harness(async () => ({
    ok: false, status: 429, headers: new Headers({ "Retry-After": "60" }),
    json: async () => ({ error: "slow down" }),
  }));
  await page.app.startPolling("testing");
  assert.equal([...page.timers.values()][0].delay, 60000);
});

test("Retry-After accepts seconds, HTTP dates, and invalid values", () => {
  assert.equal(core.retryAfterMs("60"), 60000);
  assert.equal(core.retryAfterMs("Wed, 21 Oct 2015 07:28:00 GMT", Date.UTC(2015, 9, 21, 7, 27)), 60000);
  assert.equal(core.retryAfterMs("nonsense"), 0);
  assert.equal(core.retryAfterMs(null), 0);
});
