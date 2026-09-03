# Taceta

静かに考え、手元で答える。Taceta は、Ollama をバックエンドに使う macOS 専用のローカル推論クライアントです。Rust と `eframe` / `egui` で構築されています。

Taceta Link は、ログイン済みブラウザーで行う検索や ChatGPT Web とのやり取りを Taceta から明示的に開始できる独立した Manifest V3 拡張です。Taceta と Taceta Link は OpenAI、Ollama、Brave、Google の公式製品ではありません。

## 主な機能

- Ollama モデルのストリーミング回答
- Thinking の実行設定と trace 表示の独立制御
- UTF-8 テキスト添付、および vision 能力を確認できたモデルへの画像添付
- 日本語 / 英語、System / Light / Dark、文字サイズ 10–32 の保存
- Ollamaの `OLLAMA_HOST` 設定とport変更への自動追従、および手動接続先
- 見出し、装飾、コード、引用、リスト、タスク、リンク、表、脚注などの Markdown 表示
- 会話ごとの Web Search（既定は OFF）
- Brave Search / Ollama Web Search API、または Taceta Link 経由のブラウザー検索、Google 検索、ChatGPT Web

Web Search が OFF のときは外部リクエストを作りません。ON でも通常会話をすべて検索へ送ることはありません。現在の入力に明示的な検索命令、現在性・最新性や特定日時点の確認、出典 URL の要求など明白な検索意図がある場合だけ、Taceta は回答前に最低1回の検索を完了させます。過去の会話履歴は検索意図の判定に使いません。通常会話や曖昧な入力では検索を強制せず、ローカルモデルが必要と判断した場合に限って従来どおり `web_search` を呼び出せます。さらに、ローカルモデルが tool call を返さず、現在の入力に対して直ちに検索する意思だけを通常テキストで明言した場合は、その予告文を最終回答として表示せず、同じ入力を1ターンにつき1回だけ検索へ回します。検索についての説明、過去形、否定、質問、通常会話はこのフォールバックの対象外です。検索時は選択された executor だけを使い、検索結果やブラウザーの回答は untrusted context としてローカル Ollama の最終回答に渡します。ChatGPT Web への質問回数は既定1回、設定可能範囲は1〜3回です。Google等の最大検索結果とは独立しており、設定した上限を超える追加質問は行いません。上限到達後（最大設定なら4回目以降）の `web_search` 要求は ChatGPT Web へ送らず、それまでに取得した最大3回答をローカルモデルが最終回答へ統合します。

## 画面

### 実動作

ローカルモデルの回答、Thinking trace、Markdown の表や引用、Web Search の状態を同じ画面で確認できます。

![Tacetaの日本語チャット画面](docs/images/taceta-chat-ja.png)

### 設定

表示言語、テーマ、モデル管理、Web Search、モデル保存先、context length を設定できます。

![Tacetaの日本語設定画面](docs/images/taceta-settings-ja.png)

## 構成とデータフロー

```text
Taceta (Rust/egui)
  ├─ Web Search OFF ───────────────→ Ollama (resolved endpoint)
  ├─ Brave / Ollama Web Search API ─→ 外部検索 → Ollama (最終回答)
  └─ Taceta Link ──────────────────→ Brave / Chrome
                                      └─ 検索または ChatGPT Web
                                         → Native Messaging
                                         → Taceta → Ollama (最終回答)
```

Taceta Link は `browser-extension/` の MV3 拡張、Native Messaging Host `org.mlabo.taceta.link`、ユーザー専用 Unix socket で構成されます。拡張は既存の通常ブラウザーウィンドウを優先して作業用 tab / group を作り、Taceta が所有する exact tab / group だけを追跡します。ブラウザーのウィンドウ全体を閉じることはありません。製品 version、protocol version、固定 extension ID が一致しない場合は fail-closed します。

ChatGPT Web 経路の1回目は現在の入力欄のpromptを正確に送ります。設定で2〜3回を明示的に許可した場合だけ、追加分では選択中のローカルモデルが各 `web_search` tool call で生成した検索クエリを「未検証の追加調査案」として元のpromptに添えます。固有名詞、version、前提に誤りがあれば訂正するようChatGPTへ明記します。画面の「ローカルモデル案」はTacetaが追加質問の出所を示す一時的な進捗表示であり、ChatGPTの内部思考や会話履歴ではありません。Tacetaは会話履歴、system message、添付ファイル、Thinking trace、Cookie、token、profile、localStorage を直接送信・取得・保存しません。ChatGPT Web の出力は逐次的に受信しますが、最終回答の生成はローカル Ollama が担当します。

