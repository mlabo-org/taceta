# Taceta architecture boundary

この文書は、Taceta v0.1の実装範囲と、将来のCodex harness統合を混同しないための設計境界です。ここに記載した将来機能は未実装です。

## v0.1の範囲

v0.1は次の機能に集中します。

- local chat inference
- Thinkingの生成制御とtrace表示制御
- UTF-8テキストと、vision能力が確認されたモデルへの画像attachments
- 現行チャットのcontext length（default 32k）
- 会話単位で明示的にONにするWeb Search。検索プロバイダーはBrave Search（既定）またはOllama Web Searchから選択する。

`InferenceBackend`はstateless-ishなchat inference boundaryです。モデル選択、会話入力、添付データ、Thinking設定、明示されたWeb Search設定を受け取り、Thinking delta、content delta、検索進捗、参照元、完了、失敗などのchat eventsを返します。バックエンドのwire形式はこの境界の内側に隔離します。Web SearchがOFFならtoolsは送らず、ONでもtool能力のないモデルには送信前にエラーを返します。プロバイダーは暗黙に切り替えず、キーはmacOS Keychainだけに保存します。

Web Searchはブラウザや外部アプリではなくTaceta内のHTTPハーネスです。検索語と取得先はONにした会話だけで外部へ送信され、検索進捗は会話本文に混ぜません。参照元URLだけをassistant messageへ保存して再起動後も復元します。

## 将来のCodex harness境界

Codexとの統合は、InferenceBackendへ機能を押し込む拡張ではありません。将来追加する別責務 `AgentHarness` が、次のライフサイクルを所有します。

- tool callsとその結果
- approval（人間の承認）
- sandbox境界
- workdir
- 子processの起動、監視、停止を含むprocess lifecycle

`InferenceBackend`はchat eventsだけを扱い、tool call、approval、sandbox、workdir、process lifecycleをchat eventへ偽装して流しません。両者の責務は独立したhandoffで接続します。

将来のCodex harness launcherは、既定ではCodex設定を書き換えず、認証情報を管理せず、子processを起動しません。利用者が明示的に操作した場合だけlauncherを実行します。

## 段階的なロードマップ

1. **First milestone:** 利用者が明示的にインストール済みCodex CLIを起動するlauncher。
2. **Later milestone:** 必要性とAPI契約を確認したうえで、optionalなCodex App Server integration。

Codex App Server統合は将来の選択肢であり、v0.1の実装・依存・通常起動経路には含めません。

Ollamaの公式Codex integration文書が推奨する最低64k contextは、将来のharnessで検討する要件です。現行chatのdefault 32kとは別の設定・別の受け入れ条件として扱います。

根拠:

- [Ollama Codex integration](https://docs.ollama.com/integrations/codex)
- [Codex App Server](https://developers.openai.com/codex/app-server)

---

# Taceta architecture boundary (English)

This document defines the v0.1 implementation boundary so that future Codex harness work is not confused with shipped functionality. Everything described as future work below is not implemented.

## v0.1 scope

Version 0.1 focuses on:

- local chat inference
- independent Thinking generation and trace presentation controls
- UTF-8 text attachments and image attachments only for models with confirmed vision capability
- the current chat context length (32k by default)
- explicit per-conversation Web Search, with Brave Search as the default and optional Ollama Web Search

`InferenceBackend` is a stateless-ish chat inference boundary. It accepts model selection, conversation input, attachments, Thinking settings, and explicit Web Search settings, then emits Thinking deltas, content deltas, search progress, citations, completion, and failure. When Web Search is off, no tools are sent; when on, a model without tool capability is rejected before sending. Providers never silently fall back, and credentials are stored only in the macOS Keychain. Backend wire formats remain inside this boundary.

Web Search is an in-process HTTP harness, not a browser or external application. Queries and fetched URLs leave the Mac only for conversations where it is enabled. Search progress is not mixed into conversation content; citation URLs are stored on assistant messages and restored with the conversation.

## Future Codex harness boundary

Codex integration is not an expansion of InferenceBackend. A separate future `AgentHarness` responsibility will own:

- tool calls and their results
- human approvals
- sandbox boundaries
- workdir
- process lifecycle, including launching, monitoring, and stopping child processes

`InferenceBackend` handles chat events only. Tool calls, approvals, sandbox, workdir, and process lifecycle must not be disguised as chat events. The two responsibilities connect through an explicit independent handoff.

A future Codex harness launcher will not rewrite Codex configuration by default, manage authentication, or start a child process. It runs only after an explicit user action.

## Staged roadmap

1. **First milestone:** a launcher that explicitly starts an already-installed Codex CLI.
2. **Later milestone:** optional Codex App Server integration after its need and API contract are verified.

Codex App Server integration is a future option; it is not part of the v0.1 implementation, dependencies, or normal launch path.

The minimum 64k context recommended by Ollama's official Codex integration documentation is a future harness requirement to evaluate. It is separate from the current chat default of 32k and has separate acceptance criteria.

Sources:

- [Ollama Codex integration](https://docs.ollama.com/integrations/codex)
- [Codex App Server](https://developers.openai.com/codex/app-server)
