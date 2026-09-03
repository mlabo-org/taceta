# Taceta architecture boundary

この文書は、Taceta の Rust アプリと Taceta Link の責務、そして Web Search の外部作用を定義します。Taceta は単一 Git repo / version 内に、Rust app `src/` と独立 component `browser-extension/` を物理的に持ちます。

## アプリとローカル経路

`InferenceBackend` は local chat の境界です。モデル、会話入力、添付、Thinking 設定、Web Search 設定を受け取り、Thinking delta、content delta、検索進捗、参照元、完了、失敗を返します。Thinking trace は次の入力へ混ぜません。

Web Search OFF では外部 request を作りません。ON では過去の履歴を除いた現在入力だけをローカルの構造化ルーターへ渡し、`local`、`search_current`、`search_generated` のいずれかを選ばせます。通常会話、普遍的な説明、創作は `local` です。現在性、特定日時点、リリース、価格、存在確認、出典など外部事実に依存する質問は `search_current` とし、モデルの古い知識と矛盾する名前や前提も検索せず否定しません。質問自体をモデルに作らせてから検索する入力は `search_generated` です。明示的な検索命令はルーターを迂回して必ず検索します。自由文、拒否、不正 JSON は `local` へ戻さず、外部未送信の route error として停止します。LLM が通常文で即時検索を予告した場合も、その文を回答として確定せず1ターン1回だけ検索へ昇格します。検索時は設定された executor だけを適用し、外部結果は untrusted context としてローカル Ollama の最終回答に渡します。provider は暗黙に切り替えません。

Taceta Link は同じ version を持つ MV3 拡張、Native Messaging Host `org.mlabo.taceta.link`、user-only Unix socket で構成します。アプリが job を socket へ置き、拡張が poll して実行結果を返します。product version / protocol version / extension ID の不一致は fail-closed です。Cookie、token、profile、local storage を読み出したり輸出したりしません。

ブラウザー executor は focused を優先して既存の normal window を作業コンテナとして再利用し、その中に非アクティブな agent tab / group を作成します。normal window がない場合だけ非フォーカス window を作成します。window は所有・削除せず、終了時は追跡した exact agent tab を ungroup して削除します。Default Search と Google Search は query を渡します。直接検索では、ChatGPT Web の1回目に現在のuser promptをexactに渡します。ローカルモデルが具体的な検索質問を作った場合は、現在のpromptをauthorityとして保持したenvelopeにその質問を加えます。利用者が2〜3回を明示許可した場合だけ追加調査を行い、誤った固有名詞、version、前提を訂正するよう指示します。Tacetaが過去の履歴、system message、attachments、Thinking traceを直接付加することはありません。ChatGPT Webの逐次回答と引用URLはTacetaへ戻します。質問回数上限は既定1、設定可能範囲1〜3です。

## Web ON の承認と安全境界

Web ON + Send は現在入力のローカル判定を許可し、検索が必要な場合だけ一つの Web turn を作ります。明示検索は判定を迂回し、通常会話は外部へ送りません。ChatGPT Web の turn で作成できる request は設定上限の1〜3件までで、各 job は再利用できない個別の authorization を持ちます。停止またはdropされた未完了jobはqueueとwaiterから取り除き、次のturnへ残しません。結果不明状態の同一jobは再試行しません。検索結果は最終回答そのものではなく、ローカル Ollama が生成する回答の untrusted context です。ログイン、アカウント変更、購入、削除などの destructive/account action は別途利用者の確認が必要です。

## インストール責務

アプリは macOS のデフォルトブラウザーを検出し、初期対応の Brave / Chrome に限って、拡張を Taceta Application Support 配下へ materialize します。Native Messaging Host をそのユーザー専用のブラウザー領域へ登録し、version と固定 ID `hefhkgbiiajifedgjlbiklclooifkidg` を検証してから、拡張管理ページを開きます。利用者は Developer mode を ON にし、Load unpacked / Add で materialized `browser-extension` directory を選びます。この最後の browser approval は自動化しません。更新時は拡張管理ページで Reload を案内します。Safari 等は Brave / Chrome の導入とデフォルト設定へ案内し、未対応ブラウザーへの登録は行いません。

## 将来の typed agent-harness 境界