## セキュリティとプライバシー

- 通常のチャットと会話履歴はこの Mac のローカルアプリケーションデータに保存します。生成に必要な会話内容と添付は、現在設定されているOllama接続先だけへ送ります。
- Ollamaの既定接続先は `http://127.0.0.1:11434` です。TacetaはOllamaの設定を自動解決でき、Ollamaとモデルは同梱・再配布しません。
- Web Search を有効にした場合だけ、設定した検索先へ query、または選択した Web executor の request が送られます。送信前に画面で確認できます。
- API key が必要な検索 provider の key は macOS Keychain に保存します。Cookie やブラウザーの認証 token を読み出したり、エクスポートしたりしません。
- Taceta Link の `tabs`、`tabGroups`、`scripting`、`debugger`、`nativeMessaging`、HTTPS host access などの権限は、作業経路と検索ページを扱うために必要です。拡張は Taceta が追跡している tab 以外を対象にしない設計です。
- ログイン、アカウント変更、購入、削除などの破壊的またはアカウント操作は自動実行しません。

Taceta Link は OpenAI / ChatGPT の公式拡張ではなく、ChatGPT Web の DOM を使う非公式・実験的な連携です。利用するアカウント、Web サービス、ブラウザーの規約と管理者ポリシーを確認したうえで利用してください。UI やサービス条件の変更により、この経路は動作しなくなる可能性があります。

## 必要環境

