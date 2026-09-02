import { OwnedScope } from "./scope.js";
import { CdpExecutor } from "./cdp.js";
import { envelope, jobResultPayload, validateEnvelope, validateJob } from "./protocol.js";
import { runDefaultSearch } from "./workflows.js";
import { fetchPage } from "./engines/fetch.js";
import { searchGoogle, searchDefault } from "./engines/search.js";
import { runChatGPTWeb } from "./engines/chatgpt-web.js";

const scope = new OwnedScope(chrome);
const cdp = new CdpExecutor(chrome, scope);
let port; let sessionId = crypto.randomUUID(); let polling = false; let connecting = false; let initializing = null; let retryTimer = null; let pollTimer = null;
const seenJobs = new Set();
const reply = m => port?.postMessage(m);
const request = (operation, payload={}) => envelope("request", crypto.randomUUID(), sessionId, operation, payload);
async function poll() {
  if (!port || polling) return; polling = true;
  try { reply(request("poll_job")); } finally { polling = false; }
}
async function execute(message) {
  validateEnvelope(message, "response");
  if (message.operation === "poll_job") {
    const job = message.payload.job; if (!job) { pollTimer = setTimeout(() => { pollTimer = null; poll(); }, 250); return; }
    if (seenJobs.has(job.job_id)) { pollTimer = setTimeout(() => { pollTimer = null; poll(); }, 250); return; }
    seenJobs.add(job.job_id);
    const jobPort = port;
    try {
      validateJob(job, {request_id: job.authorization.request_id, session_id: sessionId});
      await scope.open(); await cdp.attach(); const page=cdp.page();
      let result;
      if (job.workflow === "google_search") result = await searchGoogle({tab:page, query:job.query, limit:job.limit, timeoutMs:job.timeout_ms});
      else if (job.workflow === "default_search") result = await searchDefault({chrome, tab:page, tabId:scope.ledger.tabId, query:job.query, limit:job.limit, timeoutMs:job.timeout_ms});
      else if (job.workflow === "page_fetch") result = await fetchPage({tab:page, url:job.url, timeoutMs:job.timeout_ms});
      else result = await runChatGPTWeb({page, prompt:job.prompt, timeoutMs:job.timeout_ms, idleTimeoutMs:job.idle_timeout_ms});
      if (port === jobPort) reply(request("job_result", jobResultPayload(job, "completed", {results:result, answer:result.answer, citations:result.citations||[]})));
    } catch (error) { if (port === jobPort) reply(request("job_result", jobResultPayload(job, "failed", {error:{code:error.code||"workflow_failed",message:String(error.message||error)}}))); }
    finally { try { await cdp.detach(); } catch (_) {} try { await scope.close(); } catch (_) {} }
    if (port === jobPort) poll();
    return;
  }
  if (message.operation === "cancel_ack") return;
}
function connect() {
  if (port || connecting) return;
  connecting = true;
  try { port = chrome.runtime.connectNative("org.mlabo.taceta.link"); } catch (_) { connecting=false; scheduleReconnect(); return; }
  connecting = false;
  const p = port;
  chrome.alarms.clear?.("taceta-link-reconnect");
  p.onMessage.addListener(m => { try { execute(m).catch(() => {}); } catch (_) {} });
  port.onDisconnect.addListener(() => {
    const ignored=chrome.runtime.lastError; void ignored;
    if (port !== p) return;
    port=null;
    if (pollTimer !== null) { clearTimeout(pollTimer); pollTimer = null; }
    // A Native Messaging disconnect invalidates the current session. Release
    // the in-flight owned scope before reconnecting so a stale engine cannot
    // leak a window/group or report its result through the new port.
    sessionId = crypto.randomUUID();
    releaseOwnedScope().finally(scheduleReconnect).catch(() => {});
  });
  reply(request("extension_ready", {name:"Taceta Link", version:"0.1.0"}));
  poll();
}
async function releaseOwnedScope() { try { await cdp.detach(); } finally { await scope.close(); } }
async function initialize() {
  if (port || connecting) return;
  if (initializing) return initializing;
  initializing = (async()=>{ await scope.recoverAndClose(); connect(); })();
  try { await initializing; } finally { initializing = null; }
}
const initializeOrRetry = () => { initialize().catch(scheduleReconnect); };
function scheduleReconnect() {
  chrome.alarms.create("taceta-link-reconnect", {periodInMinutes:1});
  if (retryTimer===null) retryTimer=setTimeout(()=>{retryTimer=null; initializeOrRetry();},1000);
}
chrome.alarms.onAlarm.addListener(a=>{if(a.name==="taceta-link-reconnect") initializeOrRetry();});
chrome.runtime.onStartup.addListener(initializeOrRetry);
chrome.runtime.onInstalled.addListener(initializeOrRetry);
initializeOrRetry();
