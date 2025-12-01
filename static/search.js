const RESULTS_CONTAINER = document.querySelector(".results-container");
const POLL_INTERVAL = 500; 
const numPages = 10;

let lastFetched = 0; 
let polling = false;
let skeletons = 0;        // total skeletons created so far
let batchLoading = false; // prevents multiple skeleton triggers

function get_query() {
  let params = new URLSearchParams(location.search);
  return params.get("q") || "";
}

// Set active tab on page load
function setActiveTab() {
  const params = new URLSearchParams(window.location.search);
  const tab = params.get("t") || "general";

  document.querySelectorAll(".search-categories .category").forEach(el => {
    if (el.dataset.tab === tab) {
      el.classList.add("active");
    } else {
      el.classList.remove("active");
    }
  });
}

addEventListener("DOMContentLoaded", (event) => {
  setActiveTab()
  createSkeletons(numPages, 0)
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

function unwrapResults(obj) {
  if (!obj || typeof obj !== "object") return [];

  if (obj.General) return obj.General;
  if (obj.Images) return obj.Images;
  if (obj.Cs) return obj.Cs;

  console.warn("Unknown response variant:", obj);
  return [];
}

async function pollResults(query) {
  if (!polling || query === undefined || query === null) return;

  try {
    const res = await fetch(`/query?query=${query}&start=${lastFetched}&count=${numPages}`);
    if (!res.ok) throw new Error("Failed to fetch results");

    const data = await res.json();
    const results = unwrapResults(data);

    renderResults(results);   
    
    if (data.hasMore) {
      setTimeout(pollResults, POLL_INTERVAL);
    } else {
      stopPolling();
    }
  } catch (err) {
    console.error("Error polling results:", err);
    
    setTimeout(pollResults, 1000);
  }
}

function renderResults(results) {
  results.forEach((result, idx) => {
    // Compute which skeleton to fill
    const skeletonId = lastFetched + idx;
    let skeleton = RESULTS_CONTAINER.querySelector(`.result-skeleton[data-result-id="${skeletonId}"]`);

    if (!skeleton) {
      // fallback: create one if it doesn't exist
      skeleton = document.createElement("article");
      skeleton.className = "result-skeleton";
      skeleton.dataset.resultId = skeletonId;
      RESULTS_CONTAINER.appendChild(skeleton);
    }

    const enginesHtml = result.engines
      .map(e => `<span class="engine-tag">${e}</span>`)
      .join(" ");

    // Fill content
    skeleton.innerHTML = `
      <a class="url_header" target="_blank" rel="noopener noreferrer" href="${result.url}">${result.url}</a>
      <h3><a class="name" target="_blank" rel="noopener noreferrer" href="${result.url}">${result.title}</a></h3>
      <p class="description">${result.description}</p>
      <div class="engines">
        ${enginesHtml}
        ${result.cached ? '<span class="engine-tag cached">Cached ✓</span>' : ''}
      </div>
    `;
    skeleton.className = "result"; // remove skeleton styles
  });

  lastFetched += results.length;
}

function createSkeletons(count, start) {
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
    createSkeletons(numPages, skeletons)

    if (!polling) {
      startPolling(get_query()).finally(() => {
        batchLoading = false; // ready for next scroll batch
      });
    } else {
      setTimeout(() => batchLoading = false, POLL_INTERVAL);
    }
  }
});

