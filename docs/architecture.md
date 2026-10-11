# Taceta architecture boundary

この文書は、Taceta の Rust アプリと Taceta Link の責務、そして Web Search の外部作用を定義します。Taceta は単一 Git repo / version 内に、Rust app `src/` と独立 component `browser-extension/` を物理的に持ちます。

## アプリとローカル経路

`InferenceBackend` はOllamaの通常チャットの境界です。モデル、会話入力、添付、Thinking 設定、Web Search 設定を受け取り、Thinking delta、content delta、検索進捗、参照元、完了、失敗を返します。Ollama固有の通信形式はそのアダプターが所有します。Thinking trace は次の入力へ混ぜません。

Ollamaの通常チャットでは、Web Search OFF で外部検索 request を作りません。ON では過去の履歴を除いた現在入力だけをローカルの構造化ルーターへ渡し、`search_current` または `search_generated` を必ず選ばせます。検索を省く判定はなく、通常会話、普遍的な説明、創作も外部調査の対象です。現在の質問そのものを調べる入力は `search_current` とし、Taceta はモデルが書き換えた検索語ではなく利用者の原文を検索します。モデルの古い知識と矛盾する名前や前提も検索せず否定しません。質問自体をモデルに作らせてから検索する入力は `search_generated` です。明示的な検索命令はルーターを迂回して必ず検索します。自由文、拒否、不正 JSON は内部知識による回答へ戻さず、外部未送信の route error として停止します。検索時は設定された executor だけを適用し、外部結果は untrusted context としてローカル Ollama の最終回答に渡します。provider は暗黙に切り替えません。 取得したページ本文は資料であり会話ではないため、1回の回答で読む量をコンテキスト長の半分までに抑えます（残りの取得回数で等分し、使わなかった分は後のページへ回します）。割り当てに収まらないページは、切らずに、回答中のモデルがページ全体を順に読んで質問に沿った要約を作り、要約であることを添えて渡します。要約できなかったページは一部だけを渡すことはせず、取得失敗として扱います。

Taceta Link は同じ version を持つ MV3 拡張、Native Messaging Host `org.mlabo.taceta.link`、user-only Unix socket で構成します。アプリが job を socket へ置き、拡張が poll して実行結果を返します。product version / protocol version / extension ID の不一致は fail-closed です。Cookie、token、profile、local storage を読み出したり輸出したりしません。

ブラウザー executor は focused を優先して既存の normal window を作業コンテナとして再利用し、その中に非アクティブな agent tab / group を作成します。normal window がない場合だけ非フォーカス window を作成します。window は所有・削除せず、終了時は追跡した exact agent tab を ungroup して削除します。Google Search は query を渡します。直接検索では、ChatGPT Web の1回目に現在のuser promptをexactに渡します。ローカルモデルが具体的な検索質問を作った場合は、現在のpromptをauthorityとして保持したenvelopeにその質問を加えます。利用者が2〜3回を明示許可した場合だけ追加調査を行い、誤った固有名詞、version、前提を訂正するよう指示します。Tacetaが過去の履歴、system message、attachments、Thinking traceを直接付加することはありません。ChatGPT Webの逐次回答と引用URLはTacetaへ戻します。質問回数上限は既定1、設定可能範囲1〜3です。

## Web ON の承認と安全境界

Web ON + Send は現在入力をローカルで判定し、その入力に対する一つの Web turn を必ず作ります。明示検索は判定を迂回します。通常会話も外部調査を省きません。ChatGPT Web の turn で作成できる request は設定上限の1〜3件までで、各 job は再利用できない個別の authorization を持ちます。停止またはdropされた未完了jobはqueueとwaiterから取り除き、次のturnへ残しません。結果不明状態の同一jobは再試行しません。検索結果は最終回答そのものではなく、ローカル Ollama が生成する回答の untrusted context です。ログイン、アカウント変更、購入、削除などの destructive/account action は別途利用者の確認が必要です。

## インストール責務

