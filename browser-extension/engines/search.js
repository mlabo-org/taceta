const DEFAULT_LIMIT = 10;
const MAX_LIMIT = 50;
const DEFAULT_TIMEOUT_MS = 60_000;

export const SEARCH_ERROR_CODES = Object.freeze({
  CAPTCHA: "captcha_detected",
  CONSENT: "consent_required",
  NO_RESULTS_PAGE: "results_page_unavailable",
  TIMEOUT: "search_timeout",
  INVALID_TAB: "owned_tab_required",
});

export class SearchEngineError extends Error {
  constructor(code, message = code) {
    super(message);
    this.name = "SearchEngineError";
    this.code = code;
  }
}

export function normalizeLimit(value = DEFAULT_LIMIT) {
  const parsed = Number(value);
  return Math.max(1, Math.min(MAX_LIMIT, Number.isFinite(parsed) ? Math.floor(parsed) : DEFAULT_LIMIT));
}

export function normalizeTimeout(value = DEFAULT_TIMEOUT_MS) {
  const parsed = Number(value);
  return Math.max(250, Math.min(120_000, Number.isFinite(parsed) ? Math.floor(parsed) : DEFAULT_TIMEOUT_MS));
}

// This function is serialized by chrome.scripting. Keep all DOM-reading helpers
// inside it so the browser and source tests execute identical code.
export function readSearchDocument(limit = 10, documentLike = document) {
  const textOf = (node) => String(node?.innerText ?? node?.textContent ?? "").trim();
  const bodyText = textOf(documentLike.body);
  const external = (value) => {
    try {
      let url = new URL(value, documentLike.location?.href || "https://www.google.com/");
      if (/(^|\.)google\.[a-z.]+$/i.test(url.hostname)) {
        const target = url.searchParams.get("q") || url.searchParams.get("url") || url.searchParams.get("uddg");
        if (!target) return null;
        url = new URL(target);
      }
      if (url.protocol !== "https:" || url.username || url.password || /(^|\.)google\.[a-z.]+$/i.test(url.hostname)) return null;
      return url.href;
    } catch { return null; }
  };
  // Check Google's challenge surface, not words in the returned web snippets.
  const challenge = documentLike.querySelector?.('form[action*="/sorry/"], #captcha-form, iframe[src*="recaptcha"]');
  const search = documentLike.querySelector?.("#search");
  if (challenge || (!search && /(unusual traffic|人間であること|ロボットではない)/i.test(bodyText))) return { state: "captcha" };
  if (!search && /(before you continue|consent|同意|プライバシーと利用規約)/i.test(bodyText)) return { state: "consent" };
  const overview = documentLike.querySelector?.('#m-x-content');
  const heading = [...documentLike.querySelectorAll('[role="heading"], h2')]
    .find((node) => /^(AI\s*による概要|AI Overview)$/i.test(textOf(node)));
  const region = overview || heading?.closest?.('[data-subtree="mfc"]');
  if (!search && !region) return { state: "unavailable" };
  const results = [];
  const seen = new Set();
  const maximum = Math.max(1, Math.min(50, Math.floor(Number(limit) || 10)));
  const cards = [...documentLike.querySelectorAll("#search .MjjYud")];
  const headings = [...documentLike.querySelectorAll("#search h3")];
  const candidates = headings.length ? headings.map((h) => ({ heading: h, card: h.closest?.(".MjjYud") || h.parentElement }))
    : cards.map((card) => ({ heading: card.querySelector?.("h3"), card }));
  for (const { heading: h, card } of candidates) {
    if (region?.contains?.(h)) continue;
    const anchor = h?.closest?.("a");
    const url = external(anchor?.getAttribute?.("href") || anchor?.href);
    const title = textOf(h);
    if (!url || !title || seen.has(url)) continue;
    seen.add(url);
    results.push({ title, url, snippet: textOf(card?.querySelector?.(".VwiC3b")).replace(/\s+/g, " ") });
    if (results.length >= maximum) break;
  }
  let ai_overview = { status: heading ? "unavailable" : "absent" };
  if (region) {
    const answer = region.querySelector('[data-subtree="aimc"]');
    const main = answer?.querySelector('[data-container-id="main-col"]');
    const envelope = region.closest?.('[data-complete]');
    const busy = region.querySelector('[aria-busy="true"], [data-complete="false"]');
    const complete = envelope?.getAttribute('data-complete') === 'true'
      && answer?.getAttribute('data-complete') === 'true' && !busy;
    const text = textOf(main);
    if (complete && text) {
      const citations = [...new Set([...answer.querySelectorAll('a[href]')]
        .map((a) => external(a.getAttribute('href') || a.href)).filter(Boolean))];
      ai_overview = { status: "complete", text, citations };
    } else {
      // No partial text crosses the browser boundary, even on a timeout.
      ai_overview = { status: complete ? "unavailable" : "pending" };
    }
  }
  return { state: "ready", results, ai_overview };
}

