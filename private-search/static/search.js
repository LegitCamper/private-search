const POLL_INTERVAL = 500; 
const numSearchSkels = 10;
const numImageSkels = 50;

let lastFetched = 0; 
let polling = false;
let skeletons = 0;        // total skeletons created so far
let batchLoading = false; // prevents multiple skeleton triggers
let currentTab = "general";

function get_query() {
  let params = new URLSearchParams(location.search);
  return params.get("q") || "";
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
      createImageSkeletons(numImageSkels, skeletons);
  } else {
      createSearchSkeletons(numSearchSkels, skeletons);
  }
  window.scrollTo(0, 0);

  let query = get_query();
  document.querySelector(".search-input").value = query;

  startPolling(query);
});

async function startPolling(query) {
  polling = true;
  pollResults(query);
}

function stopPolling() {
  polling = false;
  batchLoading = false;
}

function unwrapPayload(obj) {
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

function escapeHtml(str) {
  return String(str)
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;");
}

// Result urls come from scraped, untrusted third-party HTML. Only allow
// http(s) links (blocks `javascript:`/`data:` etc.) and escape the rest so
// it's safe to drop into an href/src attribute.
function safeUrl(url) {
  try {
    const parsed = new URL(String(url), location.href);
    if (parsed.protocol === "http:" || parsed.protocol === "https:") {
      return escapeHtml(parsed.href);
    }
  } catch (e) {
    // not a valid URL — fall through to blocking it
  }
  return "#";
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

async function pollResults(query) {
  if (!polling || query === undefined || query === null) return;

  try {
    const res = await fetch(
      `/query?tab=${currentTab}&query=${encodeURIComponent(query)}&start=${lastFetched}&count=${numSearchSkels}`
    );

    const text = await res.text();

    if (!res.ok) {
      throw new Error(`Failed to fetch results: ${res.status}`);
    }

    if (
      text.trim() === "Query Error" ||
      text.trim().includes("Query Error")
    ) {
      console.warn("Server returned Query Error, retrying...");
      setTimeout(() => pollResults(query), 1000);
      return;
    }

    let data;
    try {
      data = JSON.parse(text);
    } catch (err) {
      console.error("Failed to parse response as JSON:", text);
      setTimeout(() => pollResults(query), 1000);
      return;
    }

    const { results, engines, hasMore } = unwrapPayload(data);

    renderEngineStatus(engines);

    if (currentTab === "images") {
      renderImageResults(results);
    } else {
      renderSearchResults(results);
    }

    if (hasMore) {
      setTimeout(() => pollResults(query), POLL_INTERVAL);
    } else {
      stopPolling();
    }
  } catch (err) {
    console.error("Error polling results:", err);
    setTimeout(() => pollResults(query), 1000);
  }
}

function renderSearchResults(results) {
  let container = document.querySelector(".results-container");
  results.forEach((result, idx) => {
    // Compute which skeleton to fill
    const skeletonId = lastFetched + idx;
    let skeleton = container.querySelector(`.result-skeleton[data-result-id="${skeletonId}"]`);

    if (!skeleton) {
      // fallback: create one if it doesn't exist
      skeleton = document.createElement("article");
      skeleton.className = "result-skeleton";
      skeleton.dataset.resultId = skeletonId;
      container.appendChild(skeleton);
    }

    const enginesHtml = result.engines
      .map(e => `<span class="engine-tag">${escapeHtml(e)}</span>`)
      .join(" ");
    const href = safeUrl(result.url);

    // Fill content
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
}

function renderImageResults(results) {
  let container = document.querySelector(".image-gallery");
  results.forEach((result, idx) => {
    // Compute which skeleton to fill
    const skeletonId = lastFetched + idx;
    let skeleton = container.querySelector(`.result-skeleton[data-result-id="${skeletonId}"]`);

    if (!skeleton) {
      // fallback: create one if it doesn't exist
      skeleton = document.createElement("article");
      skeleton.className = "image-result";
      skeleton.dataset.resultId = skeletonId;
      container.appendChild(skeleton);
    }  

    const href = safeUrl(result.url);

    skeleton.innerHTML = `
      <a href="${href}" target="_blank" rel="noopener">
        <img src="${href}" class="image-thumb" alt="">
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
}

function createSearchSkeletons(count, start) {
  const container = document.querySelector(".results-container");
  for (let i = start; i < count; i++) {
    const sk = document.createElement("article");
    sk.className = "result-skeleton";
    sk.dataset.resultId = i;

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

    container.appendChild(sk);
  }

  skeletons += count;

}

function createImageSkeletons(count, start) {
  const container = document.querySelector(".image-gallery");
  for (let i = start; i < count; i++) {
    const sk = document.createElement("article");
    sk.className = "result-skeleton";
    sk.dataset.resultId = i;

    sk.innerHTML = `
      <div class="image-thumb skeleton"></div>
      <figcaption>
        <div class="skeleton skeleton-url"></div>
        <div class="skeleton skeleton-engine"></div>
      </figcaption>
    `;

    container.appendChild(sk);
  }

  skeletons += count;
}

window.addEventListener('scroll', () => {
  const scrollTop = window.scrollY || window.pageYOffset;
  const windowHeight = window.innerHeight;
  const docHeight = Math.max(
    document.body.scrollHeight,
    document.documentElement.scrollHeight
  );

  if (scrollTop + windowHeight >= docHeight - 500) {
    if (batchLoading) return; // already loading a batch

    batchLoading = true; // mark that we are loading

    if (currentTab === "images") {
        createImageSkeletons(numImageSkels, skeletons);
    } else {
        createSearchSkeletons(numSearchSkels, skeletons);
    }

    if (!polling) {
      startPolling(get_query()).finally(() => {
        batchLoading = false; // ready for next scroll batch
      });
    } else {
      setTimeout(() => batchLoading = false, POLL_INTERVAL);
    }
  }
});

function onSearchSubmit() {
  document.getElementById("search-type").value = currentTab;
  return true;
}