アプリは macOS のデフォルトブラウザーを検出し、初期対応の Brave / Chrome に限って、拡張を Taceta Application Support 配下へ materialize します。Native Messaging Host をそのユーザー専用のブラウザー領域へ登録します。この materialize と登録は起動のたびに行います。登録の前に version の一致を確認し、登録する manifest は固定 ID `hefhkgbiiajifedgjlbiklclooifkidg` だけを許可します。固定 ID が manifest の key から導かれる値と一致することは、テストで検証します。拡張管理ページは自動では開きません。利用者は Developer mode を ON にし、Load unpacked / Add で materialized `browser-extension` directory を選びます。この最後の browser approval は自動化しません。更新時は拡張管理ページで Reload を案内します。Safari 等は Brave / Chrome の導入とデフォルト設定へ案内し、未対応ブラウザーへの登録は行いません。

## GrokとClaudeのCLI実行

`cli_agent` は、Tacetaが子プロセスとして動かすコーディングCLIに対する一つの境界です。各CLIが自分のセッション、ツール、文脈の管理（コンパクションを含む）を所有し、Tacetaは依頼を渡し、進捗を表示し、承認の求めに答え、実行を止めます。`InferenceBackend` と `AgentModel` には組み込みません。

`CliAgent` トレイトは、名前のほかに次の3つを持ちます。`readiness` はCLIの場所とversion、およびCLIが起動せずに答えられる場合はサインイン済みかどうかを返します。`models` は選べるモデルと推論の強さを返します。`run` は一つの依頼を最後まで実行します。`run` には型付きの `RunRequest`（続けるセッションID、作業フォルダー、モデル、推論の強さ、依頼文、`ApprovalPolicy`、時間上限）を渡し、`Event`（セッションの確定、回答とThinkingのdelta、操作、承認要求とその解消、使用量）を受け取り、`Control` で承認に答え、`RunOutcome` を受け取ります。返されたfutureを破棄するか、cancelを立てると、CLIとそれが開始した処理を止めます。

`cli_agent::process` は共通の子プロセス処理です。実行ファイルをPATHと各CLIの通常のインストール先から探し、利用者自身の環境のまま作業フォルダーをcwdとして起動し、標準入出力で1行1個のJSONをやり取りします。子プロセスのプロセスグループを所有し、停止または破棄でCLIとそれが開始したツールのプロセスを終了します。

ドライバーは `cli_agent::claude`（`claude -p` のstream-json）と `cli_agent::grok`（`grok agent stdio` のAgent Client Protocol）の2つです。各CLIのwire形式とrequest IDを知るのはドライバーだけで、外には上の型だけが出ます。どちらも無改変の公式CLIを利用者の通常のログインと設定で動かし、サインインを行わず、認証情報を扱いません。Claudeのドライバーが `claude auth status` の出力から読むのは、サインイン済みかどうかの真偽値だけです。Grokのドライバーはサインイン状態を問い合わせません。

`ApprovalPolicy::AskEveryTime` では、ファイルを変更する操作とコマンドの実行をすべてTacetaへ問い合わせます。Claudeは `--permission-mode manual` で起動します。Grokには、ツール実行前のフックで確認を求めるプラグインを `--plugin-dir` で読み込ませます。プラグインは `~/Library/Application Support/Taceta/GrokApprovalPlugin` に書き出し、読み取り専用のツールは確認なしで通します。承認は1回ごとに答え、「常に許可」にあたる選択肢は返しません。`ApprovalPolicy::FollowCli` ではCLI自身の権限設定が判断し、その設定が確認を求めたときだけTacetaに届きます。 `ApprovalPolicy::NoTools` は会話だけのターンです。Claudeは `--tools "" --strict-mcp-config` で道具なしに起動し、Grokには `--plugin-dir` で、すべての道具の呼び出しに `deny` を返すフックのプラグイン（`GrokApprovalPluginNoTools`）を読み込ませます。それでも届いた承認要求は `app/cli_ui` が拒否します。会話ごとの `cli_chat` がこの方針を選び、CLIはTaceta専用の空のフォルダーで動きます。要約（`app/compaction`）もこの方針で実行します。

