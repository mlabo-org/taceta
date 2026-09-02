const PROMPT_SELECTOR = '#prompt-textarea[contenteditable="true"][role="textbox"]';
const SEND_SELECTOR = 'button[data-testid="send-button"]';
const ASSISTANT_SELECTOR = '[data-message-author-role="assistant"]';
const TURN_SELECTOR = 'section[data-testid^="conversation-turn-"]';
const DEFAULT_HARD_TIMEOUT_MS = 20 * 60 * 1000;
const DEFAULT_IDLE_TIMEOUT_MS = 3 * 60 * 1000;

const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

function typedError(code, message = code, details = {}) {
  const error = new Error(message);
  error.code = code;
  Object.assign(error, details);
  return error;
}

function snapshotDocument(documentLike) {
  const assistants = [...documentLike.querySelectorAll(ASSISTANT_SELECTOR)].map((node) => ({
    id: node.getAttribute("data-message-id") || "",
    text: (node.innerText || node.textContent || "").trim(),
  }));
  const prompt = documentLike.querySelector(PROMPT_SELECTOR);
  const send = documentLike.querySelector(SEND_SELECTOR);
  const profile = documentLike.querySelector('button[aria-label*="プロファイルメニュー"],button[aria-label*="Profile"]');
  const loginText = (documentLike.body?.innerText || "").match(/ログイン|Log in|Sign in/i);
  return {
    authenticated: Boolean(profile) || !loginText,
    composer: Boolean(prompt),
    send: Boolean(send),
    sendDisabled: Boolean(send?.disabled),
    assistants,
    stop: [...documentLike.querySelectorAll("button,[aria-label]")].some((node) => /停止|stop generating|stop/i.test(node.getAttribute("aria-label") || node.textContent || "")),
  };
}

function extractCitations(documentLike, assistant) {
  const links = assistant ? assistant.querySelectorAll("a[href]") : documentLike.querySelectorAll(`${ASSISTANT_SELECTOR} a[href]`);
  return [...links].map((link) => {
    try {
      const url = new URL(link.href || link.getAttribute("href"), documentLike.location?.href || "https://chatgpt.com/");
      if (url.protocol !== "https:") return null;
      return { title: (link.innerText || link.textContent || "").trim(), url: url.href };
    } catch { return null; }
  }).filter(Boolean);
}

export function chatGPTWebSelectors() {
  return Object.freeze({ prompt: PROMPT_SELECTOR, send: SEND_SELECTOR, assistant: ASSISTANT_SELECTOR, turn: TURN_SELECTOR });
}

export function readChatGPTWebState(documentLike) { return snapshotDocument(documentLike); }
export function citationsFromAssistant(documentLike, assistant) { return extractCitations(documentLike, assistant); }
export function chatGPTWebActivityFingerprint(state) {
  const latest = state?.assistants?.at?.(-1) || {};
  return JSON.stringify([
    latest.id || "",
    latest.text || "",
    Boolean(state?.composer),
    Boolean(state?.send),
    Boolean(state?.sendDisabled),
    Boolean(state?.stop),
  ]);
}

export async function runChatGPTWeb({ page, prompt, url = "https://chatgpt.com/", timeoutMs = DEFAULT_HARD_TIMEOUT_MS, idleTimeoutMs = DEFAULT_IDLE_TIMEOUT_MS, pollMs = 250, stabilityMs = 500 }) {
  if (!page || typeof page.evaluate !== "function") throw typedError("page_required");
  if (typeof prompt !== "string" || !prompt.trim()) throw typedError("prompt_required");
  const hardTimeout = Number.isFinite(timeoutMs) ? Math.max(1_000, Math.floor(timeoutMs)) : DEFAULT_HARD_TIMEOUT_MS;
  const idleTimeout = Number.isFinite(idleTimeoutMs) ? Math.max(1_000, Math.min(hardTimeout, Math.floor(idleTimeoutMs))) : DEFAULT_IDLE_TIMEOUT_MS;
  const hardDeadline = Date.now() + hardTimeout;
  let idleDeadline = Date.now() + idleTimeout;
  if (typeof page.goto === "function") await page.goto(url);
  const read = () => page.evaluate(snapshotDocument);
  let state = await read();
  let activity = chatGPTWebActivityFingerprint(state);
  if (!state.authenticated) throw typedError("auth_required");
  while (!state.composer && Date.now() < hardDeadline && Date.now() < idleDeadline) {
    await sleep(pollMs);
    state = await read();
    const nextActivity = chatGPTWebActivityFingerprint(state);
    if (nextActivity !== activity) { activity = nextActivity; idleDeadline = Date.now() + idleTimeout; }
  }
  if (!state.composer) throw typedError("composer_timeout");
  const before = new Set(state.assistants.map((item) => item.id).filter(Boolean));
  await page.evaluate((value) => {
    const node = document.querySelector('#prompt-textarea[contenteditable="true"][role="textbox"]');
    if (!node) throw new Error("composer_missing");
    node.focus();
    node.replaceChildren(document.createTextNode(value));
    node.dispatchEvent(new InputEvent("input", { bubbles: true, inputType: "insertText", data: value }));
  }, prompt);
  await page.evaluate(() => document.querySelector('button[data-testid="send-button"]')?.click());

  let candidate = null;
  let completed = false;
  let stableSince = 0;
  activity = chatGPTWebActivityFingerprint(state);
  idleDeadline = Date.now() + idleTimeout;
  while (Date.now() < hardDeadline && Date.now() < idleDeadline) {
    state = await read();
    const nextActivity = chatGPTWebActivityFingerprint(state);
    if (nextActivity !== activity || state.stop) {
      activity = nextActivity;
      idleDeadline = Date.now() + idleTimeout;
    }
    const next = [...state.assistants].reverse().find((item) => item.id && !before.has(item.id) && item.text);
    if (next && !state.stop && state.send) {
      if (!candidate || candidate.id !== next.id || candidate.text !== next.text) { candidate = next; stableSince = Date.now(); }
      if (Date.now() - stableSince >= stabilityMs) { completed = true; break; }
    }
    await sleep(pollMs);
  }
  if (!completed) {
    if (Date.now() >= hardDeadline) throw typedError("response_hard_timeout");
    throw typedError("response_stalled");
  }
  const result = await page.evaluate((id) => {
    const nodes = [...document.querySelectorAll('[data-message-author-role="assistant"]')];
    const assistant = nodes.find((node) => node.getAttribute("data-message-id") === id) || nodes.at(-1);
    return { answer: (assistant?.innerText || assistant?.textContent || "").trim(), citations: [...(assistant?.querySelectorAll("a[href]") || [])].map((link) => ({ title: (link.innerText || link.textContent || "").trim(), url: link.href })).filter((link) => /^https:\/\//i.test(link.url)) };
  }, candidate.id);
  return { status: "completed", answer: result.answer, citations: result.citations, exact: true, mutation_state: "performed" };
}

export { PROMPT_SELECTOR, SEND_SELECTOR, ASSISTANT_SELECTOR, TURN_SELECTOR, typedError };
