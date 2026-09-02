export const SELECTORS = Object.freeze({
  googleSearch: "#search",
  googleResult: "h3",
  chatgptPrompt: "#prompt-textarea[contenteditable=true][role=textbox]",
  chatgptPlus: "composer-plus-btn",
  chatgptSend: "send-button",
  chatgptAssistant: '[data-message-author-role="assistant"]'
});
export function safeUrl(value) { try { const u = new URL(value); return u.protocol === "https:" ? u.href : null; } catch { return null; } }
export function isGoogleInternal(value) { try { const u=new URL(value); return /(^|\.)google\.[a-z.]+$/i.test(u.hostname); } catch { return true; } }
export function extractGoogle(documentLike, limit = 10) {
  return [...documentLike.querySelectorAll("#search h3")].map(h => { const a = h.closest("a"); const url = a && safeUrl(a.href); return url && !isGoogleInternal(url) ? {title:h.textContent.trim(), url, snippet:h.parentElement?.parentElement?.querySelector?.(".VwiC3b")?.textContent?.trim() || ""} : null; }).filter(Boolean).slice(0, limit);
}
export function extractChatGPT(documentLike, limit = 10) {
  return [...documentLike.querySelectorAll('a[href]')].map(a => { const url = safeUrl(a.href); return url ? {title:(a.textContent||"").trim(), url} : null; }).filter(Boolean).slice(0, limit);
}
