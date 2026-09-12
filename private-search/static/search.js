import {
  escapeHtml,
  safeUrl,
  getQueryParam,
  skeletonsNeeded,
  canLoadNextPage,
  isWithinPreloadRange,
  shouldAutoContinue,
  retryDelayMs,
  shouldFlushFirstPaint,
  sortBufferedByScore,
  SkeletonQueue,
  SSEParser,
  StreamStateReducer,
} from "./search-core.js";

const numSearchSkels = 10;
// Kept at or below `MAX_COUNT` in `main.rs`, which rejects anything larger:
// this is the *requested* page size, not just how many placeholders to draw,
// and the two have to match or the gallery keeps placeholders it can never
// fill.
const numImageSkels = 25;

const CONSECUTIVE_FAILURES_BEFORE_BANNER = 3;

// How close to the end of the results the user has to get before the next page
// starts loading.
const PRELOAD_MARGIN_PX = 500;
const HOLD_MS = 1200;

let renderedCount = 0;
let polling = false;
let batchLoading = false; // prevents multiple skeleton triggers
let currentTab = "general";
let consecutiveFailures = 0;
let hasMoreResults = true; // server said there could be another page; only fetch it once the user scrolls for it
let lastBatchSize = 0; // distinct results rendered by the most recent page
let retryTimer = null;

// Page-wide state persists across pagination and reconnects.
const seenUrls = new Set();
const urlToDom = new Map();

// A reducer and start position belong to one server window. A new page gets a
// fresh reducer because cache event IDs restart; reconnects reuse it.
let currentPageReducer = null;
let nextPageStart = 0;
let currentPageStart = 0;
let currentPageOrderToken = null;

const searchSkeletons = new SkeletonQueue();
const imageSkeletons = new SkeletonQueue();

// How many results one page of the current tab asks for. The same number is
// used for the request and for the placeholders drawn against it.
function pageSize() {
  return currentTab === "images" ? numImageSkels : numSearchSkels;
}

function get_query() {
  return getQueryParam(location.search, "q");
}

function url(u) {
  return safeUrl(u, location.href);
}

// Set active tab on page load
function setActiveTab() {
  const params = new URLSearchParams(window.location.search);
  currentTab = params.get("t") || "general";

  document.querySelectorAll(".search-categories .category").forEach(el => {
    if (el.dataset.tab === currentTab) {
      el.classList.add("active");
    } else {
      el.classList.remove("active");
    }
  });
}

addEventListener("DOMContentLoaded", (event) => {
  setActiveTab()
  if (currentTab === "images") {
      createImageSkeletons(pageSize());
  } else {
      createSearchSkeletons(pageSize());
  }
  window.scrollTo(0, 0);

  let query = get_query();
  document.querySelector(".search-input").value = query;

  startPolling(query);
});

async function startPolling(query) {
  polling = true;
  currentPageStart = nextPageStart;
  currentPageReducer = new StreamStateReducer();
  lastBatchSize = 0;
  await streamResults(query);
}

function stopPolling() {
  polling = false;
  clearRetry();
  // Only loadNextPage's finally handler releases batchLoading. Releasing it
  // here can race the next auto-started page.
}

function clearRetry() {
  if (retryTimer !== null) {
    clearTimeout(retryTimer);
    retryTimer = null;
  }
}

function scheduleRetry(query, status, message) {
  if (!polling || retryTimer !== null) return;

  onPollFailure(message);
  const delay = retryDelayMs(status, consecutiveFailures);
  retryTimer = setTimeout(() => {
    retryTimer = null;
    if (polling) streamResults(query);
  }, delay);
}

// Extracts `{error: "..."}` from a non-OK JSON response, falling back to
// the raw HTTP status when the body isn't the JSON error envelope the
// server is supposed to send (see `main.rs`'s `ApiErrorBody`).
async function describeError(res) {
  try {
    const body = await res.json();
    if (body && typeof body.error === "string") return body.error;
  } catch (e) {
    // body wasn't JSON — fall through to the status-based message
  }
  return `HTTP ${res.status}`;
}

// Shows/hides a small persistent banner once a poll has failed several times
// in a row — a single blip isn't worth alarming the user over (retries are
// cheap; results are already heavily cached), but silently retrying forever
// with zero feedback looks like the page is just broken.
function setErrorBanner(message) {
  const banner = document.getElementById("query-error-banner");
  if (!banner) return;
  banner.hidden = !message;
  banner.textContent = message || "";
}

