export const OWNED_GROUP_COLOR = "blue";

export function ownedScope(ledger, ids) {
  return Boolean(ledger && ids && ledger.windowId === ids.windowId && ledger.groupId === ids.groupId && ledger.tabId === ids.tabId);
}

export function requireOwned(ledger, ids) {
  if (!ownedScope(ledger, ids)) throw new Error("owned_scope_required");
}
