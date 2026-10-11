#!/usr/bin/env node
import fs from "node:fs";
import crypto from "node:crypto";
const root=new URL(".",import.meta.url); const read=n=>fs.readFileSync(new URL(n,root),"utf8");
const contract=JSON.parse(fs.readFileSync(new URL("../protocol/contract.json",root),"utf8"));
const fixture=JSON.parse(fs.readFileSync(new URL("../protocol/fixture.json",root),"utf8"));
const protocol=await import("./protocol.js");
for (const [key, value] of Object.entries({schema_version:protocol.SCHEMA_VERSION, product_version:protocol.PRODUCT_VERSION, protocol_version:protocol.PROTOCOL_VERSION})) if (fixture[key]!==value || contract.properties[key].const!==value) throw new Error(`protocol contract mismatch: ${key}`);
if (fixture.message_type!=="request" || fixture.operation!=="poll_job" || !contract.properties.operation.enum.includes(fixture.operation)) throw new Error("invalid shared protocol fixture");
const manifest=JSON.parse(read("manifest.json")); const version=read("VERSION").trim();
const cargo=fs.readFileSync(new URL("../Cargo.toml",root),"utf8").match(/^version\s*=\s*"([^"]+)"/m)?.[1];
if(manifest.version!==version || cargo!==version || protocol.PRODUCT_VERSION!==version) throw new Error(`version mismatch manifest=${manifest.version} VERSION=${version} cargo=${cargo} protocol.js=${protocol.PRODUCT_VERSION}`);
// Chromium extension IDs encode each SHA-256 hex nibble as a-p (0->a ... f->p).
const digest=crypto.createHash("sha256").update(Buffer.from(manifest.key,"base64")).digest("hex");
const id=[...digest.slice(0,32)].map(c=>String.fromCharCode(97+Number.parseInt(c,16))).join("");
if(!/^[a-p]{32}$/.test(id)) throw new Error(`invalid Chromium extension ID: ${id}`);
const host=JSON.parse(read("native-host-manifest.template.json"));
if(host.name!=="org.mlabo.taceta.link" || !host.allowed_origins.includes(`chrome-extension://${id}/`)) throw new Error(`identity mismatch: ${id}`);
if([...manifest.permissions, ...(manifest.host_permissions||[])].some(p=>p==="<all_urls>" || /^(\*|http|file|ftp):/.test(p))) throw new Error("host access beyond HTTPS is forbidden");
console.log(`Taceta Link ${version} valid (${id})`);