function onPollFailure(message) {
  consecutiveFailures += 1;
  console.warn(`Query failed (${message}), retrying...`);
  if (consecutiveFailures >= CONSECUTIVE_FAILURES_BEFORE_BANNER) {
    setErrorBanner(`Search is having trouble responding (${message}) — still retrying…`);
  }
}

function onPollSuccess() {
  consecutiveFailures = 0;
  setErrorBanner(null);
}

async function streamResults(query) {
  if (!polling || query === undefined || query === null) return;

  const params = new URLSearchParams({
    tab: currentTab,
    query,
    start: currentPageStart,
    count: pageSize(),
  });
  if (currentPageOrderToken !== null) {
    params.set("order", currentPageOrderToken);
  }
  if (currentPageReducer.lastEventId !== null) {
    params.set("after", currentPageReducer.lastEventId);
  }

  let receivedTerminal = false;

  try {
    const res = await fetch(`/query/stream?${params}`);
    if (!res.ok) {
      scheduleRetry(query, res.status, await describeError(res));
      return;
    }
    if (!res.body) {
      scheduleRetry(query, 0, "empty response");
      return;
    }

    const parser = new SSEParser();
    const decoder = new TextDecoder();
    const reader = res.body.getReader();
    let markedSuccessful = false;
    let firstPaintBuffer = [];
    let firstPaintStartedAt = null;
    let firstPaintTimer = null;
    let firstPaintFlushed = currentPageStart !== 0;
    const flushFirstPaint = () => {
      if (firstPaintFlushed) return;
      firstPaintFlushed = true;
      if (firstPaintTimer !== null) clearTimeout(firstPaintTimer);
      for (const action of sortBufferedByScore(firstPaintBuffer)) renderResult(action);
      firstPaintBuffer = [];
    };

    parser.on("frame", (frame) => {
      currentPageReducer.processFrame(frame);

      // Meta identifies the exact order being read. Keep it immediately so a
      // disconnect before done reconnects to the same generation.
      if (currentPageReducer.activeOrderToken !== null) {
        currentPageOrderToken = currentPageReducer.activeOrderToken;
      }

      if (!markedSuccessful && frame.event !== "error") {
        markedSuccessful = true;
        onPollSuccess();
      }

      for (const action of currentPageReducer.actions) {
        switch (action.type) {
          case "append":
            if (!firstPaintFlushed && !currentPageReducer.cached) {
              firstPaintBuffer.push(action);
              if (firstPaintStartedAt === null) {
                firstPaintStartedAt = performance.now();
                firstPaintTimer = setTimeout(flushFirstPaint, HOLD_MS);
              }
              const flushNow = shouldFlushFirstPaint({
                elapsedMs: performance.now() - firstPaintStartedAt,
                holdMs: HOLD_MS,
                isComplete: currentPageReducer.isComplete,
                bufferedCount: firstPaintBuffer.length,
                pageSize: pageSize(),
              });
              if (flushNow) flushFirstPaint();
            } else {
              flushFirstPaint();
              renderResult(action);
            }
            break;
          case "updateAttribution":
            updateResultAttribution(action);
            break;
          case "updateEngine":
            renderEngineStatusIncremental(action.name, action.report);
            break;
          case "done":
            flushFirstPaint();
            receivedTerminal = true;
            hasMoreResults = action.hasMore;
            nextPageStart = action.nextCursor ?? currentPageReducer.serverCursor;
            currentPageOrderToken = action.activeOrderId ?? currentPageOrderToken;
            if (action.hasMore) {
              stopPolling();
              if (lastBatchSize === 0) dropUnfilledSkeletons();
              watchForEnd();
              scheduleContinue();
            } else {
              finishSearch();
            }
            break;
          case "error":
            flushFirstPaint();
            receivedTerminal = true;
            scheduleRetry(query, 0, action.message);
            break;
        }
      }
    });

    while (true) {
      const { done, value } = await reader.read();
      if (done) break;
      parser.feed(decoder.decode(value, { stream: true }));
    }

    const finalChunk = decoder.decode();
    if (finalChunk) parser.feed(finalChunk);
    parser.end();

    if (!receivedTerminal) {
      scheduleRetry(query, 0, "stream disconnected");
    }
  } catch (err) {
    if (!receivedTerminal) {
      scheduleRetry(query, 0, "network error");
    }
  }
}

