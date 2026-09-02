import { CDP } from "./workflows.js";

const NAVIGATION_TIMEOUT_MS = 120_000;

function pageScript(source) {
  if (source.includes("extractReadableDocument")) return function extractReadableDocument(fallbackUrl = "") {
    const textOf = (node) => String(node?.innerText || node?.textContent || "").replace(/\s+/g, " ").trim();
    const bodyText = textOf(document.body);
    if (/(captcha|unusual traffic|robot|ロボット|人間であること)/i.test(bodyText)) return { __error: "captcha_detected" };
    const toPublicHttps = (value) => { try { const url = new URL(value || fallbackUrl, location.href); return url.protocol === "https:" && !url.username && !url.password ? url.href : null; } catch { return null; } };
    const canonical = document.querySelector('link[rel="canonical"]')?.href;
    const url = toPublicHttps(canonical || fallbackUrl);
    if (!url) return { __error: "page_url_unavailable" };
    const root = document.querySelector("article, main, [role='main']") || document.body;
    const text = textOf(root);
    if (!text) return { __error: "page_text_unavailable" };
    const maximum = 120000;
    return { url, title: String(document.title || "").trim(), text: text.slice(0, maximum), truncated: text.length > maximum, citations: [url] };
  };
  if (source.includes("extractSearchResults")) return function extractGoogleResults(limit = 10) {
    const text = String(document.body?.innerText || document.body?.textContent || "");
    if (/(captcha|unusual traffic|robot|ロボット|人間であること)/i.test(text)) return { __error: "captcha_detected" };
    if (/(before you continue|consent|同意|プライバシーと利用規約)/i.test(text)) return { __error: "consent_required" };
    if (!document.querySelector("#search")) return { __error: "results_page_unavailable" };
    const external = (value) => { try { let url = new URL(value, location.href); if (url.protocol !== "https:") return null; if (/(^|\\.)google\\.[a-z.]+$/i.test(url.hostname)) { const target = url.searchParams.get("q") || url.searchParams.get("url") || url.searchParams.get("uddg"); if (!target) return null; url = new URL(target); if (url.protocol !== "https:" || /(^|\\.)google\\.[a-z.]+$/i.test(url.hostname)) return null; } return url.href; } catch { return null; } };
    const cards = [...document.querySelectorAll("#search .MjjYud")];
    const headings = cards.length ? cards.map((card) => card.querySelector("h3")) : [...document.querySelectorAll("#search h3")];
    return headings.map((heading) => { const anchor = heading?.closest?.("a"); const url = external(anchor?.getAttribute?.("href") || anchor?.href); const title = String(heading?.textContent || "").trim(); if (!title || !url) return null; const card = heading.closest?.(".MjjYud") || heading.parentElement; const snippet = String(card?.querySelector?.(".VwiC3b")?.textContent || "").replace(/\\s+/g, " ").trim(); return { title, url, snippet }; }).filter(Boolean).slice(0, Math.max(1, Math.min(50, Number(limit) || 10)));
  };
  if (source.includes("snapshotDocument")) return function readChatGPTState() { const prompt = document.querySelector('#prompt-textarea[contenteditable="true"][role="textbox"]'); const send = document.querySelector('button[data-testid="send-button"]'); return { authenticated: !/(ログイン|Log in|Sign in)/i.test(document.body?.innerText || ""), composer: !!prompt, send: !!send, sendDisabled: !!send?.disabled, assistants: [...document.querySelectorAll('[data-message-author-role="assistant"]')].map((node) => ({ id: node.getAttribute("data-message-id") || "", text: (node.innerText || node.textContent || "").trim() })), stop: [...document.querySelectorAll("button,[aria-label]")].some((node) => /停止|stop generating|stop/i.test(node.getAttribute("aria-label") || node.textContent || "")) }; };
  if (source.includes("node.replaceChildren")) return function fillComposer(value) { const node = document.querySelector('#prompt-textarea[contenteditable="true"][role="textbox"]'); if (!node) return { __error: "composer_missing" }; node.focus(); node.replaceChildren(document.createTextNode(value)); node.dispatchEvent(new InputEvent("input", { bubbles: true, inputType: "insertText", data: value })); return null; };
  if (source.includes("send-button") && source.includes("click")) return function clickSend() { document.querySelector('button[data-testid="send-button"]')?.click(); return null; };
  if (source.includes("data-message-author-role") && source.includes("citations")) return function readAssistant(id) { const nodes = [...document.querySelectorAll('[data-message-author-role="assistant"]')]; const node = nodes.find((item) => item.getAttribute("data-message-id") === id) || nodes.at(-1); return { answer: (node?.innerText || node?.textContent || "").trim(), citations: [...(node?.querySelectorAll("a[href]") || [])].map((a) => ({ title: (a.innerText || a.textContent || "").trim(), url: a.href })).filter((a) => /^https:\/\//i.test(a.url)) }; };
  return null;
}

export class CdpExecutor {
  constructor(chromeApi, scope) { this.chrome = chromeApi; this.scope = scope; this.attached = new Set(); this.navigationPending = false; }
  async attach() { const id = this.scope.ledger?.tabId; this.scope.validate(this.scope.ledger); await this.chrome.debugger.attach({ tabId: id }, "1.3"); await this.chrome.debugger.sendCommand({ tabId: id }, CDP.enable); this.attached.add(id); return id; }
  async send(operation, args = {}) { const id = this.scope.ledger?.tabId; this.scope.validate(this.scope.ledger); const allowed = { navigate: CDP.navigate, click: CDP.click, type: CDP.insertText, keypress: CDP.keypress, snapshot: CDP.getDocument }; const method = allowed[operation]; if (!method) throw new Error("cdp_operation_not_allowed"); const params = {}; if (operation === "navigate") { params.url = String(args.url || ""); this.navigationPending = true; } if (operation === "type") params.text = String(args.text || ""); if (operation === "keypress") { params.type = "keyDown"; params.key = String(args.key || ""); } if (operation === "click") { params.type = "mousePressed"; params.x = Number(args.x); params.y = Number(args.y); params.button = "left"; params.clickCount = 1; } if (operation === "snapshot") params.depth = 2; return this.chrome.debugger.sendCommand({ tabId: id }, method, params); }
  async detach() { const id = this.scope.ledger?.tabId; if (!id || !this.attached.has(id)) return; await this.chrome.debugger.detach({ tabId: id }); this.attached.delete(id); }
  page() { return { navigate: (url) => this.send("navigate", { url }), goto: async (url, timeoutMs) => { await this.send("navigate", { url }); await this.waitForLoad(timeoutMs); }, waitForLoad: (timeout) => this.waitForLoad(timeout), waitForLoadState: (options) => this.waitForLoad(options?.timeoutMs), evaluate: (fn, ...args) => this.evaluate(fn, args) }; }
  async waitForLoad(timeout = NAVIGATION_TIMEOUT_MS) { const id = this.scope.ledger?.tabId; this.scope.validate(this.scope.ledger); const pending = this.navigationPending; const isComplete = async () => (await this.chrome.tabs.get(id))?.status === "complete"; if (!pending && await isComplete()) return; const event = this.chrome.webNavigation?.onCompleted; if (event?.addListener) { await new Promise((resolve, reject) => { const timer = setTimeout(() => { event.removeListener(listener); reject(Object.assign(new Error("navigation_timeout"), { code: "search_timeout" })); }, timeout || NAVIGATION_TIMEOUT_MS); const listener = (details) => { if (details?.tabId !== id || details.frameId !== 0) return; clearTimeout(timer); event.removeListener(listener); resolve(); }; event.addListener(listener); }); this.navigationPending = false; return; } const deadline = Date.now() + (timeout || NAVIGATION_TIMEOUT_MS); while (Date.now() < deadline) { if (await isComplete()) { this.navigationPending = false; return; } await new Promise((resolve) => setTimeout(resolve, 50)); } throw Object.assign(new Error("navigation_timeout"), { code: "search_timeout" }); }
  async evaluate(fn, args = []) { if (typeof fn !== "function") throw new Error("page_evaluate_function_required"); const id = this.scope.ledger?.tabId; this.scope.validate(this.scope.ledger); const tab = await this.chrome.tabs.get(id); if (!tab || tab.id !== id || typeof tab.windowId !== "number" || tab.incognito === true || typeof tab.url !== "string") throw new Error("page_context_unavailable"); const func = pageScript(fn.toString()) || fn; let results; try { results = await this.chrome.scripting.executeScript({ target: { tabId: id }, func, args }); } catch (error) { throw Object.assign(new Error(String(error?.message || "page_evaluate_failed")), { code: error?.code || "page_evaluate_failed" }); } const value = results?.[0]?.result; if (value?.__error) throw Object.assign(new Error(value.__error), { code: value.__error }); return value; }
}
