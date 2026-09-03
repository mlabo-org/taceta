# Taceta architecture boundary

この文書は、Taceta の Rust アプリと Taceta Link の責務、そして Web Search の外部作用を定義します。Taceta は単一 Git repo / version 内に、Rust app `src/` と独立 component `browser-extension/` を物理的に持ちます。

## アプリとローカル経路

`InferenceBackend` は local chat の境界です。モデル、会話入力、添付、Thinking 設定、Web Search 設定を受け取り、Thinking delta、content delta、検索進捗、参照元、完了、失敗を返します。Thinking trace は次の入力へ混ぜません。

Web Search OFF では外部 request を作りません。ON でも全会話を検索へ送ることはありません。現在の入力に明示的な検索命令、現在性・最新性または特定日時点の確認、出典 URL の要求など明白な検索意図がある場合だけ、Taceta は LLM が発話する前に最低1回の検索を完了させます。過去の会話履歴は意図判定に使いません。通常会話や曖昧な入力では検索を強制せず、LLM が必要と判断したときだけ従来どおり tool call を許可します。さらに、LLM が tool call を返さず、現在の入力に対して直ちに検索する意思だけを通常テキストで明言した場合は、その予告文を最終回答として表示せず、同じ入力を1ターンにつき1回だけ検索へ回します。検索についての説明、過去形、否定、質問、通常会話はこのフォールバックの対象外です。検索時は設定された executor だけを適用します。Brave Search / Ollama Web Search は API 経路、Taceta Link はブラウザー経路です。外部結果は untrusted context としてローカル Ollama の最終回答に渡します。provider は暗黙に切り替えません。

Taceta Link は同じ version を持つ MV3 拡張、Native Messaging Host `org.mlabo.taceta.link`、user-only Unix socket で構成します。アプリが job を socket へ置き、拡張が poll して実行結果を返します。product version / protocol version / extension ID の不一致は fail-closed です。Cookie、token、profile、local storage を読み出したり輸出したりしません。

ブラウザー executor は focused を優先して既存の normal window を作業コンテナとして再利用し、その中に非アクティブな agent tab / group を作成します。normal window がない場合だけ非フォーカス window を作成します。window は所有・削除せず、終了時は追跡した exact agent tab を ungroup して削除します。Default Search と Google Search は query を渡します。ChatGPT Web の1回目は現在のuser promptをexactに渡します。利用者が2〜3回を明示許可した場合だけ、追加分は選択中のローカルモデルが生成した `web_search` queryを未検証の追加調査案としてuser promptに添え、誤った固有名詞、version、前提を訂正するよう指示します。Tacetaが履歴、system message、attachments、Thinking traceを直接付加することはありません。ChatGPT Web の質問回数上限は既定1、設定可能範囲1〜3で、検索エンジンの最大検索結果数とは独立です。

## Web ON の承認と安全境界

明白な検索意図を含む入力での Web ON + Send は、回答前に最低1回の検索を完了させたうえで一つの Web turn を許可します。通常会話や曖昧な入力では検索を強制しません。ChatGPT Web の turn で作成できる request は設定上限の1〜3件までで、各 job は再利用できない個別の authorization を持ちます。結果不明状態の同一 job は再試行しません。検索結果は最終回答そのものではなく、ローカル Ollama が生成する回答の untrusted context です。ログイン、アカウント変更、購入、削除などの destructive/account action は別途利用者の確認が必要です。

## インストール責務

アプリは macOS のデフォルトブラウザーを検出し、初期対応の Brave / Chrome に限って、拡張を Taceta Application Support 配下へ materialize します。Native Messaging Host をそのユーザー専用のブラウザー領域へ登録し、version と固定 ID `hefhkgbiiajifedgjlbiklclooifkidg` を検証してから、拡張管理ページを開きます。利用者は Developer mode を ON にし、Load unpacked / Add で materialized `browser-extension` directory を選びます。この最後の browser approval は自動化しません。更新時は拡張管理ページで Reload を案内します。Safari 等は Brave / Chrome の導入とデフォルト設定へ案内し、未対応ブラウザーへの登録は行いません。

