export const SELECTORS = Object.freeze({
  googleSearch: "#search",
  googleResult: "h3",
  chatgptPrompt: "#prompt-textarea[contenteditable=true][role=textbox]",
  chatgptPlus: "composer-plus-btn",
  chatgptSend: "send-button",
  chatgptAssistant: '[data-message-author-role="assistant"]'
});
function localLiteral(hostname) {
  const host = String(hostname || "").replace(/^\[|\]$/g, "").toLowerCase();
  if (host === "localhost" || host === "localhost.localdomain" || host === "::1" || host === "0:0:0:0:0:0:0:1") return true;
  const octets = host.split(".").map(Number);
  if (octets.length !== 4 || octets.some((part) => !Number.isInteger(part) || part < 0 || part > 255)) return false;
  return octets[0] === 10 || octets[0] === 127 || (octets[0] === 172 && octets[1] >= 16 && octets[1] <= 31) || (octets[0] === 192 && octets[1] === 168) || (octets[0] === 169 && octets[1] === 254);
}
export function safeUrl(value) { try { const u = new URL(value); return u.protocol === "https:" && !u.username && !u.password && !localLiteral(u.hostname) ? u.href : null; } catch { return null; } }
export function isGoogleInternal(value) { try { const u=new URL(value); return /(^|\.)google\.[a-z.]+$/i.test(u.hostname); } catch { return true; } }
export function extractGoogle(documentLike, limit = 10) {
  return [...documentLike.querySelectorAll("#search h3")].map(h => { const a = h.closest("a"); const url = a && safeUrl(a.href); return url && !isGoogleInternal(url) ? {title:h.textContent.trim(), url, snippet:h.parentElement?.parentElement?.querySelector?.(".VwiC3b")?.textContent?.trim() || ""} : null; }).filter(Boolean).slice(0, limit);
}
export function extractChatGPT(documentLike, limit = 10) {
  return [...documentLike.querySelectorAll('a[href]')].map(a => { const url = safeUrl(a.href); return url ? {title:(a.textContent||"").trim(), url} : null; }).filter(Boolean).slice(0, limit);
}
