# Taceta

静かに考え、手元で答える。Taceta は macOS 専用のローカル推論向けネイティブチャットクライアントです。Rust と `eframe` / `egui` で構築します。

## できること

- ローカルモデルのストリーミング回答、Thinking の生成と trace 表示の独立制御
- UTF-8 テキスト添付、vision 能力を確認できたモデルへの画像添付
- 日本語 / 英語、System / Light / Dark、文字サイズ 10–32 の保存
- 会話単位の Web Search（既定 OFF）
- Brave Search / Ollama Web Search API、または Taceta Link 経由のブラウザー既定検索・Google 検索・ChatGPT Web

Web OFF は完全にローカルです。Web ON は設定された executor を自動適用します。外部結果は untrusted context として扱い、最終回答はローカル Ollama が生成します。Web ON + Send は一回の Web request だけを許可します。ChatGPT Web へは現在の入力欄の prompt だけを正確に渡し、履歴、system message、添付、Thinking trace は渡しません。アカウント操作や破壊的操作は、Web ON でも確認を要求します。

## 製品構成

Taceta は単一 Git repo / version の中で、Rust アプリ `src/` と独立 component `browser-extension/` を物理的に分離しています。Taceta Link は Manifest V3、Native Messaging Host (`org.mlabo.taceta.link`)、user-only Unix socket のローカル経路です。Codex、外部 browser plugin、Node companion、Cookie / token のエクスポートには依存しません。

拡張は既存の normal window（focused を優先）を作業コンテナとして再利用し、その中に非アクティブな agent tab と group を作成します。normal window がない場合だけ非フォーカスの window を作成します。window は所有・削除せず、Taceta が作成した exact tab/group だけを追跡し、終了時は ungroup と agent tab の削除を行います。product version と protocol version が一致しない場合は fail-closed します。固定 extension ID は `hefhkgbiiajifedgjlbiklclooifkidg` です。

## 必要環境

- macOS 13.0 以降（Apple Silicon を主対象）
- Rust 1.92 以降（開発・ビルド時）
- [Ollama](https://ollama.com/) を別途インストールし、`http://127.0.0.1:11434` で起動
- Taceta Link を使う場合は Brave または Chrome

Taceta は Ollama 本体やモデルを同梱・再配布しません。モデルの取得・削除は利用者が Model Manager 画面で明示的に実行します。API key は必要な provider ごとに macOS Keychain へ保存します。

## Taceta Link の初回セットアップ

Taceta は macOS のデフォルトブラウザーを検出します。初期対応は Brave と Chrome です。起動時に拡張を Taceta の Application Support（`~/Library/Application Support/Taceta/browser-extension`）へ materialize し、選択されたブラウザー用の Native Messaging Host を登録し、version と extension ID を検証します。その後、拡張管理ページを開いて次を案内します。

1. Brave は `brave://extensions`、Chrome は `chrome://extensions` を開く。
2. Developer mode（デベロッパーモード）を ON にする。
3. **Load unpacked（パッケージ化されていない拡張機能を読み込む）** / **Add（追加）** を押し、表示された Application Support 内の `browser-extension` フォルダーを選ぶ。
4. ID `hefhkgbiiajifedgjlbiklclooifkidg` と Taceta Link の version が一致することを確認する。

この最終的なブラウザー確認だけは利用者が行います。Taceta は拡張承認を無断で完了したり、サイレントインストールしたりしません。更新時は拡張管理ページで **Reload（再読み込み）** を押すよう案内します。デフォルトブラウザーが Safari など未対応の場合は、Brave または Chrome をインストールしてデフォルトにするよう案内し、未対応ブラウザーへ登録しません。

## ビルドと起動

```bash
cargo run
```

通常利用向け app bundle は次で生成します。

```bash
./scripts/build-macos-app.sh
open ./dist/Taceta.app
```

署名、公証、インストーラー作成はこのスクリプトの範囲外です。

## 公開範囲とライセンス

Taceta は Ollama と公式に提携・承認・後援された製品ではありません。将来、GUI 完成後に typed agent-harness 境界を追加する余地はありますが、現在の Taceta Link や通常起動経路の依存ではありません。

Taceta 自体のコードは [MIT License](LICENSE) で提供します。Copyright (c) 2026 Makoto Suzuki。

---

# Taceta (English)

Think quietly, answer locally. Taceta is a macOS-only native chat client for local inference, built with Rust and `eframe` / `egui`.

## Features

- Stream local-model answers with independent Thinking generation and trace visibility
- Attach UTF-8 text and images only to models with confirmed vision capability
- Persist Japanese / English, System / Light / Dark, and font size 10–32
- Per-conversation Web Search, off by default: Brave Search or Ollama Web Search APIs, or Taceta Link browser-default search, Google Search, and ChatGPT Web

Web OFF is completely local. Web ON automatically applies the configured executor. Browser and search output is untrusted external context; local Ollama always generates the final answer. Web ON + Send authorizes one web request. ChatGPT Web receives exactly the current prompt, never history, system messages, attachments, or Thinking traces. Account and destructive actions still require confirmation.

## Product layout

One Git repository and product version contain two physically separate components: the Rust app in `src/` and the independent extension in `browser-extension/`. Taceta Link is a local Manifest V3 + Native Messaging Host (`org.mlabo.taceta.link`) + user-only Unix socket path. Taceta has no runtime dependency on Codex, external browser plugins, Node companions, or cookie/token export.

The extension prefers an existing focused normal window as its route container and creates an inactive agent tab and group there; it creates a non-focused normal window only when none exists. The window is never owned or closed. Taceta tracks only its exact agent tab/group, then ungroups and removes that agent tab at the end of the session. Mismatched product or protocol versions fail closed. The fixed extension ID is `hefhkgbiiajifedgjlbiklclooifkidg`.

## Requirements

- macOS 13.0 or later (Apple Silicon is the primary target)
- Rust 1.92 or later for development builds
- [Ollama](https://ollama.com/) installed separately at `http://127.0.0.1:11434`
- Brave or Chrome for Taceta Link

Taceta does not bundle or redistribute Ollama or models. Model retrieval and removal are explicit user actions in Model Manager. Provider API keys, where required, are stored in the macOS Keychain.

## First-time Taceta Link setup

Taceta detects the macOS default browser. Initial support is Brave and Chrome. It materializes the extension under Taceta Application Support (`~/Library/Application Support/Taceta/browser-extension`), registers the browser-specific Native Messaging Host, and verifies the version and extension ID. It then opens the extension-management page and guides the only manual browser approval:

1. Open `brave://extensions` or `chrome://extensions`.
2. Turn on Developer mode.
3. Choose **Load unpacked** / **Add**, then select the `browser-extension` folder in the Taceta Application Support directory shown by Taceta.
4. Confirm extension ID `hefhkgbiiajifedgjlbiklclooifkidg` and the Taceta Link version.

The final browser approval is manual by design. Taceta cannot silently approve or install an unpacked extension. On updates, it guides the user to press **Reload**. Safari and other unsupported default browsers are directed to install Brave or Chrome and make one the default; Taceta does not register against an unsupported browser.

## Build and launch

```bash
cargo run
```

```bash
./scripts/build-macos-app.sh
open ./dist/Taceta.app
```

Signing, notarization, and installer creation are outside this script.

## Scope and license

Taceta is not officially affiliated with, endorsed by, or sponsored by Ollama. A future typed agent-harness boundary may be added after the GUI is complete, but it is not a current dependency or launch route. Taceta's code is released under the [MIT License](LICENSE). Copyright (c) 2026 Makoto Suzuki.
