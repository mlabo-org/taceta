import test from "node:test";
import assert from "node:assert/strict";
import {
  SearchEngineError,
  detectSearchPageState,
  extractSearchResults,
  googleSearchUrl,
  normalizeLimit,
  searchDefault,
  searchGoogle,
} from "./engines/search.js";

function fakeDocument({ state = "ready", cards = [] } = {}) {
  const body = { innerText: state === "captcha" ? "Unusual traffic" : state === "consent" ? "Before you continue" : "results" };
  const cardNodes = cards.map(({ title, href, snippet = "" }) => ({
    querySelector(selector) {
      if (selector === "h3") return { textContent: title, closest: () => ({ href, getAttribute: () => href }) };
      if (selector === ".VwiC3b") return { textContent: snippet };
      return null;
    },
  }));
  return { body, querySelector(selector) { return selector === "#search" ? (state === "ready" ? {} : null) : null; }, querySelectorAll(selector) { return selector === "#search .MjjYud" ? cardNodes : []; } };
}

test("Google DOM extraction returns title/url/snippet and excludes internal or unsafe links", () => {
  const doc = fakeDocument({ cards: [
    { title: "Ollama releases", href: "https://github.com/ollama/ollama/releases", snippet: "release notes" },
    { title: "Google internal", href: "https://www.google.com/search?q=x" },
    { title: "Unsafe", href: "javascript:alert(1)" },
  ] });
  assert.deepEqual(extractSearchResults(doc), [{ title: "Ollama releases", url: "https://github.com/ollama/ollama/releases", snippet: "release notes" }]);
});

test("CAPTCHA and consent are typed failures", () => {
  assert.equal(detectSearchPageState(fakeDocument({ state: "captcha" })), "captcha");
  assert.equal(detectSearchPageState(fakeDocument({ state: "consent" })), "consent");
  assert.throws(() => extractSearchResults(fakeDocument({ state: "captcha" })), (error) => error instanceof SearchEngineError && error.code === "captcha_detected");
});

test("Google search navigates the supplied owned tab and extracts after load", async () => {
  const calls = [];
  const tab = { async navigate(url) { calls.push(["navigate", url]); }, async waitForLoad(ms) { calls.push(["wait", ms]); }, async evaluate(fn) { return fn(fakeDocument({ cards: [{ title: "Result", href: "https://example.com" }] })); } };
  const result = await searchGoogle({ tab, query: "Ollama latest release", limit: 3 });
  assert.equal(result.results[0].url, "https://example.com/");
  assert.equal(calls[0][1], googleSearchUrl("Ollama latest release"));
});

test("default search dispatches chrome.search to the same owned tab", async () => {
  let request;
  const tab = { async waitForLoad() {}, async evaluate(fn) { return fn(fakeDocument({ cards: [{ title: "Result", href: "https://example.com" }] })); } };
  const result = await searchDefault({ chrome: { search: { query: async (value) => { request = value; } } }, tab, tabId: 7, query: "Ollama" });
  assert.deepEqual(request, { text: "Ollama", tabId: 7 });
  assert.equal(result.provider, "default");
});

test("limits are bounded", () => { assert.equal(normalizeLimit(0), 1); assert.equal(normalizeLimit(999), 50); });