`prior_context` は、それまでの会話を、まだ見ていないエンジンへ渡すテキストに書き出します。ユーザーとアシスタントの発言、テキストの添付、完了した操作（成功・失敗の別と、1件あたり先頭40行までの差分）を含めます。systemメッセージ、Thinking trace、途中で切れた回答は含めません。画像の添付は名前だけを記します。先頭には、これは同じ会話の経緯であって新しい依頼ではないことを書きます。収めるために発言を省くことはありません。`plan` は、文字数の上限に収まるならそのテキストを返し、収まらないなら要約の仕事（`CompactionJob`）を返します。要約の仕事は、新しい発言を上限の半分まで原文のまま残し、それより前を、保存済みの要約の終わりから順に、上限の半分以下の塊に分けたものです。1件が塊より大きい発言は文字単位で分け、1文字も捨てません。

`app/cli_ui` はこの作業画面を所有します。会話のメッセージが記録の正本で、CLIのセッションはその会話に対するCLI側の作業文脈です。会話は接続先ごとに `CliSession`（セッションIDと、そのセッションがすでに持っている会話メッセージの件数）を保存します。送信時は、その件数より後のメッセージを `prior_context::plan` に渡し、返ったテキストを依頼の前に付けます。セッションIDは通知された時点で保存します。実行が終わると、完了または失敗した操作を `ActionRecord` としてその回答のメッセージに保存し、完了しなかった回答には途中で切れた印を付け、本文も操作もない回答は残さず、件数を現在のメッセージ数に更新します。作業フォルダーを変更すると、その会話のCLIセッションの記録を消します。GPTも同じ件数を `GptConversation` に持ち、同じ手順でthreadへ渡します。

`app/compaction` は要約を所有します。上限は選択モデルのコンテキスト長（通知がない場合は200,000トークンとみなす）の半分を文字数としたものです。`plan` が要約を求めたら、これから答える接続先（Ollamaは選択中のモデル、CLIとGPTは要約専用の新しいセッション）に塊を古い順に渡して1つの要約へ畳み込み、`Summary`（本文と、何件目までを表すか）を会話に保存してから、待たせていた送信をやり直します。その間、入力欄の内容は取り出さず、会話のメッセージは変更しません。要約の失敗、中止、要約しても収まらない場合は、何も送りません。要約中の承認要求はすべて拒否します。

Ollamaの通常チャットはセッションを持たないため、既読0件として同じ `plan` で収まるかを判定し、要約があれば「要約＋それより後のメッセージ」を、なければ会話のメッセージ全体を入力にします。Taceta自身の作業実行は、実行を始めるたびに、作業記録にまだない発言とテキストの添付を取り込みます。作業モードでOllamaを選んでいる間だけ、作業記録のスナップショットを会話のメッセージに書き戻し、作業記録に入らない発言（他の接続先の途中で切れた回答など）はその後ろに残します。それによって位置がずれた場合は、他の接続先の既読件数と要約の範囲を、ずれていない位置まで戻します（同じ発言を二度渡すことはあっても、飛ばすことはありません）。作業記録のメッセージ一覧に件数の上限はありません。

`agent` がTaceta自身の作業実行を所有し、`AgentModel` を介してOllamaへ接続します。UIは作業フォルダー、モデル、指示、上限を渡し、進捗・承認要求・保存済み状態を表示します。ファイル読み取り、編集、検索、コマンド実行は作業フォルダーに束縛され、編集とコマンドには個別の承認が必要です。コマンドの書き込み先は作業フォルダーと専用一時領域に限り、ネットワークは許可しません。停止・時間上限・アプリ終了では子プロセスも終了します。Taceta Linkと外部CLIはこの実行経路を所有しません。

## GPTとCodexの作業実行

`gpt::GptClient` は独立したCodex App Serverの接続を所有します。`InferenceBackend` や一回のモデル推論を表す `AgentModel` には組み込みません。Codexが推論、ツール実行、承認判断、会話履歴、コンパクションを担当し、`app/gpt_ui` がユーザーの入力、作業フォルダー、モデル選択、表示、承認回答、停止を扱います。UIには型付きの `GptRunRequest`、`GptEvent`、`GptControl` を渡し、JSON-RPCのwire形式とrequest IDは接続内に留めます。

公式の既存Codex CLIを `app-server --listen stdio://` で起動します。Taceta専用の `CODEX_HOME` を使い、OAuthの開始・完了・キャンセル・解除は公開account APIに委ねます。トークンをTaceta自身で読み出さず、Codexのkeyring保存を使用します。別のCodex環境の設定・認証・会話をコピーしません。クライアントの構築だけでは認証、プロセス起動、外部リクエストを開始しません。