- macOS 13.0 以降（Apple Silicon を主対象）
- Rust 1.92 以降（ソースからビルドする場合）
- [Ollama](https://ollama.com/) を別途インストールして起動
- Taceta Link を使う場合は Brave または Chrome

モデルの取得・削除は Model Manager から利用者が明示的に行います。モデル、Ollama、ブラウザー、検索 API、ChatGPT Web の利用条件は、それぞれの提供元に従います。

## Ollama接続先

既定の「自動」モードでは、Tacetaは接続時に次の順序でOllamaの接続先を解決します。

1. macOSの `launchctl` に設定された `OLLAMA_HOST`
2. Tacetaプロセス自身の `OLLAMA_HOST`
3. 最終フォールバックの `http://127.0.0.1:11434`

Ollamaが `0.0.0.0` または `::` で待ち受ける設定は、Ollama公式の接続用変換と同様に `127.0.0.1` または `::1` へ変換します。接続操作と接続失敗後の再確認で設定を読み直すため、Tacetaの起動後に `launchctl` のportを変更した場合も追従できます。

`OLLAMA_HOST=127.0.0.1:23456 ollama serve` のように、一つのシェルだけへ設定して起動したOllamaのportは、別のGUIアプリから取得できる公式APIがありません。その場合は設定画面で「手動」を選び、接続先を指定してください。既定フォールバック以外の接続先が未到達でも、Tacetaは `11434` へ勝手に戻りません。また、誤ったportで別のOllamaを起動しないよう、Tacetaからの自動起動は既定フォールバック時だけ行います。

Ollamaの公式仕様は [API Base URL](https://docs.ollama.com/api/introduction#base-url)、[macOSの環境変数設定](https://docs.ollama.com/faq#setting-environment-variables-on-mac)、[ネットワーク公開設定](https://docs.ollama.com/faq#how-can-i-expose-ollama-on-my-network) を参照してください。

## ソースからビルドして使う

```sh
git clone https://github.com/mlabo-org/taceta.git
cd taceta
```

開発中に直接起動する場合は `cargo run` を使えます。

```sh
cargo run
```

通常利用で使う app bundle は、次の正規スクリプトで release binary から生成します。version、protocol、extension の整合性を確認し、`dist/Taceta.app` を作成します。

```sh
./scripts/build-macos-app.sh
```

生成物をユーザー単位でインストールして起動します（`/Applications` へ Finder でコピーしても構いません）。

```sh
./scripts/install-macos-app.sh
```

既定のインストール先は `~/Applications` です。別の場所へ置く場合は `./scripts/install-macos-app.sh --install-dir /Applications` のように指定します。`cargo run` は開発用であり、インストール済みランタイムとしては使用しないでください。署名、公証、インストーラー作成は現在のスクリプトの範囲外です。

## Taceta Link のセットアップ

Taceta の「Taceta Link をセットアップ」を押すと、macOS のデフォルトブラウザーが Brave または Chrome の場合に、拡張を `~/Library/Application Support/Taceta/browser-extension` へ materialize し、ユーザー専用の Native Messaging Host を登録して拡張管理ページを開きます。

ブラウザー側で一度だけ次を行います。

1. Brave は `brave://extensions`、Chrome は `chrome://extensions` を開く。
2. **Developer mode（デベロッパーモード）** を ON にする。
3. **Load unpacked（パッケージ化されていない拡張機能を読み込む）** / **Add（追加）** を選ぶ。
4. Taceta が表示した Application Support 内の `browser-extension` フォルダーを選ぶ。
5. 拡張 ID `hefhkgbiiajifedgjlbiklclooifkidg` と version が一致することを確認する。

Taceta はブラウザーの承認を無断で完了したり、拡張をサイレントインストールしたりしません。Safari などの未対応ブラウザーには登録しません。更新時は拡張管理ページで **Reload（再読み込み）** を押してください。拡張単体の開発・検証と Native Messaging の詳細は [browser-extension/README.md](browser-extension/README.md) を参照してください。

## 更新とアンインストール

更新時は Taceta を終了し、`./scripts/build-macos-app.sh` の後に `./scripts/install-macos-app.sh` を実行します。その後、ブラウザーの拡張管理ページで Taceta Link を Reload します。app bundle の更新と拡張の Reload は別の操作です。

アンインストール時は、Taceta とブラウザーで実行中の処理を終了し、拡張管理ページで Taceta Link を **Remove（削除）** してから、Taceta.app を Finder でゴミ箱へ移動します。必要であれば `~/Library/Application Support/Taceta` を確認して設定・履歴を削除してください。この最後の操作はデータを失うため、先にバックアップしてください。Ollama、Ollama のモデル、macOS Keychain の provider key は Taceta のアンインストールでは削除されません。

## 制限事項と実験的機能

- macOS 専用で、Apple Silicon を主対象としています。
- Ollamaには稼働中portを問い合わせるAPIがありません。シェルだけに設定した一時的な `OLLAMA_HOST` は自動検出できないため、設定画面の手動接続先を使用してください。
- Ollama の稼働、モデルの能力、利用可能な context length はモデルごとに異なります。設定値がモデル上限を超える場合は Ollama 側の制約が適用されます。
- Taceta Link の検索・ChatGPT Web 経路は、ログイン状態、ブラウザーの権限、ネットワーク、対象サイトの UI 変更に依存します。
- ChatGPT Web は公式 API 統合ではありません。サービス側の変更や利用条件により停止・変更され得ます。安定したプログラム統合が必要な場合は、対象サービスが提供する公式 API を検討してください。
- 配布用 app bundle のコード署名、公証、更新署名はまだ提供していません。公開バイナリを配布する場合は、Gatekeeper と署名の状態を確認してください。
- このリポジトリには OpenAI の公式拡張のコードや bundle を同梱・再配布していません。

## ライセンス

Taceta のコードと同梱アセットは [MIT License](LICENSE) で提供します。Copyright (c) 2026 Makoto Suzuki.

Ollama、ブラウザー、検索 API、ChatGPT Web、モデル、および Rust の依存クレートは Taceta とは別の製品・サービスです。それぞれのライセンス、利用規約、商標条件が適用されます。Taceta はそれらの提供元から承認、後援、提携を受けていません。

---

# Taceta (English)

Think quietly, answer locally. Taceta is a macOS-only local inference client using Ollama as its backend. It is built with Rust and `eframe` / `egui`.

Taceta Link is a separate Manifest V3 extension that lets Taceta explicitly start searches and ChatGPT Web interactions in a logged-in browser. Neither project is an official product of OpenAI, Ollama, Brave, or Google.

## Features

- Stream responses from Ollama models
- Independently control Thinking execution and Thinking-trace visibility
- Attach UTF-8 text, and images only to models with confirmed vision capability
- Persist Japanese / English, System / Light / Dark, and font size 10–32
- Follow Ollama `OLLAMA_HOST` and port changes automatically, with a manual endpoint option
- Render Markdown including headings, emphasis, code, quotes, lists, tasks, links, tables, and footnotes
- Per-conversation Web Search, off by default
- Brave Search / Ollama Web Search APIs, or browser search, Google Search, and ChatGPT Web through Taceta Link

When Web Search is OFF, Taceta creates no external request. ON does not send every conversation to the web. Only a clear search intent in the current input—such as an explicit search command, a request to verify current/latest or date-specific information, or a request for source URLs—requires Taceta to complete at least one search before answering. Conversation history is not used to detect search intent. For normal conversation or ambiguous input, search is not forced; the local model may still call `web_search` when it determines that search is needed, as before. In addition, if the local model returns no tool call but plainly states in ordinary text that it will search immediately for the current input, Taceta suppresses that announcement instead of displaying it as the final answer and routes the same input to search once per turn. Explanations, past-tense statements, negations, questions about searching, and normal conversation do not trigger this fallback. When searching, only the selected executor is used, and search or browser output is passed to local Ollama as untrusted context for the final answer. ChatGPT Web defaults to one request and can be limited from one to three independently of the search-result limit used by Google and other search providers. After the configured limit is reached (the fourth request at the maximum setting), Taceta rejects further `web_search` requests instead of sending them to ChatGPT Web, and the local model synthesizes the final answer from the responses already collected, up to three.

## Screenshots

### Chat

The main view keeps local-model output, the Thinking trace, Markdown tables and quotes, and Web Search status visible together.

![Taceta chat in English](docs/images/taceta-chat-en.png)

### Settings

Configure language, theme, model management, Web Search, the model location, and context length.

![Taceta settings in English](docs/images/taceta-settings-en.png)

## Architecture and data flow

```text
Taceta (Rust/egui)
  ├─ Web Search OFF ───────────────→ Ollama (resolved endpoint)
  ├─ Brave / Ollama Web Search API ─→ external search → Ollama (final answer)
  └─ Taceta Link ──────────────────→ Brave / Chrome
                                      └─ search or ChatGPT Web
                                         → Native Messaging
                                         → Taceta → Ollama (final answer)
```

Taceta Link consists of the MV3 extension in `browser-extension/`, the Native Messaging Host `org.mlabo.taceta.link`, and a per-user Unix socket. The extension prefers an existing normal browser window, creates a working tab/group, and tracks only the exact tab/group created by Taceta. It never closes the browser window. A product-version, protocol-version, or fixed-extension-ID mismatch fails closed.

The first ChatGPT Web request sends the current composer prompt exactly. Only when two or three requests are explicitly selected do later requests attach the selected local model's `web_search` query to the original prompt as an unverified additional research angle. ChatGPT is instructed to correct mistaken names, versions, and premises. The “local model angle” status shown in Taceta is a temporary progress label identifying the source of the follow-up query; it is not ChatGPT's internal reasoning or conversation history. Taceta does not directly read, send, or store conversation history, system messages, attachments, Thinking traces, cookies, tokens, profiles, or local storage. ChatGPT Web output is received incrementally, while local Ollama remains responsible for the final answer. This experimental route can break when the web UI or service conditions change.

## Security and privacy

- Normal chats and conversation history are stored in this Mac's local application data. Conversation context and attachments needed for generation are sent only to the currently configured Ollama endpoint.
- Ollama's default endpoint is `http://127.0.0.1:11434`. Taceta can resolve Ollama's configuration automatically and does not bundle or redistribute Ollama or models.
- Only when Web Search is enabled, the configured search provider receives a query or request. The UI asks for confirmation before sending.
- Where a search provider requires an API key, it is stored in the macOS Keychain. Browser cookies and authentication tokens are never read or exported.
- Taceta Link requests `tabs`, `tabGroups`, `scripting`, `debugger`, `nativeMessaging`, HTTPS host access, and related permissions to operate its working route and search pages. It is designed to act only on tabs tracked as Taceta-owned.
- Login, account changes, purchases, deletions, and other destructive or account actions are not automated.

Taceta Link is not an official OpenAI / ChatGPT extension. It is an unofficial, experimental integration using the ChatGPT Web DOM. Check the terms and administrator policies of the account, web services, and browser you use.

## Requirements

- macOS 13.0 or later (Apple Silicon is the primary target)
- Rust 1.92 or later when building from source
- [Ollama](https://ollama.com/) installed and running separately
- Brave or Chrome for Taceta Link

Users explicitly retrieve and remove models through Taceta's Model Manager. Ollama, browsers, search APIs, ChatGPT Web, and models remain subject to their respective provider terms and conditions.

## Ollama endpoint

In the default **Automatic** mode, Taceta resolves the Ollama endpoint in this order whenever it connects:

1. `OLLAMA_HOST` in the macOS `launchctl` environment
2. `OLLAMA_HOST` inherited by the Taceta process
3. The final fallback `http://127.0.0.1:11434`

Wildcard bind addresses `0.0.0.0` and `::` are converted to the connectable loopback addresses `127.0.0.1` and `::1`, matching Ollama's official client behavior. Taceta re-reads the configuration before connection operations and after a failed connection, so it can follow a `launchctl` port change made while Taceta is running.

There is no official API through which a separate GUI application can discover an Ollama server started with a shell-only setting such as `OLLAMA_HOST=127.0.0.1:23456 ollama serve`. Select **Manual** in Settings for that case. Taceta does not silently fall back to port `11434` when a configured non-default endpoint is unreachable. To avoid starting another server on the wrong port, Taceta auto-starts Ollama only when using the final default fallback.

See Ollama's official documentation for the [API base URL](https://docs.ollama.com/api/introduction#base-url), [macOS environment configuration](https://docs.ollama.com/faq#setting-environment-variables-on-mac), and [network binding](https://docs.ollama.com/faq#how-can-i-expose-ollama-on-my-network).

## Build and run from source

```sh
git clone https://github.com/mlabo-org/taceta.git
cd taceta
```

Use `cargo run` for development-time direct execution:

```sh
cargo run
```

For normal use, create the macOS app bundle with the canonical release materialization script. It checks product, protocol, and extension-version consistency and creates `dist/Taceta.app`.

```sh
./scripts/build-macos-app.sh
```

Install and launch the bundle for the current user:

```sh
./scripts/install-macos-app.sh
```

The default destination is `~/Applications`; use `./scripts/install-macos-app.sh --install-dir /Applications` for another destination. `cargo run` is a development command, not the installed runtime. Code signing, notarization, and installer creation are outside the current scripts.

## Set up Taceta Link

Choose **Set up Taceta Link** in Taceta. If the macOS default browser is Brave or Chrome, Taceta materializes the extension at `~/Library/Application Support/Taceta/browser-extension`, registers a per-user Native Messaging Host, and opens the extension-management page.

Complete these browser steps once:

1. Open `brave://extensions` or `chrome://extensions`.
2. Turn on **Developer mode**.
3. Choose **Load unpacked** / **Add**.
4. Select the `browser-extension` folder in the Application Support location shown by Taceta.
5. Confirm extension ID `hefhkgbiiajifedgjlbiklclooifkidg` and the matching version.

Taceta does not silently approve or install the browser extension, and does not register with Safari or other unsupported browsers. After an update, press **Reload** for Taceta Link. See [browser-extension/README.md](browser-extension/README.md) for standalone extension development and Native Messaging details.

## Update and uninstall

Quit Taceta before updating, then run `./scripts/build-macos-app.sh` followed by `./scripts/install-macos-app.sh`. Reload Taceta Link on the browser's extension-management page; updating the app bundle and reloading the extension are separate actions.

To uninstall, stop active Taceta Link work, choose **Remove** for Taceta Link in the browser, and move Taceta.app to the Trash in Finder. If desired, inspect `~/Library/Application Support/Taceta` and remove Taceta's settings and history after backing up anything needed. Uninstalling Taceta does not remove Ollama, Ollama models, or provider keys stored in the macOS Keychain.

## Limitations and experimental status

- Taceta is macOS-only, with Apple Silicon as the primary target.
- Ollama has no API for asking a running server which port it uses. A temporary `OLLAMA_HOST` set only in the server's shell cannot be auto-detected; use the manual endpoint in Settings.
- Ollama availability, model capability, and supported context length vary by model. If a configured context length exceeds a model's limit, Ollama applies its own constraint.
- Taceta Link search and ChatGPT Web routes depend on login state, browser permissions, network access, and the target site's UI.
- ChatGPT Web is not an official API integration. It can stop or change because of service changes or applicable terms. For a stable programmatic integration, consider the official API offered by the relevant service.
- Code signing, notarization, and signed update delivery for distributed app bundles are not currently provided. Verify Gatekeeper and signing status before running a downloaded binary.
- This repository does not include, bundle, or redistribute code from the official ChatGPT extension.

## License

Taceta's code and bundled assets are released under the [MIT License](LICENSE). Copyright (c) 2026 Makoto Suzuki.

Ollama, browsers, search APIs, ChatGPT Web, models, and Rust dependency crates are separate products and services. Their own licenses, terms, and trademark conditions apply. Taceta is not endorsed, sponsored, or affiliated with their providers.
