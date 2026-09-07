import test from "node:test";
import assert from "node:assert/strict";
import { chatGPTWebSelectors, readChatGPTWebState, readChatGPTDocument, runChatGPTWeb, citationsFromAssistant, chatGPTWebActivityFingerprint, chatGPTWebProgressChunk } from "./engines/chatgpt-web.js";
import { CdpExecutor } from "./cdp.js";
import vm from "node:vm";

function responseDocument({ text = "現在の米国大統領は", complete = false, stop = false } = {}) {
  const control = (testid) => ({ disabled: false, getAttribute: (name) => name === "data-testid" ? testid : null });
  const copy = control("copy-turn-action-button");
  const turn = {
    querySelector: () => complete ? copy : null,
    querySelectorAll: () => [],
  };
  const assistant = {
    innerText: text,
    getAttribute: (name) => name === "data-message-id" ? "new-answer" : null,
    closest: () => turn,
    querySelectorAll: () => [],
  };
  return {
    body: { innerText: "" },
    querySelector: () => null,
    querySelectorAll: (selector) => selector.includes("data-message-author-role") ? [assistant] : stop ? [control("stop-button")] : [],
  };
}

test("a paused partial answer is not complete without its own answer actions", () => {
  const doc = responseDocument();
  assert.equal(readChatGPTWebState(doc).assistants[0].completed, false);
  assert.equal(readChatGPTDocument("new-answer", doc).__error, "assistant_response_not_complete");
  assert.equal(readChatGPTWebState(responseDocument({ complete: true, stop: true })).assistants[0].completed, false);
  assert.equal(readChatGPTWebState(responseDocument({ complete: true })).assistants[0].completed, true);
  assert.equal(readChatGPTDocument("missing-answer", responseDocument({ complete: true })).__error, "assistant_response_not_complete");
});

test("the real scripting route executes the same self-contained completion reader", async () => {
  const scope = { ledger: { windowId: 7, groupId: 9, tabId: 8 }, validate() {} };
  let documentLike = responseDocument();
  const api = {
    tabs: { async get(id) { return { id, windowId: 7, url: "https://chatgpt.com/" }; } },
    scripting: { async executeScript({ func, args }) {
      assert.equal(func, readChatGPTDocument);
      // Like executeScript, serialize the function without its module closure.
      return [{ result: vm.runInNewContext(`(${func.toString()})(...args)`, { document: documentLike, args, URL }) }];
    } },
  };
  const executor = new CdpExecutor(api, scope);
  assert.equal((await executor.evaluate(readChatGPTDocument)).assistants[0].completed, false);
  documentLike = responseDocument({ text: "現在の米国大統領はドナルド・J・トランプです。", complete: true });
  const result = await executor.evaluate(readChatGPTDocument, ["new-answer"]);
  assert.equal(result.answer, documentLike.querySelectorAll('[data-message-author-role="assistant"]')[0].innerText);
});

test("generation pauses cannot return a partial answer even with zero stability delay", async () => {
  const base = { authenticated: true, composer: true, stop: false, assistants: [] };
  const partial = { id: "new-answer", text: "現在の米国大統領は\nUSAGov\n+1", completed: false };
  const fullText = "現在の米国大統領はドナルド・J・トランプです。";
  const states = [
    base,
    ...Array.from({ length: 4 }, () => ({ ...base, assistants: [partial] })),
    { ...base, stop: true, assistants: [{ ...partial, text: fullText }] },
    { ...base, assistants: [{ ...partial, text: fullText, completed: true }] },
  ];
  const progress = [];
  let finalReads = 0;
  const page = {
    async goto() {},
    async evaluate(fn, id) {
      if (fn !== readChatGPTDocument) return;
      if (id) {
        finalReads++;
        assert.equal(states.length, 0);
        assert.equal(id, "new-answer");
        return { answer: fullText, citations: [] };
      }
      assert.ok(states.length > 0);
      return states.shift();
    },
  };
  const result = await runChatGPTWeb({ page, prompt: "現在の米国大統領は誰だ？", pollMs: 1, stabilityMs: 0, onProgress: (chunk) => progress.push(chunk) });
  assert.equal(result.answer, fullText);
  assert.equal(result.status, "completed");
  assert.equal(finalReads, 1);
  assert.ok(progress.some((chunk) => chunk.replace && chunk.delta === fullText));
});

test("ChatGPT Web engine exposes current stable selectors", () => {
  assert.deepEqual(chatGPTWebSelectors(), {
    prompt: '#prompt-textarea[contenteditable="true"][role="textbox"]',
    send: 'button[data-testid="send-button"]',
    assistant: '[data-message-author-role="assistant"]',
    turn: 'section[data-testid^="conversation-turn-"]',
  });
});

