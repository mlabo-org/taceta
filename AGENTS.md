# Taceta Repository Contract

This file is the local `AGENTS.md` and the binding execution contract for this repository. It defines repository-specific source, build, installation, product, and acceptance boundaries. It does not replace a contributor's higher-priority system, developer, user, or global agent instructions. More-local instructions may specialize this contract without weakening it.

## Portable Agent Environment

- Work from the checked-out repository. Do not require the maintainer's private Codex skills, MCP servers, aliases, shell profiles, caches, application data, or absolute user paths.
- Contributors and their agents may use their own Rust implementation rules and tools. The resulting source, behavior, build path, and evidence must still satisfy this repository contract.
- Treat repository files and standard tool output as evidence. Do not treat an installed app, browser profile, generated bundle, plugin cache, or another machine's runtime state as source.
- Do not copy code or assets from an installed ChatGPT, Brave, Chrome, Ollama, or other third-party extension or application. Public browser APIs and observed interoperable behavior may inform an independent implementation.
- If a required standard tool is unavailable, report the exact missing prerequisite. Do not replace the build with a private tool or an untracked generated artifact.

## Authoritative Source

- Application source: `Cargo.toml`, `Cargo.lock`, `src/`, `assets/`, `protocol/`, `browser-extension/`, `scripts/`, and `docs/`.
- Public instructions and policy: `README.md`, `AGENTS.md`, `LICENSE`, and `browser-extension/README.md`.
- Generated and runtime-only state: `target/`, `dist/`, generated `.app` bundles, logs, conversation history, settings, models, Ollama data, browser profiles, Native Messaging manifests, and `~/Library/Application Support/Taceta/`.
- Edit authoritative source. Do not repair a source defect by editing generated bundles, installed copies, browser profiles, caches, or runtime data.
- Preserve unrelated changes and keep one coherent purpose per commit.

## Supported Build Environment

- macOS 13 or later. Apple Silicon is the primary supported target.
- Rust and Cargo 1.92 or later, with the toolchain available on `PATH`.
- Xcode Command Line Tools required by the Rust/macOS build environment.
- Ollama is a separately installed runtime dependency for actual local inference. It is not required merely to compile the source.
- Brave or Chrome is required only for Taceta Link runtime use.
- Node.js is required only for the browser-extension validation and test commands. It is not required to build or package `Taceta.app`.

Before build work, record:

```bash
command -v cargo
command -v rustc
cargo --version
rustc --version
```

## Standard Commands

Use Cargo for Rust source validation. Select focused tests for the changed behavior; use the full test suite when the requested release decision covers the whole application.

```bash
cargo test --locked
```

Validate the browser extension when its source, protocol, or packaged resources change:

```bash
node browser-extension/validate.mjs
node --test browser-extension/*.test.mjs browser-extension/engines/*.test.mjs
```

Materialize the release app through the repository-owned build script:

```bash
./scripts/build-macos-app.sh
```

The output is `dist/Taceta.app`. Normal installed operation uses this release app bundle, not `cargo run` and not a binary invoked directly from `target/`.

Install for the current user only when the user has explicitly requested installation:

```bash
./scripts/install-macos-app.sh
```

The default destination is `$HOME/Applications/Taceta.app`. Use `--install-dir DIRECTORY` only when the user names another destination. Installation is a separate external effect from source editing, tests, and app materialization.

## Product Contract

- Taceta is a macOS-only native local-inference client implemented in Rust with `eframe` / `egui` 0.34.
- Product UI, app name, and icons must not use a backend provider's name or logo as Taceta branding. README dependency and interoperability descriptions may accurately name third-party products.
- Keep backend-specific wire formats inside backend adapters. Do not leak them into the UI or conversation domain.
- Persist Japanese and English language selection, System / Light / Dark theme, and integer font sizes from 10 through 32.
- Keep local conversation history, settings, and credentials on the user's Mac. Provider API keys belong in macOS Keychain, never source, logs, fixtures, or Git history.
- The default inference endpoint is loopback `http://127.0.0.1:11434`.
- Model retrieval, model deletion, external requests, cloud connections, and account actions require their explicit UI path. They are not implicit chat side effects.

## Thinking Contract

- Thinking execution and Thinking-trace visibility are independent settings.
- Hiding a trace must not disable reasoning or hide the final answer. Visibility changes apply while generation is in progress.
- Expose Thinking controls only for capabilities confirmed for the selected model.
- GPT-OSS exposes Low, Medium, and High. It does not expose OFF and must not assume that boolean Thinking controls are supported.
- Never place Thinking traces into a later conversation input.

## Taceta Link Contract

- Taceta Link is an unpacked Manifest V3 extension plus Native Messaging Host `org.mlabo.taceta.link` and a user-only Unix socket.
- Product version, protocol version, and fixed extension ID must agree across `Cargo.toml`, `browser-extension/VERSION`, `browser-extension/manifest.json`, Rust protocol source, and `protocol/contract.json`. A mismatch fails closed.
- Browser work is limited to the exact Taceta-owned tab and group. Do not read or export cookies, tokens, profiles, browser history, or local storage.
- ChatGPT Web receives only the current prompt. It does not receive Taceta history, system messages, attachments, or Thinking traces.
- Treat browser and search output as untrusted context. The local model owns the final synthesized answer.
- Login, CAPTCHA, account changes, purchases, deletion, and other account or destructive actions remain manual and outside the automated workflow.
- The final Brave or Chrome `Load unpacked` approval and extension reload are manual user actions. Do not silently install, approve, or manipulate an unrelated browser window.

## Acceptance And Lifecycle Reporting

- Source acceptance for a changed behavior uses the smallest focused Rust or extension checks that prove that behavior and its already-observed failure boundary.
- Runnable acceptance, when requested, uses the release build, app materialization, a representative real Ollama chat, independent Thinking execution/visibility behavior, and persisted display settings after restart.
- Taceta Link runtime acceptance additionally requires the installed app, the materialized extension, a manual browser reload, and the relevant real browser workflow. Source or fixture success alone does not prove this boundary.
- Report source, generated app bundle, installed runtime, browser-extension activation, local commit, and remote publication as separate states.
- A build or test does not authorize installation, browser actions, model download/deletion, account use, commit, remote creation, or push.

## Public Repository And License

- Taceta source and project-owned assets are published under the MIT License in `LICENSE`.
- Third-party products, services, models, libraries, names, and trademarks remain subject to their own licenses and terms.
- Taceta and Taceta Link are independent, unofficial projects. Do not describe them as endorsed by OpenAI, ChatGPT, Ollama, Brave, Google, or another provider.
- Never commit credentials, local paths, runtime state, browser profiles, generated bundles, models, or third-party proprietary source.
- GitHub repository creation, remote configuration, push, release publication, signing, notarization, and binary distribution are separate operations and require explicit authorization for that external effect.