function renderResult(action) {
  const result = action.result;
  const url = result.url || result.href;

  // Check if we've already rendered this URL (page-global, persistent)
  if (seenUrls.has(url)) {
    return;
  }

  seenUrls.add(url);
  lastBatchSize++;

  // Get or create skeleton
  const skeleton = currentTab === "images"
    ? imageSkeletons.next(() => makeImageSkeleton())
    : searchSkeletons.next(() => makeSearchSkeleton());

  if (currentTab === "images") {
    renderImageResult(skeleton, result, !!result.cached);
  } else {
    renderSearchResult(skeleton, result, !!result.cached);
  }

  urlToDom.set(url, skeleton);
  renderedCount++;
}

function renderSearchResult(skeleton, result, cached) {
  const enginesHtml = (result.engines || [])
    .map(e => `<span class="engine-tag">${escapeHtml(e)}</span>`)
    .join(" ");
  const href = url(result.url);

  skeleton.innerHTML = `
    <a class="url_header" target="_blank" rel="noopener noreferrer" href="${href}">${escapeHtml(result.url)}</a>
    <h3><a class="name" target="_blank" rel="noopener noreferrer" href="${href}">${escapeHtml(result.title)}</a></h3>
    <p class="description">${escapeHtml(result.description)}</p>
    <div class="engines">
      ${enginesHtml}
      ${cached ? '<span class="engine-tag cached">Cached ✓</span>' : ''}
    </div>
  `;
  skeleton.className = "result";
}

function renderImageResult(skeleton, result, cached) {
  const href = url(result.url);

  skeleton.innerHTML = `
    <a href="${href}" target="_blank" rel="noopener">
      <img src="${href}" class="image-thumb" alt="" loading="lazy" decoding="async">
    </a>

    <figcaption>
      <div class="image-title">${escapeHtml(result.title || "")}</div>
      <div class="engines">
        ${(result.engines || []).map(e => `<span class="engine-tag">${escapeHtml(e)}</span>`).join(" ")}
        ${cached ? '<span class="engine-tag cached">Cached ✓</span>' : ''}
      </div>
    </figcaption>
  `;

  skeleton.className = "image-result";
}

function updateResultAttribution(action) {
  const domElement = urlToDom.get(action.url);
  const enginesContainer = domElement && domElement.querySelector(".engines");
  if (!enginesContainer) return;

  const cachedTag = enginesContainer.querySelector(".engine-tag.cached");
  enginesContainer.replaceChildren();

  for (const engine of action.engines || []) {
    const tag = document.createElement("span");
    tag.className = "engine-tag";
    tag.textContent = engine;
    enginesContainer.appendChild(tag);
  }
  if (cachedTag) enginesContainer.appendChild(cachedTag);
}

function renderEngineStatusIncremental(engineName, report) {
  const container = document.getElementById("engine-status");
  if (!container) return;

  const status = (report.status && report.status.status) || "ok";
  const detail = report.status && report.status.detail;
  const label =
    status === "ok" ? "responded"
    : status === "timed_out" ? "timed out"
    : status === "cooling_down" ? "paused"
    : "failed";

  let row = Array.from(container.querySelectorAll(".engine-status-row"))
    .find(candidate => candidate.dataset.engine === engineName);
  if (!row) {
    row = document.createElement("div");
    row.className = "engine-status-row";
    row.dataset.engine = engineName;
    container.appendChild(row);
  }

  row.title = detail || "";
  row.innerHTML = `
    <span class="engine-status-dot ${escapeHtml(status)}"></span>
    <span class="engine-status-name">${escapeHtml(engineName)}</span>
    <span class="engine-status-detail">${label}</span>
  `;
}

// Called once a search has definitively run out of results (`hasMore` is
// false). Any skeletons still sitting in the queue were over-allocated and
// will never be filled — pull them off the page instead of leaving
// permanent loading placeholders, and if nothing was ever rendered at all,
// say so instead of just... showing nothing.
function finishSearch() {
  stopPolling();
  stopWatchingForEnd();
  dropUnfilledSkeletons();

  if (renderedCount === 0) {
    const container = document.querySelector(currentTab === "images" ? ".image-gallery" : ".results-container");
    const empty = document.createElement("p");
    empty.className = "empty-state";
    empty.textContent = currentTab === "images" ? "No images found." : "No results found.";
    container.appendChild(empty);
  }
}

function makeSearchSkeleton() {
  const sk = document.createElement("article");
  sk.className = "result-skeleton";

  sk.innerHTML = `
    <div class="url_header skeleton skeleton-url"></div>
    <h3 class="name skeleton skeleton-title"></h3>
    <p class="description">
      <span class="skeleton skeleton-description"></span>
      <span class="skeleton skeleton-description"></span>
      <span class="skeleton skeleton-description"></span>
    </p>
    <div class="engines">
      <span class="skeleton skeleton-engine"></span>
    </div>
  `;

  document.querySelector(".results-container").appendChild(sk);
  return sk;
}

