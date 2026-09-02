import { safeUrl } from "../selectors.js";

const DEFAULT_TIMEOUT_MS = 30_000;
const MAX_TEXT_CHARS = 120_000;

export class PageFetchError extends Error {
  constructor(code, message = code) {
    super(message);
    this.name = "PageFetchError";
    this.code = code;
  }
}

function pageText(node) {
  return String(node?.innerText || node?.textContent || "")
    .replace(/\s+/g, " ")
    .trim();
}

function publicUrl(value, fallback = "") {
  const candidate = safeUrl(value || fallback);
  return candidate || null;
}

/**
 * Extracts a bounded, readable page snapshot. It deliberately returns text
 * only: scripts, forms, cookies, and page instructions remain in the browser.
 */
export function extractReadableDocument(documentLike, fallbackUrl = "") {
  const bodyText = pageText(documentLike?.body);
  if (/(captcha|unusual traffic|robot|ロボット|人間であること)/i.test(bodyText)) {
    throw new PageFetchError("captcha_detected");
  }
  const canonical = documentLike?.querySelector?.('link[rel="canonical"]')?.href;
  const url = publicUrl(canonical, fallbackUrl);
  if (!url) throw new PageFetchError("page_url_unavailable");

  const root = documentLike?.querySelector?.("article, main, [role='main']") || documentLike?.body;
  const text = pageText(root);
  if (!text) throw new PageFetchError("page_text_unavailable");
  const truncated = text.length > MAX_TEXT_CHARS;
  return {
    url,
    title: pageText(documentLike?.querySelector?.("title")) || "",
    text: text.slice(0, MAX_TEXT_CHARS),
    truncated,
    citations: [url],
  };
}

async function waitForTab(tab, timeoutMs) {
  if (typeof tab.waitForLoad === "function") return tab.waitForLoad(timeoutMs);
  if (typeof tab.waitForLoadState === "function") {
    return tab.waitForLoadState({ state: "load", timeoutMs });
  }
  return undefined;
}

export async function fetchPage({ tab, url, timeoutMs = DEFAULT_TIMEOUT_MS }) {
  const target = safeUrl(url);
  if (!target) throw new PageFetchError("public_https_url_required");
  if (!tab || (typeof tab.goto !== "function" && typeof tab.navigate !== "function")) {
    throw new PageFetchError("owned_tab_required");
  }
  if (typeof tab.goto === "function") {
    await tab.goto(target, timeoutMs);
  } else {
    await tab.navigate(target);
    await waitForTab(tab, timeoutMs);
  }
  if (typeof tab.evaluate !== "function") throw new PageFetchError("owned_tab_required");
  return tab.evaluate(
    (documentLike, fallbackUrl) => extractReadableDocument(documentLike, fallbackUrl),
    target,
  );
}