GPTの会話は返されたthread IDに結び付け、UIが保存した確認を接続側へ返してからturnを開始します。Codex履歴をTacetaの `AgentSession` に再構築せず、次のturnには今回の入力と、そのthreadがまだ受け取っていない会話のメッセージ（`prior_context`）を送り、再起動後は `thread/resume` で続行します。モデルの変更は同じthreadで可能です。GPTのモード・作業フォルダーを変更すると、thread IDを退避して同じ会話のまま新しいthreadで続けます。Tacetaから削除したGPT会話のCodex原文は専用領域に残します。

Ollamaの作業からGPTへの移管は `AgentSession::export_handoff` が担当します。対象は、接続先がOllamaで、Taceta自身の作業モードにあり、作業フォルダーと履歴を持ち、まだGPTのthreadがない会話です。停止中の同一作業・フォルダーを確認し、元のユーザー指示と訂正を順序通りに保持し、構造化された作業状態、保存済み要約、現在の完全な会話・ツール結果の組、未解決の操作と以前の原文記録の場所を渡します。Thinking、途中で切れた回答、承認は移しません。UIは同じTaceta会話IDを維持し、最初のCodex入力に移管資料をデータとして加えます。元の `AgentSession` は書き換えず、以後の作業はCodexが所有します。初回送信に失敗した場合はCodexに保存済みの入力で移管済みか判断し、同じ資料を重複送信しません。

上の移管以外の切り替えでは、接続先がGPTとそれ以外の間で変わっても同じ会話が続き、受け取っていない発言を `prior_context` で渡します。

コーディングには `workspace-write`、`untrusted`、ユーザー承認を使い、コマンドのネットワーク接続を禁止します。Codexが要求したコマンド・ファイル変更の承認は、一度だけ許可または拒否します。通常チャットは空の専用cwdとread-only、承認never、実行環境なしで開始し、ファイル・コマンドのツールを提供しません。時間とツール操作通知を基準に中断し、未完了の停止を成功に変換しません。操作通知による上限はCodex内部の推論回数や並行実行済み操作を厳密に制限するものではありません。

回答はitem単位に逐次表示し、確定itemで置き換えます。完了の根拠は対応するturnの終端通知です。接続切断やEOFを完了とみなしません。Thinking通知は表示専用で、Tacetaが次の入力へ追加することはありません。Codex内部の会話管理と推論状態はCodexが所有します。

## Ollamaのコンパクションと再開

元の出来事を保存する追記式の記録と、現在モデルに渡す入力を分離します。目的、ユーザー指示、構造化した作業状態は要約から独立して保持し、Thinking traceを後続入力へ戻しません。圧縮はツール呼び出しと結果の組が完了した境界で行い、モデルの入力上限と返却使用量を基準に、要約と次の出力の余白を確保します。手動圧縮も同じ処理を使用します。

成功した要約、保持履歴の範囲、元記録の境界、window IDと前window ID、使用モデル、context長を再開地点に保存します。再起動は最新の再開地点と後続記録から復元し、元の記録は履歴検索と読み取りで参照できます。要約が未完了・失敗の場合は再開地点を置き換えません。入力上限に必須情報が収まらなければ、切り捨てず停止します。結果不明の編集やコマンドは自動で再実行しません。

通常のモデル推論で要約を作り、モデルを越えて扱えるテキストと構造化状態として保存します。Codex固有の不透明な圧縮項目は使いません。Codexの公開実装を設計上の参考としていますが、サービス側の圧縮や長時間作業のモデル品質との同等性は未確認です。

---

# Taceta architecture boundary (English)

This document defines the responsibilities of Taceta's Rust app and Taceta Link, plus the external-effect boundary for Web Search. One Git repository and product version contain two physically separate components: the Rust app in `src/` and the independent component in `browser-extension/`.

## App and local transport

`InferenceBackend` owns regular Ollama chat. It accepts model, conversation input, attachments, Thinking settings, and Web Search settings, then emits Thinking deltas, content deltas, search progress, citations, completion, and failure. Ollama's wire format is owned by its adapter. Thinking traces never enter the next input.

