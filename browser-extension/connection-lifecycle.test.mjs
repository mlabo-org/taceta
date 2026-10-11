import test from "node:test";
import assert from "node:assert/strict";
import { PRODUCT_VERSION, PROTOCOL_VERSION, SCHEMA_VERSION } from "./protocol.js";

// background.js talks to the browser at import time, so it runs here against
// a recording stand-in for the extension APIs.
function fakeBrowser() {
  const ports = [];
  const alarms = [];
  const listeners = () => { const set = []; return { addListener: (fn) => set.push(fn), set }; };
  const chrome = {
    alarms: { create: (name) => alarms.push(name), clear: () => {}, onAlarm: listeners() },
    runtime: {
      lastError: undefined,
      onStartup: listeners(),
      onInstalled: listeners(),
      connectNative(name) {
        const port = { name, sent: [], disconnected: false, onMessage: listeners(), onDisconnect: listeners(),
          postMessage(message) { this.sent.push(message); }, disconnect() { this.disconnected = true; } };
        ports.push(port);
        return port;
      },
    },
    storage: { local: { get: async () => ({}), set: async () => {}, remove: async () => {} } },
  };
  return { chrome, ports, alarms };
}

const settle = (ms = 20) => new Promise((resolve) => setTimeout(resolve, ms));

test("the extension announces itself, polls, and starts a fresh session after an unusable message", async () => {
  const browser = fakeBrowser();
  globalThis.chrome = browser.chrome;
  await import("./background.js");
  await settle();

  assert.equal(browser.ports.length, 1);
  const [first] = browser.ports;
  assert.equal(first.name, "org.mlabo.taceta.link");
  assert.deepEqual(first.sent.map((message) => message.operation), ["extension_ready", "poll_job"]);
  assert.equal(first.sent[0].payload.version, PRODUCT_VERSION);
  const firstSession = first.sent[0].session_id;

  // A response from a different product version cannot be handled.
  first.onMessage.set[0]({ schema_version: SCHEMA_VERSION, product_version: "0.0.0-other", protocol_version: PROTOCOL_VERSION,
    message_type: "response", request_id: "r", session_id: firstSession, operation: "poll_job", payload: { job: null } });
  await settle();
  assert.equal(first.disconnected, true);
  assert.deepEqual(browser.alarms, ["taceta-link-reconnect"]);

  await settle(1100);
  assert.equal(browser.ports.length, 2);
  const second = browser.ports[1];
  assert.deepEqual(second.sent.map((message) => message.operation), ["extension_ready", "poll_job"]);
  assert.notEqual(second.sent[0].session_id, firstSession);
});
