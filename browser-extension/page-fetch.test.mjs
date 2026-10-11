import test from "node:test";
import assert from "node:assert/strict";
import vm from "node:vm";
import { PageFetchError, fetchPage, readPageDocument } from "./engines/fetch.js";
import { CdpExecutor } from "./cdp.js";

function page({ href = "https://example.com/article", title = "Example", text = "Readable body", canonical, challenge = false } = {}) {
  const document = {
    title,
    body: { innerText: text },
    querySelector(selector) {
      if (selector.includes("captcha")) return challenge ? {} : null;
      if (selector === 'link[rel="canonical"]') return canonical ? { href: canonical } : null;
      return null;
    },
  };
  return { document, location: { href }, URL };
}

// Executes the reader exactly as the browser does: from its source text, with
// only page globals in scope.
function readIn(context) {
  // Results cross the page boundary as structured data, as they do in Chrome.
  return JSON.parse(JSON.stringify(vm.runInNewContext(`(${readPageDocument.toString()})()`, context)));
}

function tabFor(context, calls = []) {
  return {
    async goto(url) { calls.push(["goto", url]); },
    async evaluate(fn, ...args) {
      assert.equal(fn, readPageDocument);
      assert.deepEqual(args, []);
      return readIn(context);
    },
  };
}

test("page fetch navigates one owned tab and returns bounded readable context", async () => {
  const calls = [];
  const result = await fetchPage({ tab: tabFor(page(), calls), url: "https://example.com/article" });
  assert.deepEqual(calls, [["goto", "https://example.com/article"]]);
  assert.equal(result.title, "Example");
  assert.equal(result.text, "Readable body");
  assert.deepEqual(result.citations, ["https://example.com/article"]);
});

test("page fetch rejects non-HTTPS targets before navigation", async () => {
  await assert.rejects(
    () => fetchPage({ tab: { goto: async () => {} }, url: "http://127.0.0.1/" }),
    (error) => error instanceof PageFetchError && error.code === "public_https_url_required",
  );
});

test("the page is cited by the URL the tab loaded, not by one it declares", () => {
  const result = readIn(page({ href: "https://a.example/post", canonical: "https://reputable.example/original" }));
  assert.equal(result.url, "https://a.example/post");
  assert.deepEqual(result.citations, ["https://a.example/post"]);
});

test("a page that only mentions robots or CAPTCHAs is still readable", () => {
  const result = readIn(page({ text: "A robot solved the CAPTCHA despite unusual traffic. ロボット工学入門" }));
  assert.equal(result.text, "A robot solved the CAPTCHA despite unusual traffic. ロボット工学入門");
  assert.deepEqual(readIn(page({ challenge: true })), { __error: "captcha_detected" });
});

test("a missing body, a local address, or a redirect to plain HTTP fails instead of inventing a result", async () => {
  await assert.rejects(
    () => fetchPage({ tab: tabFor(page({ text: "" })), url: "https://example.com/article" }),
    (error) => error instanceof PageFetchError && error.code === "page_text_unavailable",
  );
  for (const href of ["http://example.com/article", "https://192.168.0.10/admin", "https://localhost/"]) {
    assert.deepEqual(readIn(page({ href })), { __error: "page_url_unavailable" });
  }
});

test("the browser executor injects the page reader itself, not a substitute", async () => {
  const context = page();
  const scope = { ledger: { windowId: 7, groupId: 9, tabId: 8 }, validate() {} };
  const api = {
    tabs: { async get(id) { return { id, windowId: 7, url: context.location.href }; } },
    scripting: { async executeScript({ func, args }) {
      assert.equal(func, readPageDocument);
      return [{ result: vm.runInNewContext(`(${func.toString()})(...args)`, { ...context, args }) }];
    } },
  };
  const result = await new CdpExecutor(api, scope).evaluate(readPageDocument, []);
  assert.equal(result.text, "Readable body");
});
