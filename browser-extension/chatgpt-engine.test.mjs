import test from "node:test";
import assert from "node:assert/strict";
import { chatGPTWebSelectors, readChatGPTWebState, citationsFromAssistant, chatGPTWebActivityFingerprint } from "./engines/chatgpt-web.js";

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

test("activity fingerprint changes while ChatGPT generation progresses", () => {
  const base = { composer: true, send: false, sendDisabled: true, stop: true, assistants: [{ id: "a1", text: "検索中" }] };
  assert.equal(chatGPTWebActivityFingerprint(base), chatGPTWebActivityFingerprint({ ...base }));
  assert.notEqual(chatGPTWebActivityFingerprint(base), chatGPTWebActivityFingerprint({ ...base, assistants: [{ id: "a1", text: "検索結果を確認中" }] }));
  assert.notEqual(chatGPTWebActivityFingerprint(base), chatGPTWebActivityFingerprint({ ...base, stop: false, send: true, sendDisabled: false }));
});
