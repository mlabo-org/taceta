# Taceta architecture boundary

この文書は、Taceta の Rust アプリと Taceta Link の責務、そして Web Search の外部作用を定義します。Taceta は単一 Git repo / version 内に、Rust app `src/` と独立 component `browser-extension/` を物理的に持ちます。

## アプリとローカル経路

`InferenceBackend` は通常チャットの境界です。モデル、会話入力、添付、Thinking 設定、Web Search 設定を受け取り、Thinking delta、content delta、検索進捗、参照元、完了、失敗を返します。OllamaとGrokは同じ境界に接続する推論先で、各アダプターが固有の通信形式を所有します。Thinking trace は次の入力へ混ぜません。

Ollamaの通常チャットでは、Web Search OFF で外部検索 request を作りません。ON では過去の履歴を除いた現在入力だけをローカルの構造化ルーターへ渡し、`local`、`search_current`、`search_generated` のいずれかを選ばせます。通常会話、普遍的な説明、創作は `local` です。現在性、特定日時点、リリース、価格、存在確認、出典など外部事実に依存する質問は `search_current` とし、モデルの古い知識と矛盾する名前や前提も検索せず否定しません。質問自体をモデルに作らせてから検索する入力は `search_generated` です。明示的な検索命令はルーターを迂回して必ず検索します。自由文、拒否、不正 JSON は `local` へ戻さず、外部未送信の route error として停止します。LLM が通常文で即時検索を予告した場合も、その文を回答として確定せず1ターン1回だけ検索へ昇格します。検索時は設定された executor だけを適用し、外部結果は untrusted context としてローカル Ollama の最終回答に渡します。provider は暗黙に切り替えません。

Taceta Link は同じ version を持つ MV3 拡張、Native Messaging Host `org.mlabo.taceta.link`、user-only Unix socket で構成します。アプリが job を socket へ置き、拡張が poll して実行結果を返します。product version / protocol version / extension ID の不一致は fail-closed です。Cookie、token、profile、local storage を読み出したり輸出したりしません。

ブラウザー executor は focused を優先して既存の normal window を作業コンテナとして再利用し、その中に非アクティブな agent tab / group を作成します。normal window がない場合だけ非フォーカス window を作成します。window は所有・削除せず、終了時は追跡した exact agent tab を ungroup して削除します。Google Search は query を渡します。直接検索では、ChatGPT Web の1回目に現在のuser promptをexactに渡します。ローカルモデルが具体的な検索質問を作った場合は、現在のpromptをauthorityとして保持したenvelopeにその質問を加えます。利用者が2〜3回を明示許可した場合だけ追加調査を行い、誤った固有名詞、version、前提を訂正するよう指示します。Tacetaが過去の履歴、system message、attachments、Thinking traceを直接付加することはありません。ChatGPT Webの逐次回答と引用URLはTacetaへ戻します。質問回数上限は既定1、設定可能範囲1〜3です。

## Web ON の承認と安全境界

Web ON + Send は現在入力のローカル判定を許可し、検索が必要な場合だけ一つの Web turn を作ります。明示検索は判定を迂回し、通常会話は外部へ送りません。ChatGPT Web の turn で作成できる request は設定上限の1〜3件までで、各 job は再利用できない個別の authorization を持ちます。停止またはdropされた未完了jobはqueueとwaiterから取り除き、次のturnへ残しません。結果不明状態の同一jobは再試行しません。検索結果は最終回答そのものではなく、ローカル Ollama が生成する回答の untrusted context です。ログイン、アカウント変更、購入、削除などの destructive/account action は別途利用者の確認が必要です。

## インストール責務

アプリは macOS のデフォルトブラウザーを検出し、初期対応の Brave / Chrome に限って、拡張を Taceta Application Support 配下へ materialize します。Native Messaging Host をそのユーザー専用のブラウザー領域へ登録し、version と固定 ID `hefhkgbiiajifedgjlbiklclooifkidg` を検証してから、拡張管理ページを開きます。利用者は Developer mode を ON にし、Load unpacked / Add で materialized `browser-extension` directory を選びます。この最後の browser approval は自動化しません。更新時は拡張管理ページで Reload を案内します。Safari 等は Brave / Chrome の導入とデフォルト設定へ案内し、未対応ブラウザーへの登録は行いません。

## Grok接続と作業実行

`backend/grok` は、公開Grok Build方式のOAuth認証、Taceta専用キーチェーン、トークン更新、モデル一覧、ストリーミング推論を所有します。ログインは設定画面から明示的に開始し、PKCE、state、一時的なloopback callbackを使用します。モデル一覧の通信形式に従いChat CompletionsまたはResponsesを使い、失敗時に別のサービスへ切り替えません。公開client IDのTacetaでの受理とアカウントの利用権は、実サービスでの確認が必要です。Grokを選択した送信はクラウド推論であり、会話と必要な作業内容をxAIへ送ります。

