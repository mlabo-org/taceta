export const OWNED_GROUP_COLOR = "blue";
export const MUTATION_STATES = new Set(["not_performed", "pending", "performed", "performed_or_unknown"]);
export function ownedScope(ledger, ids) { return Boolean(ledger && ids && ledger.windowId === ids.windowId && ledger.groupId === ids.groupId && ledger.tabId === ids.tabId); }
export function requireOwned(ledger, ids) { if (!ownedScope(ledger, ids)) throw new Error("owned_scope_required"); }
export function effectFor(operation) { return ["job_progress", "job_result"].includes(operation) ? "external_submit" : "read_only"; }
export function requiresConfirmation(effect, authorization) { return ["external_submit","account_action","destructive_action"].includes(effect) && authorization !== true; }
export function isAuthorizedWebWorkflow(message) {
  const p=message?.payload; const a=p?.authorization;
  return ["default_search","google_search","page_fetch","chatgpt_web"].includes(p?.workflow) &&
    a?.kind === "web_request" && a.request_id === message.request_id && a.session_id === message.session_id && a.once === true;
}
export function mutationTransition(state, dispatchStarted, dispatchKnown) { if (!dispatchStarted) return "not_performed"; if (!dispatchKnown) return "performed_or_unknown"; return state === "pending" ? "performed" : state; }
