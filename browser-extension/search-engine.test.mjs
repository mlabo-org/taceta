import test from "node:test";
import assert from "node:assert/strict";
import vm from "node:vm";
import { CdpExecutor } from "./cdp.js";
import {
  SearchEngineError,
  readSearchDocument,
  detectSearchPageState,
  extractSearchResults,
  googleSearchUrl,
  normalizeLimit,
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
  const tab = { async navigate(url) { calls.push(["navigate", url]); }, async waitForLoad(ms) { calls.push(["wait", ms]); }, async evaluate(fn, limit) { return fn(limit, fakeDocument({ cards: [{ title: "Result", href: "https://example.com" }] })); } };
  const result = await searchGoogle({ tab, query: "Ollama latest release", limit: 3 });
  assert.equal(result.results[0].url, "https://example.com/");
  assert.equal(calls[0][1], googleSearchUrl("Ollama latest release"));
});


test("limits are bounded", () => { assert.equal(normalizeLimit(0), 1); assert.equal(normalizeLimit(999), 50); });

// Minimal independent markup model of the observed Google overview structure.
// In particular, a completed paragraph cannot stand in for the outer stream.
function overviewDocument({ complete = false, answerComplete = complete, text = "現在の米国大統領は", cards = [], links = ["https://example.org/source"] } = {}) {
  const doc = fakeDocument({ cards });
  const main = { innerText: text };
  const answer = {
    getAttribute: (name) => name === "data-complete" ? String(answerComplete) : null,
    querySelector: (selector) => selector === '[data-container-id="main-col"]' ? main : null,
    querySelectorAll: () => links.map((href) => ({ getAttribute: () => href })),
  };
  const envelope = { getAttribute: () => String(complete) };
  const region = { closest: () => envelope, contains: () => false,
    querySelector: (selector) => selector === '[data-subtree="aimc"]' ? answer : null,
  };
  const original = doc.querySelector;
  doc.querySelector = (selector) => selector === '#m-x-content' ? region : original(selector);
  return doc;
}

function clock() { let time = 0; return { now: () => time, sleep: async (ms) => { time += ms; } }; }
function sequenceTab(documents) {
  let index = 0;
  return { async navigate() {}, async waitForLoad() {}, async evaluate(fn, limit) {
    const doc = documents[Math.min(index++, documents.length - 1)];
    return fn(limit, doc);
  } };
}

test("completed overview preserves full text and source URLs alongside normal results", () => {
  const doc = overviewDocument({ complete: true, text: "冒頭の回答。\n折りたたまれた後半も全文。", cards: [{ title: "Official", href: "https://example.com", snippet: "Independent result" }], links: ["https://example.org/source", "https://example.org/source", "https://www.google.com/search?q=related", "javascript:alert(1)"] });
  const result = readSearchDocument(10, doc);
  assert.deepEqual(result.ai_overview, { status: "complete", text: "冒頭の回答。\n折りたたまれた後半も全文。", citations: ["https://example.org/source"] });
  assert.equal(result.results[0].snippet, "Independent result");
  assert.equal(readSearchDocument(10, fakeDocument()).ai_overview.status, "absent");
});

test("an unchanged partial overview and a completed paragraph never admit partial text", () => {
  for (const answerComplete of [false, true]) {
    const result = readSearchDocument(10, overviewDocument({ answerComplete }));
    assert.deepEqual(result.ai_overview, { status: "pending" });
    assert.equal(JSON.stringify(result).includes("現在の米国大統領は"), false);
  }
});

test("Google waits through delayed mount and paused generation for explicit completion", async () => {
  const result = await searchGoogle({ tab: sequenceTab([
    fakeDocument(), overviewDocument(), overviewDocument(), overviewDocument(),
    overviewDocument({ complete: true, text: "完成した回答", cards: [{ title: "Result", href: "https://example.com" }] }),
  ]), query: "質問", ...clock() });
  assert.equal(result.ai_overview.text, "完成した回答");
  assert.deepEqual(result.citations, ["https://example.org/source", "https://example.com/"]);
});

test("unfinished overview times out into normal results without leaking its partial text", async () => {
  const result = await searchGoogle({ tab: sequenceTab([overviewDocument({ cards: [{ title: "Result", href: "https://example.com" }] })]), query: "質問", timeoutMs: 500, ...clock() });
  assert.deepEqual(result.ai_overview, { status: "unavailable", reason: "generation_timeout" });
  assert.equal(result.results.length, 1);
  assert.equal(JSON.stringify(result).includes("現在の米国大統領は"), false);
  await assert.rejects(searchGoogle({ tab: sequenceTab([overviewDocument()]), query: "質問", timeoutMs: 250, ...clock() }), (e) => e.code === "search_timeout");
});

test("ordinary search and unknown overview layout keep available web results", async () => {
  const doc = fakeDocument({ cards: [{ title: "CAPTCHA documentation", href: "https://example.com" }] });
  doc.body.innerText = "CAPTCHA robot consent documentation";
  const original = doc.querySelectorAll;
  doc.querySelectorAll = (selector) => selector === '[role="heading"], h2' ? [{ innerText: "AI による概要", closest: () => null }] : original(selector);
  const result = await searchGoogle({ tab: sequenceTab([doc]), query: "質問", ...clock() });
  assert.deepEqual(result.ai_overview, { status: "unavailable" });
  assert.equal(result.results.length, 1);
});

test("the real browser executor serializes the same search reader without a substitute", async () => {
  const document = overviewDocument({ complete: true, text: "全文", cards: [{ title: "Result", href: "https://example.com" }] });
  const scope = { ledger: { windowId: 7, groupId: 9, tabId: 8 }, validate() {} };
  const api = { tabs: { async get(id) { return { id, windowId: 7, url: "https://www.google.com/search?q=test" }; } },
    scripting: { async executeScript({ func, args }) {
      assert.equal(func, readSearchDocument);
      return [{ result: vm.runInNewContext(`(${func.toString()})(...args)`, { document, args, URL }) }];
    } },
  };
  const result = await new CdpExecutor(api, scope).evaluate(readSearchDocument, [10]);
  assert.equal(result.ai_overview.text, "全文");
  assert.equal(result.results[0].url, "https://example.com/");
});