OAuth proxyの互換性チェックには、実装の参照元であるGrok Build `1.0.24` を `x-grok-client-version` として送ります。この値は[参照commitの版定義](https://github.com/xai-org/grok-build/blob/37949780c144e37df692e3d669051a21fec24f20/crates/codegen/xai-grok-version/Cargo.toml)に対応し、[公開samplerが送るヘッダー](https://github.com/xai-org/grok-build/blob/37949780c144e37df692e3d669051a21fec24f20/crates/codegen/xai-grok-sampler/src/client.rs#L554-L608)に合わせています。TacetaのUser-Agentとclient identifierはTaceta自身を示します。互換性値はソース内で固定し、通信契約の対応なしに最新リリースへ追随させません。HTTP 426の表示は互換性更新の要求を伝え、Tacetaの利用禁止やアカウント拒否とは判定しません。

`agent` がTaceta自身の作業実行を所有し、`AgentModel` を介してOllamaまたはGrokへ接続します。UIは作業フォルダー、モデル、指示、上限を渡し、進捗・承認要求・保存済み状態を表示します。ファイル読み取り、編集、検索、コマンド実行は作業フォルダーに束縛され、編集とコマンドには個別の承認が必要です。コマンドの書き込み先は作業フォルダーと専用一時領域に限り、ネットワークは許可しません。停止・時間上限・アプリ終了では子プロセスも終了します。Taceta Linkと外部CLIはこの実行経路を所有しません。

## コンパクションと再開

元の出来事を保存する追記式の記録と、現在モデルに渡す入力を分離します。目的、ユーザー指示、構造化した作業状態は要約から独立して保持し、Thinking traceを後続入力へ戻しません。圧縮はツール呼び出しと結果の組が完了した境界で行い、モデルの入力上限と返却使用量を基準に、要約と次の出力の余白を確保します。手動圧縮も同じ処理を使用します。

成功した要約、保持履歴の範囲、元記録の境界、window IDと前window ID、使用モデル、context長を再開地点に保存します。再起動は最新の再開地点と後続記録から復元し、元の記録は履歴検索と読み取りで参照できます。要約が未完了・失敗の場合は再開地点を置き換えません。入力上限に必須情報が収まらなければ、切り捨てず停止します。結果不明の編集やコマンドは自動で再実行しません。

通常のモデル推論で要約を作り、モデルや推論先を越えて扱えるテキストと構造化状態として保存します。OAuth proxyで専用の圧縮APIやCodex固有の不透明な圧縮項目を利用できるとは仮定しません。Codexの公開実装を設計上の参考としていますが、サービス側の圧縮や長時間作業のモデル品質との同等性は未確認です。

---

# Taceta architecture boundary (English)

This document defines the responsibilities of Taceta's Rust app and Taceta Link, plus the external-effect boundary for Web Search. One Git repository and product version contain two physically separate components: the Rust app in `src/` and the independent component in `browser-extension/`.

## App and local transport

`InferenceBackend` owns regular chat. It accepts model, conversation input, attachments, Thinking settings, and Web Search settings, then emits Thinking deltas, content deltas, search progress, citations, completion, and failure. Ollama and Grok implement this same boundary, with provider wire formats owned by their adapters. Thinking traces never enter the next input.

In regular Ollama chat, Web Search OFF creates no external search request. When it is ON, a local structured router receives only the current input, never conversation history, and chooses `local`, `search_current`, or `search_generated`. Timeless explanation, writing, and casual conversation stay local. Questions that depend on current, date-specific, released, priced, sourced, existence, or otherwise externally verifiable facts use `search_current`; a name or premise that conflicts with old model knowledge must be verified rather than denied. A request to have the model formulate a question before searching uses `search_generated`. An explicit search command bypasses the router and always searches. Free text, refusal, or invalid JSON never silently falls back to a local answer. If the answering LLM nevertheless announces an immediate search in ordinary text, Taceta suppresses that announcement and promotes it to one real search per turn. The configured executor is used without provider fallback, and external output is untrusted context for a final answer generated locally by Ollama.

Taceta Link consists of a same-version MV3 extension, Native Messaging Host `org.mlabo.taceta.link`, and a user-only Unix socket. The app places jobs on the socket; the extension polls and returns results. Product version, protocol version, or extension-ID mismatch fails closed. Cookies, tokens, profiles, and local storage are never read or exported.

The browser executor prefers an existing focused normal window as its route container, creating an inactive agent tab and group there; only when no normal window exists does it create a non-focused normal window. The window is never owned or closed. At session end it ungroups and removes only the exact tracked agent tab. Google Search receives a query. For a direct search request, ChatGPT Web receives the current user prompt exactly on the first request. When the current prompt asks the local model to formulate the question first, or the model itself produces the first concrete `web_search` query, that first request instead contains an anchored envelope with the current prompt and the concrete query. Only when the user explicitly allows two or three requests do later requests attach another local-model query as an unverified research angle and instruct ChatGPT to correct mistaken names, versions, and premises. Taceta does not directly attach earlier conversation history, system messages, attachments, or Thinking traces. Its request limit defaults to one and can be set from one to three independently of the maximum search-result count used by search engines.

## Web ON authorization and safety

Web ON + Send permits local routing of the current input and creates one web turn only when needed; an explicit search command bypasses routing, while ordinary conversation remains local. That turn may create from one to three ChatGPT Web requests up to the configured limit, and each job receives a distinct, non-reusable authorization. A stopped or dropped pending job is removed from the queue and waiters so it cannot block the next turn. An unknown outcome is not retried for the same job. Search output is untrusted context, not the final answer; local Ollama generates that answer. Login, account changes, purchases, deletions, and other destructive/account actions still require separate user confirmation.

## Installation responsibility

The app detects the macOS default browser and supports Brave and Chrome initially. It materializes the extension under Taceta Application Support, registers the per-user Native Messaging Host, verifies version and fixed ID `hefhkgbiiajifedgjlbiklclooifkidg`, and opens the extension-management page. The user turns on Developer mode and chooses Load unpacked / Add for the materialized `browser-extension` directory. This final browser approval remains manual; it is not silently automated. Updates guide the user to press Reload. Safari and other unsupported browsers are directed to install Brave or Chrome and make one the default; no registration is attempted for an unsupported browser.

## Grok connection and Agent execution

`backend/grok` owns the public Grok Build OAuth flow, Taceta-only Keychain storage, token refresh, model discovery and streaming inference. Sign-in starts explicitly in Settings and uses PKCE, state and a temporary loopback callback. Model metadata selects Chat Completions or Responses; failures do not select another service. Taceta acceptance of the public client ID and account eligibility require live service confirmation. Sending with Grok selected uses cloud inference and sends conversation and required task content to xAI.

For the OAuth proxy's compatibility check, requests send `x-grok-client-version: 1.0.24`, the Grok Build release used as this adapter's implementation reference. This follows the [reference commit's version declaration](https://github.com/xai-org/grok-build/blob/37949780c144e37df692e3d669051a21fec24f20/crates/codegen/xai-grok-version/Cargo.toml) and [public sampler header](https://github.com/xai-org/grok-build/blob/37949780c144e37df692e3d669051a21fec24f20/crates/codegen/xai-grok-sampler/src/client.rs#L554-L608). User-Agent and client identifier identify Taceta itself. The compatibility value stays pinned in source and does not follow release updates without corresponding protocol support. HTTP 426 reports a compatibility upgrade requirement; it does not establish that Taceta or the account is prohibited.

`agent` owns task execution within Taceta and connects to Ollama or Grok through `AgentModel`. The UI supplies workspace, model, instructions and limits, and displays progress, approval requests and saved state. Reads, edits, search and commands are bound to the chosen workspace. Each edit and command requires its own approval. Commands may write only to that workspace and their scratch area, with networking denied. Cancellation, time limits and app shutdown terminate child processes. Taceta Link and external CLIs do not own this execution path.

## Compaction and resumption

An append-only event journal is separate from the current model input. Goals, user instructions and structured work state remain independent of summaries; Thinking traces are not replayed. Compaction occurs between complete tool-call/result groups, using the model context limit and returned usage while reserving space for summarization and the next response. Manual compaction uses the same path.

A successful checkpoint stores the summary, retained history ranges, original event boundary, window and previous-window IDs, model and context length. Restart reconstructs state from the latest checkpoint plus later events. History search and reads retain access to original events. Incomplete or failed summaries do not replace the checkpoint. Mandatory input that cannot fit causes an explicit stop instead of silent truncation. Edits and commands with unknown outcomes are not automatically repeated.

Summaries use regular model inference and persist portable text and structured state across model and provider changes. Taceta does not assume that the OAuth proxy supports a dedicated compaction API or Codex-specific opaque compaction items. The public Codex implementation informs the design; equivalence with its service-side compaction or model quality on long tasks is unverified.
