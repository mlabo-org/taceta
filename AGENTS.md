# Taceta Local Contract

このファイルは、`Taceta` repository 配下における Codex の局所 `AGENTS.md` であり、本スコープ内の実行条件を定義する SSOT である。
本書は助言集ではなく、正本境界、製品契約、外部作用、検証条件を拘束する運用契約として扱う。
上位の `AGENTS.md`、システム指示、開発者指示、ユーザーの明示要求と競合する場合は、Codex の優先順位規則に従う。
より深い階層の `AGENTS.md` は、そのスコープ内の通常ルールを具体化できるが、上位の名前付き不変条件を弱めない。

## Source Boundary

- `Cargo.toml`、`Cargo.lock`、`src/`、`assets/`、`scripts/` をアプリの正本 source とする。
- `target/`、生成済み `.app`、ログ、会話履歴、設定、モデル、Ollama runtime data は正本 source ではない。
- 通常利用は release build から materialize した native macOS app bundle を直接起動する。`cargo run` を installed runtime にしない。

## Product Contract

- Taceta は Rust と `eframe` / `egui` 0.34で構築するmacOS専用の独立ローカル推論クライアントである。
- 製品UI、アプリ名、アイコンにバックエンド提供者の名称またはロゴを使用しない。READMEの依存関係説明と謝辞だけでOllamaを明示する。
- バックエンド固有のwire形式はbackend adapter内に隔離し、UIと会話domainへ漏らさない。
- 日本語と英語、System・Light・Dark theme、10–32の整数font sizeを永続化する。

## Thinking Contract

- Thinkingの実行モードとThinking traceの表示状態は、独立した設定として実装する。
- trace非表示は推論を停止せず、最終回答の表示を妨げない。表示切替は生成中にも適用できる。
- Thinking対応モデルではAPI能力に従ってON/OFFまたはlevelを提示する。能力未確認のモデルへ制御可能と表示しない。
- GPT-OSSではOFFを提示せず、Low・Medium・Highだけを提示する。`true`または`false`が効くと推定しない。
- Thinking traceを次の会話入力へ混ぜない。

## External Effects

- 既定接続先はloopback `http://127.0.0.1:11434` とする。
- モデル取得、削除、外部公開、クラウド接続、Ollama設定変更をチャットの通常経路から実行しない。
- 生成停止は進行中HTTP streamのcancelとして扱う。モデルのunloadは別の明示操作にする。
- 会話履歴と設定はユーザーのlocal application dataだけへ保存し、外部送信しない。

## Acceptance

- source acceptanceは、Thinking request変換、NDJSON分離、state transition、persistenceのfocused Rust testsを所有する。
- runnable acceptanceは、release binary build、`.app` materialization、実Ollama APIとの代表チャット、Thinking実行と表示の独立操作、再起動後の表示設定復元を所有する。
- source、app bundle、installed runtime、Git commit、GitHub公開を別の状態として報告する。
