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

function unwrapResults(obj) {
  if (!obj || typeof obj !== "object") return [];

  if (obj.General) return obj.General;
  if (obj.Images) return obj.Images;

  console.warn("Unknown response variant:", obj);
  return [];
}

async function pollResults(query) {
  if (!polling || query === undefined || query === null) return;
  const params = new URLSearchParams(window.location.search);

  try {
    const res = await fetch(`/query?tab=${currentTab}&query=${query}&start=${lastFetched}&count=${numSearchSkels}`);
    if (!res.ok) throw new Error("Failed to fetch results");

    const data = await res.json();
    const results = unwrapResults(data);

    if (currentTab === "images") {
        renderImageResults(results);
    } else {
        renderSearchResults(results);
    }
    
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

    skeleton.innerHTML = `
      <a href="${result.url}" target="_blank" rel="noopener">
        <img src="${result.url}" class="image-thumb" alt="">
      </a>

      <figcaption>
        <div class="image-title">${result.title || ""}</div>
        <div class="engines">
          ${result.engines.map(e => `<span class="engine-tag">${e}</span>`).join(" ")}
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
