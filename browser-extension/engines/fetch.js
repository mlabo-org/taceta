import { safeUrl } from "../selectors.js";

const DEFAULT_TIMEOUT_MS = 30_000;

export class PageFetchError extends Error {
  constructor(code, message = code) {
    super(message);
    this.name = "PageFetchError";
    this.code = code;
  }
}

/**
 * Runs inside the fetched page, so it must not use anything from this module.
 * Returns a bounded, readable snapshot as text only: scripts, forms, cookies,
 * and page instructions remain in the browser. The reported URL is the one the
 * tab actually loaded, never a URL the page declares about itself.
 */
export function readPageDocument() {
  const textOf = (node) => String(node?.innerText || node?.textContent || "").replace(/\s+/g, " ").trim();
  // Check challenge surfaces, not words in the page's own text.
  const challenge = document.querySelector(
    '#captcha-form, form[action*="/sorry/"], iframe[src*="recaptcha"], iframe[src*="hcaptcha.com"], iframe[src*="challenges.cloudflare.com"]',
  );
  if (challenge) return { __error: "captcha_detected" };
  let url = null;
  try {
    const loaded = new URL(location.href);
    const host = loaded.hostname.replace(/^\[|\]$/g, "").toLowerCase();
    const octets = host.split(".").map(Number);
    const privateV4 = octets.length === 4 && octets.every((part) => Number.isInteger(part) && part >= 0 && part <= 255)
      && (octets[0] === 10 || octets[0] === 127 || (octets[0] === 172 && octets[1] >= 16 && octets[1] <= 31)
        || (octets[0] === 192 && octets[1] === 168) || (octets[0] === 169 && octets[1] === 254));
    const local = host === "localhost" || host === "localhost.localdomain" || host === "::1" || privateV4;
    if (loaded.protocol === "https:" && !loaded.username && !loaded.password && !local) url = loaded.href;
  } catch { url = null; }
  if (!url) return { __error: "page_url_unavailable" };
  const root = document.querySelector("article, main, [role='main']") || document.body;
  const text = textOf(root);
  if (!text) return { __error: "page_text_unavailable" };
  const maximum = 120000;
  return {
    url,
    title: String(document.title || "").trim(),
    text: text.slice(0, maximum),
    truncated: text.length > maximum,
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
  const snapshot = await tab.evaluate(readPageDocument);
  if (snapshot?.__error) throw new PageFetchError(snapshot.__error);
  return snapshot;
}
