import { OWNED_GROUP_COLOR, requireOwned } from "./safety.js";
const OWNED_SCOPE_KEY = "taceta_owned_scope";
function validLedger(ids) { return ids && Number.isInteger(ids.windowId) && Number.isInteger(ids.groupId) && Number.isInteger(ids.tabId); }
function missingExactId(error) { return /not[_ ]found|no (tab|group|window) with id/i.test(String(error?.message || error)); }
async function exactOrNull(read) { try { return await read(); } catch (error) { if (missingExactId(error)) return null; throw error; } }
export class OwnedScope {
  constructor(chromeApi) { this.chrome = chromeApi; this.ledger = null; this.refs = new Map(); this.closing = null; }
  async recover() {
    const stored = await this.chrome.storage.local.get(OWNED_SCOPE_KEY);
    const ids = stored?.[OWNED_SCOPE_KEY];
    if (!ids) return null;
    if (!validLedger(ids)) { await this.chrome.storage.local.remove(OWNED_SCOPE_KEY); return null; }
    const [group, tab] = await Promise.all([exactOrNull(() => this.chrome.tabGroups.get(ids.groupId)), exactOrNull(() => this.chrome.tabs.get(ids.tabId))]);
    if (!group && !tab) { await this.chrome.storage.local.remove(OWNED_SCOPE_KEY); return null; }
    if ((group && (group.id !== ids.groupId || typeof group.windowId !== "number")) || (tab && (tab.id !== ids.tabId || typeof tab.windowId !== "number" || tab.groupId < 0))) throw new Error("stored_owned_scope_changed");
    this.ledger = {windowId:ids.windowId, groupId:ids.groupId, tabId:ids.tabId};
    return this.ledger;
  }
  async recoverAndClose() { if (await this.recover()) await this.close(); }
  async open() {
    if (this.ledger) return this.ledger;
    const normalWindows = await this.chrome.windows.getAll({windowTypes:["normal"]});
    const list = Array.isArray(normalWindows) ? normalWindows : [];
    const existing = list.find(w => w?.focused && typeof w.id === "number") || list.find(w => typeof w?.id === "number");
    const w = existing || await this.chrome.windows.create({focused:false, type:"normal", url:"about:blank"});
    if (typeof w?.id !== "number") throw new Error("owned_scope_creation_failed");
    const returnedTab = Array.isArray(w.tabs) ? w.tabs.find(t => typeof t?.id === "number") : null;
    const createdTab = returnedTab || await this.chrome.tabs.create({active:false, windowId:w.id, url:"about:blank"});
    if (!createdTab || typeof createdTab.id !== "number") throw new Error("owned_scope_creation_failed");
    const tab = await this.chrome.tabs.get(createdTab.id);
    if (!tab || tab.id !== createdTab.id || typeof tab.windowId !== "number") throw new Error("owned_scope_creation_failed");
    let groupId;
    try {
      groupId = await this.chrome.tabs.group({tabIds:[tab.id], createProperties:{windowId:tab.windowId}});
      if (typeof groupId !== "number") throw new Error("owned_group_creation_failed");
      this.ledger = {windowId:tab.windowId, groupId, tabId:tab.id};
      try { await this.chrome.tabGroups.update(groupId, {title:"Taceta", color:OWNED_GROUP_COLOR}); } catch (_) {}
      await this.chrome.storage.local.set({[OWNED_SCOPE_KEY]:this.ledger});
      return this.ledger;
    } catch (error) {
      try { await this.chrome.tabs.remove(tab.id); } catch (_) {}
      this.ledger = null; this.refs.clear();
      throw error;
    }
  }
  validate(ids) { requireOwned(this.ledger, ids); return true; }
  remember(ref, selector) { if (typeof ref !== "string" || !selector) throw new Error("invalid_element_ref"); this.refs.set(ref, selector); }
  resolve(ref) { const s=this.refs.get(ref); if (!s) throw new Error("element_ref_expired"); return s; }
  async close() {
    if (this.closing) return this.closing;
    if (!this.ledger) return;
    const ids = {...this.ledger};
    requireOwned(this.ledger, ids);
    const closing = (async () => {
      const tab = await exactOrNull(() => this.chrome.tabs.get(ids.tabId));
      if (tab && (tab.id !== ids.tabId || typeof tab.windowId !== "number")) throw new Error("stored_owned_scope_changed");
      if (tab && tab.groupId === ids.groupId) { try { await this.chrome.tabs.ungroup([ids.tabId]); } catch (_) {} }
      if (tab) await this.chrome.tabs.remove(ids.tabId);
      await this.chrome.storage.local.remove(OWNED_SCOPE_KEY);
      if (this.ledger?.windowId === ids.windowId && this.ledger?.groupId === ids.groupId && this.ledger?.tabId === ids.tabId) this.ledger = null;
      this.refs.clear();
    })();
    this.closing = closing;
    try { return await closing; } finally { if (this.closing === closing) this.closing = null; }
  }
}
