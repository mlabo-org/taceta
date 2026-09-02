import test from "node:test";
import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { envelope, jobProgressPayload, jobResultPayload, validateEnvelope, validateJob } from "./protocol.js";
import { effectFor, requiresConfirmation, mutationTransition, isAuthorizedWebWorkflow } from "./safety.js";
import { googleResultsFromNodes, workflowSpec, chatgptPromptContract, runDefaultSearch } from "./workflows.js";
import { safeUrl } from "./selectors.js";
import { OwnedScope } from "./scope.js";
import { CdpExecutor } from "./cdp.js";

test("protocol is one versioned envelope and rejects old raw browser wire",()=>{
  const s="00000000-0000-4000-8000-000000000002";
  const m=envelope("request","00000000-0000-4000-8000-000000000001",s,"poll_job",{});
  assert.equal(validateEnvelope(m,"request").operation,"poll_job");
  assert.throws(()=>validateEnvelope({...m,legacy_field:"old"},"request"));
  assert.throws(()=>envelope("request","r1",s,"unknown_operation",{}));
});
test("Rust poll response fixture is accepted by the browser protocol",async()=>{
  const fixture=JSON.parse(await readFile(new URL("../protocol/response.json",import.meta.url),"utf8"));
  assert.equal(validateEnvelope(fixture,"response").payload.job,null);
});
test("job results preserve the owning workflow on success and failure",()=>{
  const job={job_id:"j1",workflow:"google_search"};
  assert.deepEqual(jobResultPayload(job,"completed",{results:[]}),{results:[],job_id:"j1",workflow:"google_search",status:"completed",mutation_state:"performed"});
  assert.deepEqual(jobResultPayload(job,"failed",{error:{code:"search_timeout"}}),{error:{code:"search_timeout"},job_id:"j1",workflow:"google_search",status:"failed",mutation_state:"not_performed"});
});
test("ChatGPT progress is ordered incremental output",()=>{
  const job={job_id:"j2",workflow:"chatgpt_web"};
  assert.deepEqual(jobProgressPayload(job,2,"続き",false),{job_id:"j2",workflow:"chatgpt_web",sequence:2,delta:"続き",replace:false,status:"streaming",mutation_state:"performed"});
  assert.throws(()=>jobProgressPayload(job,0,"bad",false));
  assert.throws(()=>jobProgressPayload({...job,workflow:"google_search"},1,"bad",false));
});
test("page fetch is a separately authorized browser job",()=>{
  const job={job_id:"j1",workflow:"page_fetch",query:null,prompt:null,url:"https://example.com/article",limit:1,timeout_ms:30000,authorization:{kind:"web_request",request_id:"r1",session_id:"s1",once:true}};
  assert.equal(validateJob(job,{request_id:"r1",session_id:"s1"}),job);
  assert.throws(()=>validateJob({...job,url:"http://127.0.0.1/"},{request_id:"r1",session_id:"s1"}));
});
test("ChatGPT Web accepts a bounded sliding idle timeout",()=>{
  const job={job_id:"j2",workflow:"chatgpt_web",query:null,url:null,prompt:"調査して",limit:1,timeout_ms:1200000,idle_timeout_ms:180000,authorization:{kind:"web_request",request_id:"r2",session_id:"s2",once:true}};
  assert.equal(validateJob(job,{request_id:"r2",session_id:"s2"}),job);
  assert.throws(()=>validateJob({...job,idle_timeout_ms:1200001},{request_id:"r2",session_id:"s2"}));
});
test("workflow allowlist and exact ChatGPT passthrough",()=>{ assert.throws(()=>workflowSpec("shell","x")); assert.equal(workflowSpec("page_fetch","https://example.com").workflow,"page_fetch"); assert.deepEqual(chatgptPromptContract("  exact?  ").text,"  exact?  "); });
test("confirmation and unknown mutation are fail closed",()=>{assert.equal(effectFor("job_progress"),"external_submit"); assert.equal(effectFor("job_result"),"external_submit"); assert.equal(requiresConfirmation("external_submit",false),true); assert.equal(mutationTransition("pending",true,false),"performed_or_unknown");});
test("web authorization is exact request/session scoped and cannot authorize mutations",()=>{
  const message={request_id:"r1",session_id:"s1",payload:{workflow:"google_search",authorization:{kind:"web_request",request_id:"r1",session_id:"s1",once:true}}};
  assert.equal(isAuthorizedWebWorkflow(message),true); assert.equal(requiresConfirmation("external_submit",isAuthorizedWebWorkflow(message)),false);
  assert.equal(isAuthorizedWebWorkflow({...message,request_id:"r2"}),false);
  assert.equal(isAuthorizedWebWorkflow({...message,payload:{...message.payload,authorization:{...message.payload.authorization,session_id:"other"}}}),false);
  assert.equal(isAuthorizedWebWorkflow({...message,payload:{...message.payload,workflow:"not_allowed"}}),false);
  assert.equal(isAuthorizedWebWorkflow({...message,payload:{...message.payload,workflow:"page_fetch",url:"https://example.com",query:null}}),true);
  assert.equal(requiresConfirmation("external_submit",false),true); assert.equal(requiresConfirmation("external_submit",false),true);
});
test("Google extraction rejects unsafe URLs",()=>{assert.deepEqual(googleResultsFromNodes([{title:"ok",url:"https://example.com/a"},{title:"bad",url:"javascript:alert(1)"}]),[{title:"ok",url:"https://example.com/a",snippet:""}]); assert.equal(safeUrl("javascript:alert(1)"),null);});
test("scope reuses a focused normal window and creates an inactive agent tab",async()=>{let created=false;let tabOptions;let groupOptions;const api={windows:{getAll:async()=>[{id:5,focused:false},{id:7,focused:true}],create:async()=>{created=true}},tabs:{create:async o=>{tabOptions=o;return{id:8,windowId:7}},get:async()=>({id:8,windowId:7}),group:async o=>{groupOptions=o;return 9}},tabGroups:{update:async()=>{}},storage:{local:{set:async()=>{},remove:async()=>{}}}};const ids=await new OwnedScope(api).open();assert.equal(created,false);assert.deepEqual(tabOptions,{active:false,windowId:7,url:"about:blank"});assert.deepEqual(groupOptions,{tabIds:[8],createProperties:{windowId:7}});assert.deepEqual(ids,{windowId:7,groupId:9,tabId:8});});
test("scope creates a focused:false normal window only when none exists",async()=>{let createOptions;const api={windows:{getAll:async()=>[],create:async o=>{createOptions=o;return{id:7,tabs:[{id:8,windowId:7}]}},},tabs:{create:async()=>{throw new Error("unexpected_tab_create")},get:async()=>({id:8,windowId:7}),group:async()=>9},tabGroups:{update:async()=>{}},storage:{local:{set:async()=>{},remove:async()=>{}}}};await new OwnedScope(api).open();assert.deepEqual(createOptions,{focused:false,type:"normal",url:"about:blank"});});
test("scope removes the exact created tab when grouping fails",async()=>{let removedTab; const api={windows:{getAll:async()=>[],create:async()=>({id:7,tabs:[{id:8}]}),remove:async()=>{}},tabs:{get:async()=>({id:8,windowId:7}),group:async()=>{throw new Error("group_failed")},remove:async id=>{removedTab=id}},tabGroups:{update:async()=>{}},storage:{local:{set:async()=>{},remove:async()=>{}}}}; await assert.rejects(()=>new OwnedScope(api).open(),/group_failed/); assert.equal(removedTab,8);});
test("scope groups and keeps update failure non-fatal",async()=>{let updated=false;const api={windows:{getAll:async()=>[],create:async()=>({id:7,tabs:[{id:8,windowId:7}]})},tabs:{get:async()=>({id:8,windowId:7}),group:async()=>9},tabGroups:{update:async()=>{updated=true;throw new Error("update_failed")}},storage:{local:{set:async()=>{},remove:async()=>{}}}};assert.deepEqual(await new OwnedScope(api).open(),{windowId:7,groupId:9,tabId:8});assert.equal(updated,true);});
test("scope cleanup ungroups then removes the exact agent tab",async()=>{let calls=[];const ids={windowId:7,groupId:9,tabId:8};const api={tabs:{get:async()=>({id:8,windowId:7,groupId:9}),ungroup:async ids=>calls.push(["ungroup",ids]),remove:async id=>calls.push(["remove",id])},tabGroups:{get:async()=>({id:9,windowId:7})},storage:{local:{get:async()=>({taceta_owned_scope:ids}),remove:async()=>calls.push(["storage"])}}};await new OwnedScope(api).recoverAndClose();assert.deepEqual(calls,[["ungroup",[8]],["remove",8],["storage"]]);});
test("scope removes a browser-reparented owned tab without closing its destination window",async()=>{let removedTab;let removedWindow=false;let storageRemoved;const ids={windowId:7,groupId:9,tabId:8};const api={windows:{remove:async()=>{removedWindow=true}},tabs:{get:async()=>({id:8,windowId:10,groupId:9}),remove:async id=>{removedTab=id}},tabGroups:{get:async()=>({id:9,windowId:10})},storage:{local:{get:async()=>({taceta_owned_scope:ids}),remove:async key=>{storageRemoved=key}}}};const scope=new OwnedScope(api);await scope.recoverAndClose();assert.equal(removedTab,8);assert.equal(removedWindow,false);assert.equal(storageRemoved,"taceta_owned_scope");});
test("scope removes only the exact agent tab while preserving unrelated resources",async()=>{let removedTab;const ids={windowId:7,groupId:9,tabId:8};const api={tabs:{get:async()=>({id:8,windowId:10,groupId:9}),ungroup:async()=>{},remove:async id=>{removedTab=id}},tabGroups:{get:async()=>({id:9,windowId:10})},storage:{local:{get:async()=>({taceta_owned_scope:ids}),remove:async()=>{}}}};await new OwnedScope(api).recoverAndClose();assert.equal(removedTab,8);});
test("scope serializes concurrent cleanup of the same ledger",async()=>{let removals=0;let release;const gate=new Promise(resolve=>{release=resolve});const ids={windowId:7,groupId:9,tabId:8};const api={tabs:{get:async()=>({id:8,windowId:7,groupId:9}),ungroup:async()=>{},remove:async()=>{removals++;await gate}},storage:{local:{remove:async()=>{}}}};const scope=new OwnedScope(api);scope.ledger=ids;const first=scope.close();const second=scope.close();release();await Promise.all([first,second]);assert.equal(removals,1);});
test("scope clears stale persisted IDs without closing any window",async()=>{let removedWindow=false;let storageRemoved;const ids={windowId:7,groupId:9,tabId:8};const api={windows:{remove:async()=>{removedWindow=true}},tabs:{get:async()=>{throw new Error("not_found")}},tabGroups:{get:async()=>{throw new Error("not_found")}},storage:{local:{get:async()=>({taceta_owned_scope:ids}),remove:async key=>{storageRemoved=key}}}};const scope=new OwnedScope(api);assert.equal(await scope.recover(),null);assert.equal(removedWindow,false);assert.equal(storageRemoved,"taceta_owned_scope");});
test("CDP attach uses the supported protocol on the exact owned tab",async()=>{let attachArgs;const scope={ledger:{windowId:7,groupId:9,tabId:8},validate:()=>true};const api={debugger:{attach:async(...args)=>{attachArgs=args},sendCommand:async()=>{}}};await new CdpExecutor(api,scope).attach();assert.deepEqual(attachArgs,[{tabId:8},"1.3"]);});
test("page evaluation validates the exact tab and uses scripting execution",async()=>{const calls=[];const scope={ledger:{windowId:7,groupId:9,tabId:8},validate:()=>true};const api={tabs:{get:async id=>{calls.push(["get",id]);return{id,windowId:7,url:"https://www.google.com/search?q=x",status:"complete"};}},scripting:{executeScript:async details=>{calls.push(["execute",details]);return[{result:[{title:"x",url:"https://example.com",snippet:""}]}];}},debugger:{sendCommand:async()=>{throw new Error("legacy evaluator must not be used");}}};const value=await new CdpExecutor(api,scope).evaluate(function extractSearchResults() {},[3]);assert.deepEqual(value,[{title:"x",url:"https://example.com",snippet:""}]);assert.equal(calls[0][0],"get");assert.equal(calls[1][0],"execute");assert.equal(calls[1][1].target.tabId,8);assert.equal(typeof calls[1][1].func,"function");assert.deepEqual(calls[1][1].args,[3]);});
test("page navigation waits for completion of the same exact tab",async()=>{let listener;let status="loading";const scope={ledger:{windowId:7,groupId:9,tabId:8},validate:()=>true};const api={tabs:{get:async id=>({id,windowId:7,url:"https://www.google.com/search?q=x",status})},webNavigation:{onCompleted:{addListener:fn=>{listener=fn;},removeListener:()=>{}}},debugger:{sendCommand:async()=>{status="loading";return{};}}};const executor=new CdpExecutor(api,scope);await executor.send("navigate",{url:"https://www.google.com/search?q=x"});const pending=executor.waitForLoad(1000);status="complete";listener({tabId:8,frameId:0});await pending;assert.equal(executor.navigationPending,false);});
test("default search uses official API on the already-owned tab",async()=>{let call; await runDefaultSearch({chrome:{search:{query:async q=>{call=q;}}},tabId:8,query:"exact query"}); assert.deepEqual(call,{text:"exact query",tabId:8});});