GUI 完成後に、tool calls、approval、sandbox、workdir、子 process lifecycle を扱う optional な typed agent-harness を追加する余地があります。これは `InferenceBackend` や Taceta Link の責務へ混ぜず、明示的な handoff で接続します。現時点の Taceta はその harness、Codex、または別の外部実行基盤に依存しません。

---

# Taceta architecture boundary (English)

This document defines the responsibilities of Taceta's Rust app and Taceta Link, plus the external-effect boundary for Web Search. One Git repository and product version contain two physically separate components: the Rust app in `src/` and the independent component in `browser-extension/`.

## App and local transport

`InferenceBackend` owns local chat. It accepts model, conversation input, attachments, Thinking settings, and Web Search settings, then emits Thinking deltas, content deltas, search progress, citations, completion, and failure. Thinking traces never enter the next input.

With Web Search OFF, no external request is created. When it is ON, a local structured router receives only the current input, never conversation history, and chooses `local`, `search_current`, or `search_generated`. Timeless explanation, writing, and casual conversation stay local. Questions that depend on current, date-specific, released, priced, sourced, existence, or otherwise externally verifiable facts use `search_current`; a name or premise that conflicts with old model knowledge must be verified rather than denied. A request to have the model formulate a question before searching uses `search_generated`. An explicit search command bypasses the router and always searches. Free text, refusal, or invalid JSON never silently falls back to a local answer. If the answering LLM nevertheless announces an immediate search in ordinary text, Taceta suppresses that announcement and promotes it to one real search per turn. The configured executor is used without provider fallback, and external output is untrusted context for a final answer generated locally by Ollama.

Taceta Link consists of a same-version MV3 extension, Native Messaging Host `org.mlabo.taceta.link`, and a user-only Unix socket. The app places jobs on the socket; the extension polls and returns results. Product version, protocol version, or extension-ID mismatch fails closed. Cookies, tokens, profiles, and local storage are never read or exported.

The browser executor prefers an existing focused normal window as its route container, creating an inactive agent tab and group there; only when no normal window exists does it create a non-focused normal window. The window is never owned or closed. At session end it ungroups and removes only the exact tracked agent tab. Default Search and Google Search receive a query. For a direct search request, ChatGPT Web receives the current user prompt exactly on the first request. When the current prompt asks the local model to formulate the question first, or the model itself produces the first concrete `web_search` query, that first request instead contains an anchored envelope with the current prompt and the concrete query. Only when the user explicitly allows two or three requests do later requests attach another local-model query as an unverified research angle and instruct ChatGPT to correct mistaken names, versions, and premises. Taceta does not directly attach earlier conversation history, system messages, attachments, or Thinking traces. Its request limit defaults to one and can be set from one to three independently of the maximum search-result count used by search engines.

## Web ON authorization and safety

Web ON + Send permits local routing of the current input and creates one web turn only when needed; an explicit search command bypasses routing, while ordinary conversation remains local. That turn may create from one to three ChatGPT Web requests up to the configured limit, and each job receives a distinct, non-reusable authorization. A stopped or dropped pending job is removed from the queue and waiters so it cannot block the next turn. An unknown outcome is not retried for the same job. Search output is untrusted context, not the final answer; local Ollama generates that answer. Login, account changes, purchases, deletions, and other destructive/account actions still require separate user confirmation.

## Installation responsibility

The app detects the macOS default browser and supports Brave and Chrome initially. It materializes the extension under Taceta Application Support, registers the per-user Native Messaging Host, verifies version and fixed ID `hefhkgbiiajifedgjlbiklclooifkidg`, and opens the extension-management page. The user turns on Developer mode and chooses Load unpacked / Add for the materialized `browser-extension` directory. This final browser approval remains manual; it is not silently automated. Updates guide the user to press Reload. Safari and other unsupported browsers are directed to install Brave or Chrome and make one the default; no registration is attempted for an unsupported browser.

## Future typed agent-harness boundary

After the GUI is complete, an optional typed agent-harness may be added for tool calls, approvals, sandbox, workdir, and child-process lifecycle. It will remain separate from `InferenceBackend` and Taceta Link and connect through an explicit handoff. Current Taceta has no dependency on that harness, Codex, or another external execution platform.