test("state identifies authenticated composer and completion controls", () => {
  const node = (attrs = {}, text = "") => ({ getAttribute: (name) => attrs[name] ?? null, textContent: text, innerText: text, disabled: Boolean(attrs.disabled), querySelectorAll: () => [] });
  const documentLike = {
    body: { innerText: "Makoto Suzuki" },
    querySelector: (selector) => selector.startsWith("#prompt") ? node() : selector.startsWith("button[data-testid") ? node() : selector.includes("プロファイル") ? node() : null,
    querySelectorAll: () => [],
  };
  const state = readChatGPTWebState(documentLike);
  assert.equal(state.authenticated, true);
  assert.equal(state.composer, true);
  assert.equal(state.send, true);
  assert.equal(state.stop, false);
});

test("citation extraction keeps HTTPS links and rejects unsafe links", () => {
  const links = [
    { href: "https://example.com/a", innerText: "Example", textContent: "Example" },
    { href: "javascript:alert(1)", innerText: "bad", textContent: "bad" },
  ];
  const assistant = { querySelectorAll: () => links };
  assert.deepEqual(citationsFromAssistant({ location: { href: "https://chatgpt.com/" } }, assistant), [{ title: "Example", url: "https://example.com/a" }]);
});

test("citation extraction includes only links in the assistant's conversation turn", () => {
  const link = (href, title) => ({ href, innerText: title, textContent: title });
  const answerLink = link("https://example.com/answer", "Answer source");
  const turnSource = link("https://example.com/turn", "Turn source");
  const duplicate = link("https://example.com/answer", "Duplicate");
  const otherTurn = link("https://example.com/other", "Other turn");
  const pageLink = link("https://example.com/page", "Page navigation");
  const turn = { querySelectorAll: (selector) => selector === "a[href]" ? [answerLink, turnSource, duplicate] : [], closest: () => null };
  const assistant = {
    querySelectorAll: (selector) => selector === "a[href]" ? [answerLink] : [],
    closest: (selector) => selector === 'section[data-testid^="conversation-turn-"]' ? turn : null,
  };
  const other = { querySelectorAll: () => [otherTurn] };
  const documentLike = {
    location: { href: "https://chatgpt.com/" },
    querySelectorAll: (selector) => selector === "a[href]" ? [pageLink] : [other],
  };
  assert.deepEqual(citationsFromAssistant(documentLike, assistant), [
    { title: "Answer source", url: "https://example.com/answer" },
    { title: "Turn source", url: "https://example.com/turn" },
  ]);
});

test("citation extraction rejects credential-bearing HTTPS URLs and deduplicates canonical URLs", () => {
  const links = [
    { href: "https://user:pass@example.com/private", innerText: "private", textContent: "private" },
    { href: "https://example.com/a#one", innerText: "one", textContent: "one" },
    { href: "https://example.com/a#one", innerText: "duplicate", textContent: "duplicate" },
  ];
  const assistant = { querySelectorAll: () => links };
  assert.deepEqual(citationsFromAssistant({ location: { href: "https://chatgpt.com/" } }, assistant), [
    { title: "one", url: "https://example.com/a#one" },
  ]);
});

test("activity fingerprint changes while ChatGPT generation progresses", () => {
  const base = { composer: true, send: false, sendDisabled: true, stop: true, assistants: [{ id: "a1", text: "検索中" }] };
  assert.equal(chatGPTWebActivityFingerprint(base), chatGPTWebActivityFingerprint({ ...base }));
  assert.notEqual(chatGPTWebActivityFingerprint(base), chatGPTWebActivityFingerprint({ ...base, assistants: [{ id: "a1", text: "検索結果を確認中" }] }));
  assert.notEqual(chatGPTWebActivityFingerprint(base), chatGPTWebActivityFingerprint({ ...base, stop: false, send: true, sendDisabled: false }));
});

test("progress chunks stream append-only text and replace rewritten DOM text", () => {
  assert.deepEqual(chatGPTWebProgressChunk("", "回答", 1), {sequence: 1, delta: "回答", replace: false});
  assert.deepEqual(chatGPTWebProgressChunk("回答", "回答です", 2), {sequence: 2, delta: "です", replace: false});
  assert.deepEqual(chatGPTWebProgressChunk("回答です", "修正版です", 3), {sequence: 3, delta: "修正版です", replace: true});
  assert.equal(chatGPTWebProgressChunk("回答", "回答", 4), null);
});
