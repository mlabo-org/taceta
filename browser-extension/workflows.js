import { SELECTORS, safeUrl } from "./selectors.js";

export const CDP = Object.freeze({
  enable: "DOM.enable", getDocument: "DOM.getDocument", query: "DOM.querySelector",
  click: "Input.dispatchMouseEvent", insertText: "Input.insertText", keypress: "Input.dispatchKeyEvent",
  navigate: "Page.navigate"
});

export function workflowSpec(workflow, input, limit = 10) {
  if (typeof input !== "string" || !input) throw new Error("workflow_input_required");
  if (!["google_search", "default_search", "page_fetch", "chatgpt_web"].includes(workflow)) throw new Error("workflow_not_allowed");
  return { workflow, input, limit: Math.max(1, Math.min(50, Number(limit) || 10)) };
}

export async function runDefaultSearch({ chrome, tabId, query, extract }) {
  if (!Number.isInteger(tabId)) throw new Error("owned_tab_required");
  // chrome.search has no returned tab id. CURRENT_TAB is intentional: the tab is
  // created and owned by OwnedScope before this call, so ownership is preserved.
  await chrome.search.query({ text: query, tabId });
  return extract ? extract() : { tabId, status: "search_dispatched" };
}

export function googleResultsFromNodes(nodes, limit = 10) {
  return nodes.map(n => { const url = safeUrl(n.url); return url ? {title:n.title || "", url, snippet:n.snippet || ""} : null; }).filter(Boolean).slice(0,limit);
}

export function chatgptPromptContract(input) {
  return { selector: SELECTORS.chatgptPrompt, text: input, exact: true };
}
