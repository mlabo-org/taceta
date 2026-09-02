import test from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
test("MV3 reconnect contract has alarm wake, duplicate guard and no job replay",()=>{
  const source=fs.readFileSync(new URL("./background.js",import.meta.url),"utf8");
  assert.match(source,/chrome\.alarms\.create\("taceta-link-reconnect"/);
  assert.match(source,/if \(port \|\| connecting\) return/);
  assert.match(source,/const ignored=chrome\.runtime\.lastError/);
  assert.match(source,/seenJobs\.has\(job\.job_id\)/);
  assert.match(source,/releaseOwnedScope\(\)\.finally\(scheduleReconnect\)/);
  assert.match(source,/scope\.recoverAndClose\(\)/);
});