function makeImageSkeleton() {
  const sk = document.createElement("article");
  sk.className = "result-skeleton";

  sk.innerHTML = `
    <div class="image-thumb skeleton"></div>
    <figcaption>
      <div class="skeleton skeleton-url"></div>
      <div class="skeleton skeleton-engine"></div>
    </figcaption>
  `;

  document.querySelector(".image-gallery").appendChild(sk);
  return sk;
}

function createSearchSkeletons(count) {
  for (let i = 0; i < count; i++) {
    searchSkeletons.push(makeSearchSkeleton());
  }
}

function createImageSkeletons(count) {
  for (let i = 0; i < count; i++) {
    imageSkeletons.push(makeImageSkeleton());
  }
}


// Pulls off any placeholders the current tab has left unfilled. Called both
// when the results run out for good and when a batch comes back empty — either
// way they will never be filled, and a stuck skeleton reads as "still loading"
// forever.
function dropUnfilledSkeletons() {
  const queue = currentTab === "images" ? imageSkeletons : searchSkeletons;
  queue.drain().forEach(sk => sk.remove());
}

// The end-of-results marker, declared in `search.html.hbs` after both result
// containers. Recreated here if it's missing so a stale cached page still
// paginates.
function sentinel() {
  let el = document.getElementById("load-more-sentinel");
  if (!el) {
    el = document.createElement("div");
    el.id = "load-more-sentinel";
    el.setAttribute("aria-hidden", "true");
    document.body.appendChild(el);
  }
  return el;
}

function sentinelInPreloadRange() {
  return isWithinPreloadRange(
    sentinel().getBoundingClientRect().top,
    window.innerHeight,
    PRELOAD_MARGIN_PX
  );
}

let endObserver = null;

// Watching the marker beats listening for `scroll`: an observer reports the
// *state* ("the end of the list is in view"), so a first page too short to
// scroll still loads a second one, and nothing depends on scroll events the
// browser stops sending once the user has reached the bottom.
function watchForEnd() {
  if (endObserver) return;
  endObserver = new IntersectionObserver(
    entries => {
      if (entries.some(e => e.isIntersecting)) loadNextPage();
    },
    { rootMargin: `${PRELOAD_MARGIN_PX}px 0px` }
  );
  endObserver.observe(sentinel());
}

function stopWatchingForEnd() {
  if (!endObserver) return;
  endObserver.disconnect();
  endObserver = null;
}

function loadNextPage() {
  // `polling` covers the initial request and retries as well as scroll-
  // initiated ones. Do not append placeholders while any of those is still
  // active: a slow initial response used to let every scroll event add
  // another page of skeletons.
  if (!canLoadNextPage({ batchLoading, polling, hasMoreResults })) return;

  batchLoading = true; // mark that we are loading
  lastBatchSize = 0; // Reset for the new page

  if (currentTab === "images") {
    createImageSkeletons(skeletonsNeeded(imageSkeletons.length, pageSize()));
  } else {
    createSearchSkeletons(skeletonsNeeded(searchSkeletons.length, pageSize()));
  }

  startPolling(get_query()).finally(() => {
    batchLoading = false; // ready for the next batch
  });
}

// Re-checks, after a batch lands, whether the end of the list is *still* in
// view — the observer alone won't say so, because a target that was already
// intersecting before the batch arrived reports no new entry, and a user
// parked at the bottom of the page produces no scroll events either. This is
// also what carries the retry path: a poll that only succeeded on its second
// or third attempt resolved its caller's promise long ago.
function maybeContinue() {
  if (
    shouldAutoContinue({
      lastBatchSize,
      inPreloadRange: sentinelInPreloadRange(),
      batchLoading,
      polling,
      hasMoreResults,
    })
  ) {
    loadNextPage();
  }
}

// Deferred so the just-rendered results are laid out before the marker's
// position is measured.
function scheduleContinue() {
  setTimeout(maybeContinue, 0);
}

function onSearchSubmit() {
  document.getElementById("search-type").value = currentTab;
  return true;
}

// `search.html.hbs` wires these up via inline `onsubmit`/`onclick` handlers,
// which look functions up on `window` — plain top-level `function`
// declarations aren't implicitly global anymore now that this file is an ES
// module, so they need to be attached explicitly.
window.onSearchSubmit = onSearchSubmit;
window.get_query = get_query;