## 将来の typed agent-harness 境界

GUI 完成後に、tool calls、approval、sandbox、workdir、子 process lifecycle を扱う optional な typed agent-harness を追加する余地があります。これは `InferenceBackend` や Taceta Link の責務へ混ぜず、明示的な handoff で接続します。現時点の Taceta はその harness、Codex、または別の外部実行基盤に依存しません。

---

# Taceta architecture boundary (English)

This document defines the responsibilities of Taceta's Rust app and Taceta Link, plus the external-effect boundary for Web Search. One Git repository and product version contain two physically separate components: the Rust app in `src/` and the independent component in `browser-extension/`.

## App and local transport

`InferenceBackend` owns local chat. It accepts model, conversation input, attachments, Thinking settings, and Web Search settings, then emits Thinking deltas, content deltas, search progress, citations, completion, and failure. Thinking traces never enter the next input.

With Web Search OFF, no external request is created. ON does not send every conversation to the web. Only a clear search intent in the current input—such as an explicit search command, a request to verify current/latest or date-specific information, or a request for source URLs—requires Taceta to complete at least one search before the LLM speaks. Conversation history is not used to detect intent. Normal conversation and ambiguous input are not forced to search; the LLM may still call the tool when it determines that search is needed, as before. In addition, if the LLM returns no tool call but plainly states in ordinary text that it will search immediately for the current input, Taceta suppresses that announcement instead of displaying it as the final answer and routes the same input to search once per turn. Explanations, past-tense statements, negations, questions about searching, and normal conversation do not trigger this fallback. When searching, the configured executor is applied: Brave Search / Ollama Web Search through their APIs, or Taceta Link through the browser. Returned external data is untrusted context for a final answer generated locally by Ollama. Providers never silently fall back.

Taceta Link consists of a same-version MV3 extension, Native Messaging Host `org.mlabo.taceta.link`, and a user-only Unix socket. The app places jobs on the socket; the extension polls and returns results. Product version, protocol version, or extension-ID mismatch fails closed. Cookies, tokens, profiles, and local storage are never read or exported.

The browser executor prefers an existing focused normal window as its route container, creating an inactive agent tab and group there; only when no normal window exists does it create a non-focused normal window. The window is never owned or closed. At session end it ungroups and removes only the exact tracked agent tab. Default Search and Google Search receive a query. ChatGPT Web receives the current user prompt exactly on the first request. Only when the user explicitly allows two or three requests do later requests attach the selected local model's `web_search` query as an unverified research angle and instruct ChatGPT to correct mistaken names, versions, and premises. Taceta does not directly attach history, system messages, attachments, or Thinking traces. Its request limit defaults to one and can be set from one to three independently of the maximum search-result count used by search engines.

## Web ON authorization and safety

For an input with clear search intent, Web ON + Send completes at least one search before allowing one web turn. Normal and ambiguous conversation is not forced to search. That turn may create from one to three ChatGPT Web requests up to the configured limit, and each job receives a distinct, non-reusable authorization. An unknown outcome is not retried for the same job. Search output is untrusted context, not the final answer; local Ollama generates that answer. Login, account changes, purchases, deletions, and other destructive/account actions still require separate user confirmation.

## Installation responsibility

The app detects the macOS default browser and supports Brave and Chrome initially. It materializes the extension under Taceta Application Support, registers the per-user Native Messaging Host, verifies version and fixed ID `hefhkgbiiajifedgjlbiklclooifkidg`, and opens the extension-management page. The user turns on Developer mode and chooses Load unpacked / Add for the materialized `browser-extension` directory. This final browser approval remains manual; it is not silently automated. Updates guide the user to press Reload. Safari and other unsupported browsers are directed to install Brave or Chrome and make one the default; no registration is attempted for an unsupported browser.

## Future typed agent-harness boundary

After the GUI is complete, an optional typed agent-harness may be added for tool calls, approvals, sandbox, workdir, and child-process lifecycle. It will remain separate from `InferenceBackend` and Taceta Link and connect through an explicit handoff. Current Taceta has no dependency on that harness, Codex, or another external execution platform.