In regular Ollama chat, Web Search OFF creates no external search request. When it is ON, a local structured router receives only the current input, never conversation history, and must choose `search_current` or `search_generated`. Skipping research is not allowed, so timeless explanation, writing, and casual conversation are searched too. An existing user question uses `search_current`, and Taceta searches that original input rather than a model-written rewrite. A name or premise that conflicts with old model knowledge must be verified rather than denied. A request to have the model formulate a question before searching uses `search_generated`. An explicit search command bypasses the router and always searches. Free text, refusal, or invalid JSON stops as a route error before anything is sent externally and never falls back to an answer from internal knowledge. The configured executor is used without provider fallback, and external output is untrusted context for a final answer generated locally by Ollama. Fetched page text is source material, not conversation, so one answer reads at most half the context length of it: the allowance is divided among the fetches still permitted, and what a page does not use stays for later pages. A page larger than its allowance is not cut: the answering model reads the whole page in order and writes a summary for the question, which is handed over marked as a summary. A page that cannot be summarized is not passed on in part; it is reported as a failed fetch.

Taceta Link consists of a same-version MV3 extension, Native Messaging Host `org.mlabo.taceta.link`, and a user-only Unix socket. The app places jobs on the socket; the extension polls and returns results. Product version, protocol version, or extension-ID mismatch fails closed. Cookies, tokens, profiles, and local storage are never read or exported.

The browser executor prefers an existing focused normal window as its route container, creating an inactive agent tab and group there; only when no normal window exists does it create a non-focused normal window. The window is never owned or closed. At session end it ungroups and removes only the exact tracked agent tab. Google Search receives a query. For a direct search request, ChatGPT Web receives the current user prompt exactly on the first request. When the current prompt asks the local model to formulate the question first, or the model itself produces the first concrete `web_search` query, that first request instead contains an anchored envelope with the current prompt and the concrete query. Only when the user explicitly allows two or three requests do later requests attach another local-model query as an unverified research angle and instruct ChatGPT to correct mistaken names, versions, and premises. Taceta does not directly attach earlier conversation history, system messages, attachments, or Thinking traces. Its request limit defaults to one and can be set from one to three independently of the maximum search-result count used by search engines.

## Web ON authorization and safety

Web ON + Send routes the current input locally and always creates one web turn for that input. An explicit search command bypasses routing. Ordinary conversation is not exempt from external research. That turn may create from one to three ChatGPT Web requests up to the configured limit, and each job receives a distinct, non-reusable authorization. A stopped or dropped pending job is removed from the queue and waiters so it cannot block the next turn. An unknown outcome is not retried for the same job. Search output is untrusted context, not the final answer; local Ollama generates that answer. Login, account changes, purchases, deletions, and other destructive/account actions still require separate user confirmation.

## Installation responsibility

The app detects the macOS default browser and supports Brave and Chrome initially. It materializes the extension under Taceta Application Support, and registers the per-user Native Messaging Host. Materialization and registration run at every start. Versions are checked before registration, and the registered manifest allows only the fixed ID `hefhkgbiiajifedgjlbiklclooifkidg`. That this ID is the one derived from the manifest key is verified by a test. The extension-management page is not opened automatically. The user turns on Developer mode and chooses Load unpacked / Add for the materialized `browser-extension` directory. This final browser approval remains manual; it is not silently automated. Updates guide the user to press Reload. Safari and other unsupported browsers are directed to install Brave or Chrome and make one the default; no registration is attempted for an unsupported browser.

## Grok and Claude CLI execution

`cli_agent` is one boundary over the coding CLIs that Taceta drives as child processes. Each CLI owns its session, tools and context management, including compaction. Taceta supplies the request, shows progress, answers approval requests and stops the run. This is outside `InferenceBackend` and `AgentModel`.

Besides a name, the `CliAgent` trait has three operations. `readiness` returns the CLI's location and version and, where the CLI can answer without starting a session, whether it is signed in. `models` returns the selectable models and their reasoning efforts. `run` carries one request to its end. `run` takes a typed `RunRequest` (session ID to continue, workspace, model, effort, prompt, `ApprovalPolicy`, time limit), emits `Event` values (session ready, answer and Thinking deltas, actions, approval requests and their resolution, usage), accepts approval answers as `Control`, and returns a `RunOutcome`. Dropping the returned future or setting cancel stops the CLI and everything it started.

