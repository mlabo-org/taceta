const DEFAULT_LIMIT = 10;
const MAX_LIMIT = 50;
const DEFAULT_TIMEOUT_MS = 15_000;

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

function visibleText(documentLike) {
  return String(documentLike?.body?.innerText || documentLike?.body?.textContent || "").toLowerCase();
}

export function detectSearchPageState(documentLike) {
  const text = visibleText(documentLike);
  if (/(captcha|unusual traffic|robot|ロボット|人間であること)/i.test(text)) return "captcha";
  if (/(before you continue|consent|同意|プライバシーと利用規約)/i.test(text)) return "consent";
  if (!documentLike?.querySelector?.("#search")) return "unavailable";
  return "ready";
}

function externalHttpsUrl(value, base = "https://www.google.com/") {
  let url;
  try { url = new URL(value, base); } catch { return null; }
  if (url.protocol !== "https:") return null;
  if (/(^|\.)google\.[a-z.]+$/i.test(url.hostname)) {
    const candidate = url.searchParams.get("q") || url.searchParams.get("url") || url.searchParams.get("uddg");
    if (!candidate) return null;
    try { url = new URL(candidate); } catch { return null; }
    if (url.protocol !== "https:" || /(^|\.)google\.[a-z.]+$/i.test(url.hostname)) return null;
  }
  return url.href;
}

function resultFromCard(card) {
  const heading = card.querySelector?.("h3");
  const anchor = heading?.closest?.("a");
  const url = externalHttpsUrl(anchor?.getAttribute?.("href") || anchor?.href);
  const title = String(heading?.textContent || "").trim();
  if (!title || !url) return null;
  const snippet = String(card.querySelector?.(".VwiC3b")?.textContent || "").replace(/\s+/g, " ").trim();
  return { title, url, snippet };
}

export function extractSearchResults(documentLike, limit = DEFAULT_LIMIT) {
  const state = detectSearchPageState(documentLike);
  if (state === "captcha") throw new SearchEngineError(SEARCH_ERROR_CODES.CAPTCHA);
  if (state === "consent") throw new SearchEngineError(SEARCH_ERROR_CODES.CONSENT);
  if (state !== "ready") throw new SearchEngineError(SEARCH_ERROR_CODES.NO_RESULTS_PAGE);
  const cards = [...documentLike.querySelectorAll("#search .MjjYud")];
  const primary = cards.map(resultFromCard).filter(Boolean);
  // Google occasionally omits .MjjYud while retaining the result heading.
  const fallback = primary.length ? primary : [...documentLike.querySelectorAll("#search h3")]
    .map((heading) => resultFromCard(heading.closest?.("div") || heading.parentElement || heading))
    .filter(Boolean);
  return fallback.slice(0, normalizeLimit(limit));
}

export function googleSearchUrl(query) {
  return `https://www.google.com/search?q=${encodeURIComponent(String(query))}`;
}

async function waitForTab(tab, timeoutMs) {
  if (typeof tab.waitForLoad === "function") return tab.waitForLoad(timeoutMs);
  if (typeof tab.waitForLoadState === "function") return tab.waitForLoadState({ state: "load", timeoutMs });
  return undefined;
}

async function extractFromTab(tab, limit) {
  if (typeof tab.evaluate !== "function") throw new SearchEngineError(SEARCH_ERROR_CODES.INVALID_TAB);
  return tab.evaluate((documentLike) => extractSearchResults(documentLike, limit));
}

export async function searchGoogle({ tab, query, limit = DEFAULT_LIMIT, timeoutMs = DEFAULT_TIMEOUT_MS }) {
  if (!tab || typeof tab.navigate !== "function") throw new SearchEngineError(SEARCH_ERROR_CODES.INVALID_TAB);
  if (typeof query !== "string" || !query.trim()) throw new TypeError("search_query_required");
  const timeout = normalizeTimeout(timeoutMs);
  await tab.navigate(googleSearchUrl(query));
  await waitForTab(tab, timeout);
  const results = await extractFromTab(tab, limit);
  return { provider: "google", query, results };
}

export async function searchDefault({ chrome, tab, tabId, query, limit = DEFAULT_LIMIT, timeoutMs = DEFAULT_TIMEOUT_MS }) {
  if (!chrome?.search?.query || !Number.isInteger(tabId)) throw new SearchEngineError(SEARCH_ERROR_CODES.INVALID_TAB);
  if (typeof query !== "string" || !query.trim()) throw new TypeError("search_query_required");
  const timeout = normalizeTimeout(timeoutMs);
  await chrome.search.query({ text: query, tabId });
  await waitForTab(tab, timeout);
  const results = await extractFromTab(tab, limit);
  return { provider: "default", query, results };
}

export const extractGoogleResults = extractSearchResults;
export const runGoogleSearch = searchGoogle;
export const runDefaultSearch = searchDefault;
