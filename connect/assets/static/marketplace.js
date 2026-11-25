"use strict";

(() => {
  const TOKEN_KEY = "ozma.connect.review-token";
  const SAFE_IMAGE_PREFIX = /^data:image\/(?:png|jpeg|jpg|gif|webp|avif);base64,[a-z0-9+/=\s]+$/i;
  const SAFE_COLOR = /^(?:#[0-9a-f]{3}|#[0-9a-f]{4}|#[0-9a-f]{6}|#[0-9a-f]{8}|(?:rgb|hsl)a?\([0-9.% ,/+-]+\))$/i;
  const UNSAFE_URL_CHARS = /[\u0000-\u001f\u007f\\]/;

  const state = {
    offset: 0,
    limit: 24,
    total: 0,
    entries: [],
    detailId: null,
    lastFocused: null,
  };

  let searchTimer = null;
  let reviewToken = "";
  let reviewUnavailable = false;

  const byId = (id) => document.getElementById(id);

  function elt(tag, className, value) {
    const node = document.createElement(tag);
    if (className) {
      node.className = className;
    }
    if (value !== undefined && value !== null) {
      node.textContent = String(value);
    }
    return node;
  }

  function clear(node) {
    if (node) {
      node.replaceChildren();
    }
  }

  function shortened(value, limit) {
    const text = value === undefined || value === null ? "" : String(value);
    return text.length > limit ? text.slice(0, limit) + "…" : text;
  }

  function safeColor(value) {
    if (typeof value !== "string") {
      return null;
    }
    const colour = value.trim();
    if (!colour || colour.length > 64 || !SAFE_COLOR.test(colour)) {
      return null;
    }
    if (typeof CSS !== "undefined" && CSS.supports && !CSS.supports("color", colour)) {
      return null;
    }
    return colour;
  }

  function safeImageUrl(value) {
    if (typeof value !== "string") {
      return null;
    }
    const raw = value.trim();
    if (!raw || raw.length > 4096 || UNSAFE_URL_CHARS.test(raw)) {
      return null;
    }
    if (raw.toLowerCase().startsWith("data:")) {
      return SAFE_IMAGE_PREFIX.test(raw) ? raw : null;
    }
    if (raw.startsWith("//")) {
      return null;
    }
    let parsed = null;
    try {
      parsed = new URL(raw, window.location.origin);
    } catch (_error) {
      return null;
    }
    if (parsed.protocol !== "http:" && parsed.protocol !== "https:") {
      return null;
    }
    if (parsed.origin !== window.location.origin) {
      return null;
    }
    return parsed.href;
  }

  function formatValue(value, depth) {
    const level = depth || 0;
    if (value === undefined || value === null) {
      return "";
    }
    if (typeof value === "string") {
      return shortened(value, 1200);
    }
    if (typeof value === "number" || typeof value === "boolean") {
      return String(value);
    }
    if (Array.isArray(value)) {
      return value.map((item) => formatValue(item, level + 1)).join(", ");
    }
    if (typeof value === "object") {
      if (level >= 2) {
        return "…";
      }
      return Object.keys(value).sort().map((key) => (
        key + ": " + formatValue(value[key], level + 1)
      )).join("; ");
    }
    return "";
  }

  async function requestJSON(url, options) {
    const settings = Object.assign(
      { credentials: "same-origin", headers: { Accept: "application/json" } },
      options || {},
    );
    settings.headers = Object.assign({}, settings.headers || {});
    const response = await fetch(url, settings);
    let body = null;
    try {
      body = await response.json();
    } catch (_error) {
      body = null;
    }
    if (!response.ok) {
      const detail = body && body.detail;
      let message = "Request failed (HTTP " + response.status + ")";
      let code = "";
      if (detail && typeof detail === "object") {
        message = detail.message || message;
        code = detail.code || "";
      } else if (typeof detail === "string") {
        message = detail;
      }
      const error = new Error(message);
      error.status = response.status;
      error.code = code;
      throw error;
    }
    return body || {};
  }

  function errorText(error) {
    const message = error && error.message ? error.message : "Request failed";
    const code = error && error.code ? " (" + error.code + ")" : "";
    return message + code;
  }

  const els = {};

  function cacheElements() {
    [
      "marketplace-q", "marketplace-type", "marketplace-tag",
      "marketplace-licence", "marketplace-sort", "marketplace-page-size",
      "marketplace-search", "marketplace-reset", "marketplace-status",
      "marketplace-error", "marketplace-results", "marketplace-prev",
      "marketplace-next", "marketplace-page", "marketplace-detail",
      "detail-title", "detail-body", "detail-back", "review-toggle",
      "review-panel", "review-token-form", "review-token", "review-forget",
      "review-message", "review-content", "review-refresh", "review-list",
    ].forEach((id) => {
      els[id] = byId(id);
    });
  }

  function showResultsError(message) {
    if (!els["marketplace-error"]) {
      return;
    }
    els["marketplace-error"].textContent = message;
    els["marketplace-error"].hidden = false;
  }

  function clearResultsError() {
    if (els["marketplace-error"]) {
      els["marketplace-error"].hidden = true;
      els["marketplace-error"].textContent = "";
    }
  }

  function currentLimit() {
    const selected = Number(els["marketplace-page-size"].value);
    return Number.isFinite(selected) && selected > 0 ? selected : 24;
  }

  function searchParameters() {
    const params = new URLSearchParams();
    const q = els["marketplace-q"].value.trim().slice(0, 200);
    const type = els["marketplace-type"].value;
    const tag = els["marketplace-tag"].value;
    const licence = els["marketplace-licence"].value;
    const sort = els["marketplace-sort"].value || "name";
    if (q) {
      params.set("q", q);
    }
    if (type) {
      params.set("type", type);
    }
    if (tag) {
      params.set("tag", tag);
    }
    if (licence) {
      params.set("licence", licence);
    }
    params.set("sort", sort);
    params.set("limit", String(state.limit));
    params.set("offset", String(state.offset));
    return params;
  }

  function sortEntries(entries) {
    const sort = els["marketplace-sort"].value;
    const copy = entries.slice();
    if (sort === "name_desc") {
      copy.sort((a, b) => String(b.name || "").localeCompare(String(a.name || "")));
    } else if (sort === "author") {
      copy.sort((a, b) => (
        String(a.author || "").localeCompare(String(b.author || "")) ||
        String(a.name || "").localeCompare(String(b.name || ""))
      ));
    } else if (sort === "published") {
      copy.sort((a, b) => String(b.published_at || "").localeCompare(String(a.published_at || "")));
    } else if (sort === "id") {
      copy.sort((a, b) => String(a.id || "").localeCompare(String(b.id || "")));
    } else {
      copy.sort((a, b) => String(a.name || "").localeCompare(String(b.name || "")));
    }
    return copy;
  }

  function packCard(entry) {
    const packId = String(entry.id || "");
    const item = elt("li", "pack");
    item.setAttribute("data-pack-id", packId);

    const preview = safeImageUrl(entry.preview);
    if (preview) {
      const image = elt("img", "preview");
      image.src = preview;
      image.alt = "";
      image.loading = "lazy";
      image.decoding = "async";
      image.referrerPolicy = "no-referrer";
      item.append(image);
    }

    const heading = elt("h2");
    const link = elt("a", "", entry.name || packId || "Untitled pack");
    link.href = "/assets/entry/" + encodeURIComponent(packId);
    link.addEventListener("click", (event) => {
      if (
        event.button !== 0 ||
        event.metaKey ||
        event.ctrlKey ||
        event.shiftKey ||
        event.altKey
      ) {
        return;
      }
      event.preventDefault();
      openDetail(packId);
    });
    heading.append(link);
    item.append(heading);

    const meta = elt("div", "meta");
    meta.append(elt("span", "", entry.author || "Unknown author"));
    meta.append(elt("span", "", "Version " + (entry.version || "unknown")));
    meta.append(elt("span", "", entry.license || "Unknown licence"));
    if (entry.type) {
      meta.append(elt("span", "", entry.type));
    }
    item.append(meta);

    if (entry.description) {
      item.append(elt("p", "description", shortened(entry.description, 280)));
    }

    if (Array.isArray(entry.tags) && entry.tags.length) {
      const tags = elt("div", "tags");
      entry.tags.forEach((tag) => {
        if (String(tag).trim()) {
          tags.append(elt("span", "tag", tag));
        }
      });
      item.append(tags);
    }
    return item;
  }

  function renderResults() {
    const list = els["marketplace-results"];
    clear(list);
    if (!state.entries.length) {
      list.append(elt("li", "empty", "No approved packs match this view."));
    } else {
      state.entries.forEach((entry) => list.append(packCard(entry)));
    }

    const shown = state.entries.length;
    if (state.total) {
      const first = shown ? state.offset + 1 : 0;
      const last = state.offset + shown;
      els["marketplace-status"].textContent = (
        "Showing " + first + "–" + last + " of " + state.total +
        " approved packs."
      );
      const pages = Math.max(1, Math.ceil(state.total / state.limit));
      const current = Math.min(pages, Math.floor(state.offset / state.limit) + 1);
      els["marketplace-page"].textContent = "Page " + current + " of " + pages;
    } else {
      els["marketplace-status"].textContent = "No approved packs match this view.";
      els["marketplace-page"].textContent = "";
    }
    els["marketplace-prev"].disabled = state.offset <= 0;
    els["marketplace-next"].disabled = state.offset + shown >= state.total;
  }

  async function runSearch(reset) {
    if (reset) {
      state.offset = 0;
    }
    state.limit = currentLimit();
    clearResultsError();
    els["marketplace-results"].setAttribute("aria-busy", "true");
    try {
      const data = await requestJSON(
        "/assets/search?" + searchParameters().toString(),
      );
      state.entries = sortEntries(Array.isArray(data.entries) ? data.entries : []);
      state.total = Number(data.total) || 0;
      renderResults();
    } catch (error) {
      state.entries = [];
      state.total = 0;
      renderResults();
      showResultsError("Could not load packs: " + errorText(error));
    } finally {
      els["marketplace-results"].removeAttribute("aria-busy");
    }
  }

  function addDefinition(list, label, value) {
    if (value === undefined || value === null || value === "") {
      return;
    }
    list.append(elt("dt", "", label));
    list.append(elt("dd", "", formatValue(value)));
  }

  function metadataBlock(title, value) {
    const section = elt("section", "detail-block");
    section.append(elt("h3", "", title));
    if (value && typeof value === "object" && !Array.isArray(value)) {
      const list = elt("dl", "detail-list");
      Object.keys(value).sort().forEach((key) => {
        addDefinition(list, key, value[key]);
      });
      section.append(list);
      const colourValue = value.color || value.accent || value.tint;
      const colour = safeColor(colourValue);
      if (colour) {
        const swatch = elt("span", "colour-swatch");
        swatch.style.backgroundColor = colour;
        swatch.title = "Validated metadata colour";
        section.append(swatch);
      }
    } else {
      const text = formatValue(value);
      if (text) {
        section.append(elt("p", "detail-value", text));
      }
    }
    return section;
  }

  function renderAssets(body, assets) {
    if (!Array.isArray(assets) || !assets.length) {
      return;
    }
    const section = elt("section", "detail-block");
    section.append(elt("h3", "", "Assets"));
    const list = elt("ul", "asset-list");
    assets.forEach((asset) => {
      const item = elt("li");
      const path = elt("code", "", asset.path || asset.id || "asset");
      item.append(path);
      const details = [];
      if (asset.mime || asset.mime_type) {
        details.push(asset.mime || asset.mime_type);
      }
      if (asset.size_bytes !== undefined || asset.size !== undefined) {
        details.push(String(asset.size_bytes !== undefined ? asset.size_bytes : asset.size) + " bytes");
      }
      if (asset.executable) {
        details.push("executable");
      }
      if (details.length) {
        item.append(elt("span", "muted", " — " + details.join(", ")));
      }
      list.append(item);
    });
    section.append(list);
    body.append(section);
  }

  function renderVersions(body, packId, versions, currentVersion) {
    if (!Array.isArray(versions) || !versions.length) {
      return;
    }
    const section = elt("section", "detail-block");
    section.append(elt("h3", "", "Versions"));
    const list = elt("ul", "version-list");
    versions.forEach((item) => {
      const version = String(item.version || "");
      if (!version) {
        return;
      }
      const row = elt("li");
      const button = elt("button", "secondary", version);
      button.type = "button";
      button.setAttribute("aria-pressed", version === currentVersion ? "true" : "false");
      button.addEventListener("click", () => openDetail(packId, version));
      row.append(button);
      if (version === currentVersion) {
        row.append(elt("span", "muted", " current"));
      }
      if (item.yanked) {
        row.append(elt("span", "badge", " yanked"));
      }
      list.append(row);
    });
    section.append(list);
    body.append(section);
  }

  function renderDetail(entry, versionsData, packId) {
    const body = els["detail-body"];
    clear(body);
    const name = String(entry.name || packId || "Untitled pack");
    els["detail-title"].textContent = name;

    const preview = safeImageUrl(entry.preview);
    if (preview) {
      const figure = elt("figure", "detail-preview");
      const image = elt("img");
      image.src = preview;
      image.alt = "Preview of " + name;
      image.loading = "lazy";
      image.decoding = "async";
      image.referrerPolicy = "no-referrer";
      figure.append(image);
      body.append(figure);
    }

    const summary = elt("dl", "detail-list");
    addDefinition(summary, "Pack id", entry.id || packId);
    addDefinition(summary, "Author", entry.author || "Unknown author");
    addDefinition(summary, "Version", entry.version);
    addDefinition(summary, "Type", entry.type);
    addDefinition(summary, "Licence", entry.license || entry.licence || "Unknown licence");
    addDefinition(summary, "Description", entry.description);
    addDefinition(summary, "Source", entry.source_url);
    addDefinition(summary, "Published", entry.published_at);
    addDefinition(summary, "Confidence", entry.confidence);
    body.append(summary);

    if (Array.isArray(entry.tags) && entry.tags.length) {
      const tags = elt("div", "tags");
      entry.tags.forEach((tag) => tags.append(elt("span", "tag", tag)));
      body.append(tags);
    }

    ["provenance", "vibe", "shape", "coherence"].forEach((key) => {
      if (entry[key] !== undefined && entry[key] !== null && entry[key] !== "") {
        body.append(metadataBlock(key.charAt(0).toUpperCase() + key.slice(1), entry[key]));
      }
    });

    renderAssets(body, entry.assets);
    renderVersions(
      body,
      packId,
      versionsData && Array.isArray(versionsData.versions) ? versionsData.versions : [],
      String(entry.version || ""),
    );

    const close = elt("button", "secondary", "Back to results");
    close.type = "button";
    close.addEventListener("click", closeDetail);
    body.append(close);
  }

  async function openDetail(packId, version) {
    if (!packId) {
      return;
    }
    state.lastFocused = document.activeElement;
    state.detailId = packId;
    els["marketplace-detail"].hidden = false;
    els["detail-title"].textContent = "Pack details";
    clear(els["detail-body"]);
    els["detail-body"].append(elt("p", "status", "Loading pack details…"));

    const versionQuery = version
      ? "?version=" + encodeURIComponent(version)
      : "";
    try {
      const results = await Promise.all([
        requestJSON("/assets/entry/" + encodeURIComponent(packId) + versionQuery),
        requestJSON("/assets/versions/" + encodeURIComponent(packId)).catch(() => null),
      ]);
      renderDetail(results[0], results[1], packId);
    } catch (error) {
      clear(els["detail-body"]);
      els["detail-body"].append(elt(
        "p",
        "status error",
        "Could not load this pack: " + errorText(error),
      ));
    }
    els["detail-title"].focus();
    const reduced = window.matchMedia &&
      window.matchMedia("(prefers-reduced-motion: reduce)").matches;
    if (els["marketplace-detail"].scrollIntoView) {
      els["marketplace-detail"].scrollIntoView({
        block: "start",
        behavior: reduced ? "auto" : "smooth",
      });
    }
  }

  function closeDetail() {
    els["marketplace-detail"].hidden = true;
    clear(els["detail-body"]);
    state.detailId = null;
    if (state.lastFocused && state.lastFocused.isConnected) {
      state.lastFocused.focus();
    } else {
      const heading = byId("results-heading");
      if (heading) {
        heading.tabIndex = -1;
        heading.focus();
      }
    }
  }

  function reviewMessage(message, isError) {
    const node = els["review-message"];
    if (!node) {
      return;
    }
    node.textContent = message;
    node.className = isError ? "status error" : "status";
  }

  function readStoredToken() {
    try {
      return window.sessionStorage.getItem(TOKEN_KEY) || "";
    } catch (_error) {
      return "";
    }
  }

  function storeToken(value) {
    try {
      if (value) {
        window.sessionStorage.setItem(TOKEN_KEY, value);
      } else {
        window.sessionStorage.removeItem(TOKEN_KEY);
      }
    } catch (_error) {
      // The token still works in memory when sessionStorage is unavailable.
    }
  }

  function reviewHeaders() {
    return { "X-Ozma-Review-Token": reviewToken };
  }

  function disableReview(message) {
    reviewUnavailable = true;
    els["review-token-form"].hidden = true;
    els["review-content"].hidden = true;
    els["review-forget"].hidden = true;
    reviewMessage(message, true);
  }

  function handleReviewError(error) {
    if (error && error.status === 503) {
      disableReview("The review API is unavailable in this deployment (HTTP 503).");
      return;
    }
    if (error && (error.status === 401 || error.status === 403)) {
      reviewMessage("The review token was rejected. Enter a valid operator token.", true);
      return;
    }
    reviewMessage("Review request failed: " + errorText(error), true);
  }

  function reviewItem(item) {
    const row = elt("li", "review-item");
    const heading = elt("h3", "", item.name || item.pack_id || item.id || "Untitled pack");
    row.append(heading);

    const meta = elt("div", "meta");
    meta.append(elt("span", "", "Version " + (item.version || "unknown")));
    meta.append(elt("span", "", "Status " + (item.status || "unknown")));
    if (item.author) {
      meta.append(elt("span", "", item.author));
    }
    if (item.submitted_at) {
      meta.append(elt("span", "", "Submitted " + item.submitted_at));
    }
    row.append(meta);

    const noteId = "review-note-" + String(item.pack_id || item.id || "").replace(/[^a-z0-9_-]/gi, "-");
    const label = elt("label", "field field-wide", "Review note");
    label.htmlFor = noteId;
    const note = elt("textarea");
    note.id = noteId;
    note.rows = 3;
    note.maxLength = 2000;
    note.value = item.note || "";
    label.append(note);
    row.append(label);

    const actions = elt("div", "actions");
    const approve = elt("button", "", "Approve");
    approve.type = "button";
    const reject = elt("button", "danger", "Reject");
    reject.type = "button";
    actions.append(approve, reject);
    row.append(actions);

    const decide = (action) => decideReview(item, action, note, [approve, reject]);
    approve.addEventListener("click", () => decide("approve"));
    reject.addEventListener("click", () => decide("reject"));
    return row;
  }

  function renderReviewList(reviews) {
    const list = els["review-list"];
    clear(list);
    if (!Array.isArray(reviews) || !reviews.length) {
      list.append(elt("li", "empty", "No pending reviews."));
      return;
    }
    reviews.forEach((item) => list.append(reviewItem(item)));
  }

  async function loadPending() {
    if (reviewUnavailable || !reviewToken) {
      reviewMessage("Enter the operator token to load pending reviews.", false);
      return;
    }
    els["review-refresh"].disabled = true;
    reviewMessage("Loading pending reviews…", false);
    try {
      const data = await requestJSON(
        "/assets/review?status=pending&limit=50&offset=0",
        { headers: reviewHeaders() },
      );
      const reviews = Array.isArray(data.reviews)
        ? data.reviews
        : (Array.isArray(data.entries) ? data.entries : []);
      renderReviewList(reviews);
      reviewMessage(
        reviews.length
          ? reviews.length + " pending review(s)."
          : "No pending reviews.",
        false,
      );
    } catch (error) {
      handleReviewError(error);
    } finally {
      els["review-refresh"].disabled = reviewUnavailable;
    }
  }

  async function decideReview(item, action, noteField, buttons) {
    if (action === "reject" && !noteField.value.trim()) {
      reviewMessage("A note is required to reject a pack.", true);
      noteField.focus();
      return;
    }
    const packId = String(item.pack_id || item.id || "");
    const version = String(item.version || "");
    buttons.forEach((button) => {
      button.disabled = true;
    });
    reviewMessage(
      action === "approve" ? "Approving pack…" : "Rejecting pack…",
      false,
    );
    try {
      await requestJSON(
        "/assets/review/" + encodeURIComponent(packId) + "/" + action,
        {
          method: "POST",
          headers: Object.assign(
            { "Content-Type": "application/json" },
            reviewHeaders(),
          ),
          body: JSON.stringify({
            version: version || null,
            note: noteField.value.trim().slice(0, 2000),
          }),
        },
      );
      reviewMessage(
        action === "approve" ? "Pack approved." : "Pack rejected.",
        false,
      );
      await loadPending();
    } catch (error) {
      handleReviewError(error);
    } finally {
      buttons.forEach((button) => {
        button.disabled = reviewUnavailable;
      });
    }
  }

  function openReviewPanel() {
    els["review-panel"].hidden = false;
    els["review-toggle"].setAttribute("aria-expanded", "true");
    if (reviewToken && !reviewUnavailable) {
      els["review-content"].hidden = false;
      els["review-forget"].hidden = false;
      loadPending();
      els["review-refresh"].focus();
    } else {
      els["review-token"].focus();
    }
  }

  function closeReviewPanel() {
    els["review-panel"].hidden = true;
    els["review-toggle"].setAttribute("aria-expanded", "false");
    els["review-toggle"].focus();
  }

  function bindEvents() {
    els["marketplace-search"].addEventListener("submit", (event) => {
      event.preventDefault();
      runSearch(true);
    });
    els["marketplace-reset"].addEventListener("click", () => {
      window.setTimeout(() => {
        els["marketplace-sort"].value = "name";
        els["marketplace-page-size"].value = "24";
        runSearch(true);
      }, 0);
    });
    ["marketplace-type", "marketplace-tag", "marketplace-licence",
      "marketplace-sort", "marketplace-page-size"].forEach((id) => {
      els[id].addEventListener("change", () => runSearch(true));
    });
    els["marketplace-q"].addEventListener("input", () => {
      if (searchTimer) {
        window.clearTimeout(searchTimer);
      }
      searchTimer = window.setTimeout(() => runSearch(true), 300);
    });
    els["marketplace-prev"].addEventListener("click", () => {
      state.offset = Math.max(0, state.offset - state.limit);
      runSearch(false);
    });
    els["marketplace-next"].addEventListener("click", () => {
      state.offset += state.limit;
      runSearch(false);
    });
    els["detail-back"].addEventListener("click", closeDetail);
    document.addEventListener("keydown", (event) => {
      if (event.key === "Escape" && !els["marketplace-detail"].hidden) {
        closeDetail();
      }
    });
    els["review-toggle"].addEventListener("click", () => {
      if (els["review-panel"].hidden) {
        openReviewPanel();
      } else {
        closeReviewPanel();
      }
    });
    els["review-token-form"].addEventListener("submit", (event) => {
      event.preventDefault();
      const value = els["review-token"].value.trim();
      if (!value) {
        reviewMessage("Enter the operator review token.", true);
        els["review-token"].focus();
        return;
      }
      reviewToken = value;
      storeToken(value);
      els["review-token"].value = "";
      els["review-content"].hidden = false;
      els["review-forget"].hidden = false;
      loadPending();
      els["review-refresh"].focus();
    });
    els["review-forget"].addEventListener("click", () => {
      reviewToken = "";
      storeToken("");
      els["review-token"].value = "";
      els["review-content"].hidden = true;
      els["review-forget"].hidden = true;
      clear(els["review-list"]);
      reviewMessage("Review token cleared from this tab.", false);
      els["review-token"].focus();
    });
    els["review-refresh"].addEventListener("click", () => loadPending());
  }

  function init() {
    cacheElements();
    reviewToken = readStoredToken();
    state.limit = currentLimit();
    bindEvents();
    if (reviewToken && !reviewUnavailable) {
      els["review-panel"].hidden = false;
      els["review-toggle"].setAttribute("aria-expanded", "true");
      els["review-content"].hidden = false;
      els["review-forget"].hidden = false;
      loadPending();
    }
    runSearch(false);
  }

  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", init);
  } else {
    init();
  }
})();