`cli_agent::process` is the shared child-process helper. It finds the executable on PATH and in the CLIs' usual install locations, starts it with the user's own environment and the workspace as cwd, and exchanges one JSON value per line on stdio. It owns the child's process group; stopping or dropping it ends the CLI and the tool processes it started.

There are two drivers: `cli_agent::claude` (`claude -p` with stream-json) and `cli_agent::grok` (`grok agent stdio` over the Agent Client Protocol). Only a driver knows its CLI's wire format and request IDs; nothing but the types above leaves it. Both run the unmodified official CLI with the user's normal login and settings, perform no sign-in and handle no credentials. The Claude driver reads only the signed-in boolean from `claude auth status`. The Grok driver does not ask about sign-in.

With `ApprovalPolicy::AskEveryTime`, every action that changes files or runs a command is sent to Taceta. Claude is started with `--permission-mode manual`. Grok is given, through `--plugin-dir`, a plugin whose pre-tool hook asks; the plugin is written to `~/Library/Application Support/Taceta/GrokApprovalPlugin` and lets read-only tools through without asking. Each approval is answered once, never with an "always" option. With `ApprovalPolicy::FollowCli`, the CLI's own permission settings decide, and Taceta is asked only when those settings ask. `ApprovalPolicy::NoTools` is a conversation-only turn. Claude is started without tools (`--tools "" --strict-mcp-config`), and Grok is given, through `--plugin-dir`, a plugin whose hook answers `deny` to every tool call (`GrokApprovalPluginNoTools`). An approval request that still arrives is refused by `app/cli_ui`. A conversation's `cli_chat` selects this policy, and the CLI runs in an empty folder of Taceta's own. Summaries (`app/compaction`) run under this policy too.

`prior_context` writes the conversation so far as text for an engine that has not seen it. It includes user and assistant turns, text attachments, and finished actions (done or failed, with the first 40 lines of each diff). System messages, Thinking traces and interrupted answers are excluded. Image attachments are named but not included. A header states that this is the earlier part of the same conversation and not a new request. Nothing is left out to make it fit. `plan` returns the text when it fits the character budget, and otherwise a `CompactionJob`: the newest messages stay verbatim within half the budget, and everything before them, starting where the stored summary ends, is cut into pieces of at most half the budget. A message larger than a piece is split by characters, and no character is discarded.

`app/cli_ui` owns this task view. The conversation's messages are the record; a CLI's session is that CLI's working context for the conversation. A conversation stores one `CliSession` per provider: the session ID and how many of the conversation's messages that session already holds. On send, the messages after that count go through `prior_context::plan` and the returned text is placed before the request. The session ID is saved as soon as it is announced. When the run ends, finished and failed actions are stored as `ActionRecord` values on that answer's message, an answer that did not complete is marked interrupted, an answer with neither text nor actions is not kept, and the count is set to the current number of messages. Changing the workspace clears the conversation's CLI sessions. GPT keeps the same count in `GptConversation` and hands turns to its thread the same way.

`app/compaction` owns summarizing. The budget is half the selected model's context length, counted in characters, assuming 200,000 tokens when none is reported. When `plan` asks for a summary, the engine about to answer (the selected model for Ollama; a new summary-only session for a CLI or GPT) receives the pieces oldest first and folds them into one summary. The `Summary` (its text and how many messages it stands for) is stored on the conversation, and the waiting send is then started again. Meanwhile the draft is not taken and the conversation's messages are not changed. If summarizing fails, is stopped, or the conversation still does not fit, nothing is sent. Approval requests during summarizing are all denied.

