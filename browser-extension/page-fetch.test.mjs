import test from "node:test";
import assert from "node:assert/strict";
import { PageFetchError, extractReadableDocument, fetchPage } from "./engines/fetch.js";

function page({ url = "https://example.com/article", title = "Example", text = "Readable body" } = {}) {
  const body = { innerText: text };
  return {
    body,
    querySelector(selector) {
      if (selector === 'link[rel="canonical"]') return { href: url };
      if (selector === "title") return { textContent: title };
      return null;
    },
  };
}

test("page fetch navigates one owned tab and returns bounded readable context", async () => {
  const calls = [];
  const tab = {
    async goto(url) { calls.push(["goto", url]); },
    async evaluate(fn, fallbackUrl) { return fn(page(), fallbackUrl); },
  };
  const result = await fetchPage({ tab, url: "https://example.com/article" });
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

test("readable extraction reports a missing body instead of inventing text", () => {
  assert.throws(
    () => extractReadableDocument(page({ text: "" })),
    (error) => error instanceof PageFetchError && error.code === "page_text_unavailable",
  );
});