export function detectSearchPageState(documentLike) {
  return readSearchDocument(DEFAULT_LIMIT, documentLike).state;
}

function requireSearchPage(snapshot) {
  const code = { captcha: SEARCH_ERROR_CODES.CAPTCHA, consent: SEARCH_ERROR_CODES.CONSENT, unavailable: SEARCH_ERROR_CODES.NO_RESULTS_PAGE }[snapshot.state];
  if (code) throw new SearchEngineError(code);
  return snapshot;
}

export function extractSearchResults(documentLike, limit = DEFAULT_LIMIT) {
  return requireSearchPage(readSearchDocument(limit, documentLike)).results;
}

export function googleSearchUrl(query) {
  return `https://www.google.com/search?q=${encodeURIComponent(String(query))}`;
}

async function waitForTab(tab, timeoutMs) {
  if (typeof tab.waitForLoad === "function") return tab.waitForLoad(timeoutMs);
  if (typeof tab.waitForLoadState === "function") return tab.waitForLoadState({ state: "load", timeoutMs });
  return undefined;
}

async function collectSearchPage(tab, limit, deadline, { now = Date.now, sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms)) } = {}) {
  if (typeof tab.evaluate !== "function") throw new SearchEngineError(SEARCH_ERROR_CODES.INVALID_TAB);
  const mountedAt = now();
  for (;;) {
    const snapshot = await tab.evaluate(readSearchDocument, normalizeLimit(limit));
    if (snapshot.state === "captcha" || snapshot.state === "consent") requireSearchPage(snapshot);
    const expired = now() >= deadline;
    if (snapshot.state === "ready") {
      const status = snapshot.ai_overview.status;
      // Allow an asynchronously mounted overview to appear after page load.
      // Once present, only Google's explicit completion markers admit its text.
      if (status === "complete" || expired || (status !== "pending" && now() - mountedAt >= 1_000)) {
        const ai_overview = status === "pending" ? { status: "unavailable", reason: "generation_timeout" } : snapshot.ai_overview;
        if (!snapshot.results.length && ai_overview.status !== "complete" && status === "pending") throw new SearchEngineError(SEARCH_ERROR_CODES.TIMEOUT);
        return { results: snapshot.results, ai_overview, citations: [...new Set([
          ...(ai_overview.citations || []), ...snapshot.results.map((result) => result.url),
        ])] };
      }
    } else if (expired) requireSearchPage(snapshot);
    await sleep(Math.min(250, Math.max(1, deadline - now())));
  }
}

export async function searchGoogle({ tab, query, limit = DEFAULT_LIMIT, timeoutMs = DEFAULT_TIMEOUT_MS, now = Date.now, sleep }) {
  if (!tab || typeof tab.navigate !== "function") throw new SearchEngineError(SEARCH_ERROR_CODES.INVALID_TAB);
  if (typeof query !== "string" || !query.trim()) throw new TypeError("search_query_required");
  const timeout = normalizeTimeout(timeoutMs);
  const deadline = now() + timeout;
  await tab.navigate(googleSearchUrl(query));
  await waitForTab(tab, timeout);
  return { provider: "google", query, ...await collectSearchPage(tab, limit, deadline, { now, sleep }) };
}

export const extractGoogleResults = extractSearchResults;
export const runGoogleSearch = searchGoogle;