Regular Ollama chat keeps no session, so it runs the same `plan` with nothing seen to decide whether the conversation fits, and its input is the summary plus the messages after it, or all messages when there is no summary. Taceta's own task execution imports, each time a run starts, the turns and text attachments not yet in its task record. Only while Ollama is selected in task mode is a task-record snapshot written back to the conversation's messages; messages that never enter the record (such as another engine's interrupted answer) are kept after it. When that moves them, the counts other engines hold and the summary's range fall back to the last position that still holds, so a turn may be handed over twice but is never skipped. The task record's message list has no upper limit.

`agent` owns task execution within Taceta and connects to Ollama through `AgentModel`. The UI supplies workspace, model, instructions and limits, and displays progress, approval requests and saved state. Reads, edits, search and commands are bound to the chosen workspace. Each edit and command requires its own approval. Commands may write only to that workspace and their scratch area, with networking denied. Cancellation, time limits and app shutdown terminate child processes. Taceta Link and external CLIs do not own this execution path.

## GPT and Codex execution

`gpt::GptClient` owns the independent Codex App Server connection, outside `InferenceBackend` and the single-inference `AgentModel` interface. Codex owns inference, tool execution, approval decisions, history and compaction. `app/gpt_ui` owns user input, workspace and model selection, presentation, approval responses and stopping. The boundary uses typed `GptRunRequest`, `GptEvent` and `GptControl`; wire JSON-RPC and request IDs remain in the adapter.

The already-installed official Codex CLI runs as `app-server --listen stdio://` with Taceta's own `CODEX_HOME`. Public account methods manage OAuth, cancellation and logout. Codex uses keyring storage; Taceta does not extract tokens or copy another Codex environment's credentials, settings or history. Client construction starts no authentication, process or external request.

The UI durably saves a returned thread ID and acknowledges it before the next turn starts. Subsequent turns send the new input and the conversation's messages the thread has not yet received (`prior_context`); restart uses `thread/resume`. Codex history is not reconstructed inside Taceta's `AgentSession`. Models can change within a thread. Changing a GPT thread's mode or workspace sets the thread ID aside and continues the same conversation in a new thread. Deleting a GPT conversation from Taceta retains original Codex history in the isolated home.

`AgentSession::export_handoff` owns an explicit Ollama-to-GPT task transfer. It applies to a conversation whose provider is Ollama, in Taceta's own task mode, with a workspace and history and no GPT thread yet. It locks the inactive task, verifies the same workspace, preserves ordered user requests and corrections, and exports structured work state, saved summary, complete current message/tool-result groups, unresolved effects and the exact original-record directory. Thinking, interrupted prose and approvals are excluded. The UI keeps the same Taceta conversation ID and supplies the transfer as data in the first Codex input. The original journal remains unchanged; Codex owns subsequent work. A retry consults saved Codex input to distinguish an empty thread from an already-delivered transfer, preventing duplicate delivery.

Apart from that transfer, a switch between GPT and any other provider continues the same conversation, and turns the receiving engine has not seen are handed over through `prior_context`.

Coding uses `workspace-write`, `untrusted` and user approval, with command network access disabled. Codex-requested approvals are accepted once or denied. Ordinary chat uses an empty cwd, read-only sandbox, never approval and no execution environment, so it has no filesystem or command tools. Time and tool-action notifications trigger interruption. Interrupted or limited runs remain distinct from success; notification thresholds do not strictly cap internal inference calls or already-running parallel actions.

Assistant messages stream by item and are replaced by their final item text. Only the matching turn's terminal notification establishes its outcome; disconnect or EOF is not completion. Thinking notifications are for display and are never added to new inputs by Taceta. Codex owns its internal conversation and reasoning state.

## Ollama compaction and resumption

An append-only event journal is separate from the current model input. Goals, user instructions and structured work state remain independent of summaries; Thinking traces are not replayed. Compaction occurs between complete tool-call/result groups, using the model context limit and returned usage while reserving space for summarization and the next response. Manual compaction uses the same path.

A successful checkpoint stores the summary, retained history ranges, original event boundary, window and previous-window IDs, model and context length. Restart reconstructs state from the latest checkpoint plus later events. History search and reads retain access to original events. Incomplete or failed summaries do not replace the checkpoint. Mandatory input that cannot fit causes an explicit stop instead of silent truncation. Edits and commands with unknown outcomes are not automatically repeated.

Summaries use regular model inference and persist portable text and structured state across model changes. Codex-specific opaque compaction items are not used. The public Codex implementation informs the design; equivalence with its service-side compaction or model quality on long tasks is unverified.
