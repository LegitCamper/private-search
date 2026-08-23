import {
  escapeHtml,
  safeUrl,
  unwrapPayload,
  getQueryParam,
  skeletonsNeeded,
  canLoadNextPage,
  isWithinPreloadRange,
  shouldAutoContinue,
  SkeletonQueue,
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

let lastFetched = 0;
let polling = false;
let batchLoading = false; // prevents multiple skeleton triggers
let currentTab = "general";
let consecutiveFailures = 0;
let hasMoreResults = true; // server said there could be another page; only fetch it once the user scrolls for it
let lastBatchSize = 0; // results rendered by the most recent poll

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
  await pollResults(query);
}

function stopPolling() {
  polling = false;
  batchLoading = false;
}

function renderEngineStatus(engines) {
  const container = document.getElementById("engine-status");
  if (!container) return;

  if (!engines || engines.length === 0) {
    container.innerHTML = "";
    return;
  }

  container.innerHTML = engines
    .map(report => {
      const status = (report.status && report.status.status) || "ok";
      const detail = report.status && report.status.detail;
      const label =
        status === "ok" ? "responded"
        : status === "timed_out" ? "timed out"
        : status === "cooling_down" ? "paused"
        : "failed";

      return `
        <div class="engine-status-row" title="${detail ? escapeHtml(detail) : ""}">
          <span class="engine-status-dot ${escapeHtml(status)}"></span>
          <span class="engine-status-name">${escapeHtml(report.engine)}</span>
          <span class="engine-status-detail">${label}</span>
        </div>
      `;
    })
    .join("");
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

async function pollResults(query) {
  if (!polling || query === undefined || query === null) return;

  try {
    const res = await fetch(
      `/query?tab=${currentTab}&query=${encodeURIComponent(query)}&start=${lastFetched}&count=${pageSize()}`
    );

    if (!res.ok) {
      const message = await describeError(res);
      onPollFailure(message);
      // A 429 means we're rate limited — back off longer than the normal
      // retry interval instead of hammering the server further.
      setTimeout(() => pollResults(query), res.status === 429 ? 2000 : 1000);
      return;
    }

    let data;
    try {
      data = await res.json();
    } catch (err) {
      onPollFailure("bad response");
      setTimeout(() => pollResults(query), 1000);
      return;
    }

    onPollSuccess();

    const { results, engines, hasMore } = unwrapPayload(data);

    renderEngineStatus(engines);

    if (currentTab === "images") {
      renderImageResults(results);
    } else {
      renderSearchResults(results);
    }

    // Only fetch this page — the next one is loaded when the user scrolls
    // for it (see the `scroll` listener below), not automatically. Without
    // this, a single search would recursively page through the *entire*
    // result set, hammering the upstream engines with
    // requests no one asked for.
    hasMoreResults = hasMore;
    if (hasMore) {
      stopPolling();
      // A batch that filled none of its placeholders would otherwise leave
      // them sitting on the page as permanent "loading" rows.
      if (lastBatchSize === 0) dropUnfilledSkeletons();
      // Deliberately not started before the first batch renders: an observer
      // set up against an empty page reports its marker as visible straight
      // away and would fetch page 2 before the user asked for anything.
      watchForEnd();
      scheduleContinue();
    } else {
      finishSearch();
    }
  } catch (err) {
    onPollFailure("network error");
    setTimeout(() => pollResults(query), 1000);
  }
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

  if (lastFetched === 0) {
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

function renderSearchResults(results) {
  results.forEach((result) => {
    const skeleton = searchSkeletons.next(makeSearchSkeleton);

    const enginesHtml = result.engines
      .map(e => `<span class="engine-tag">${escapeHtml(e)}</span>`)
      .join(" ");
    const href = url(result.url);

    skeleton.innerHTML = `
      <a class="url_header" target="_blank" rel="noopener noreferrer" href="${href}">${escapeHtml(result.url)}</a>
      <h3><a class="name" target="_blank" rel="noopener noreferrer" href="${href}">${escapeHtml(result.title)}</a></h3>
      <p class="description">${escapeHtml(result.description)}</p>
      <div class="engines">
        ${enginesHtml}
        ${result.cached ? '<span class="engine-tag cached">Cached ✓</span>' : ''}
      </div>
    `;
    skeleton.className = "result"; // remove skeleton styles
  });

  lastFetched += results.length;
  lastBatchSize = results.length;
}

function renderImageResults(results) {
  results.forEach((result) => {
    const skeleton = imageSkeletons.next(makeImageSkeleton);
    const href = url(result.url);

    skeleton.innerHTML = `
      <a href="${href}" target="_blank" rel="noopener">
        <img src="${href}" class="image-thumb" alt="" loading="lazy" decoding="async">
      </a>

      <figcaption>
        <div class="image-title">${escapeHtml(result.title || "")}</div>
        <div class="engines">
          ${result.engines.map(e => `<span class="engine-tag">${escapeHtml(e)}</span>`).join(" ")}
          ${result.cached ? '<span class="engine-tag cached">Cached ✓</span>' : ''}
        </div>
      </figcaption>
    `;

    skeleton.className = "image-result";
  });
  lastFetched += results.length;
  lastBatchSize = results.length;
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
