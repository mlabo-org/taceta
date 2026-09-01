# Taceta

静かに考え、手元で答える。Taceta は macOS 専用の、ローカル推論向けネイティブチャットクライアントです。Rust と `eframe` / `egui` で構築し、会話と推論の表示を一つの落ち着いた画面にまとめます。

## できること

- ローカルのモデルへ接続してストリーミング回答を表示
- Thinking の「生成」と「経過表示」を独立して切り替え
- Thinking を動かしたまま、trace は画面に出さない設定
- 対応モデルの能力に応じた Thinking ON/OFF または強度選択
- UTF-8 テキストファイルの添付
- vision 能力を持つモデルへの画像添付（能力がないモデルへは送信しない）
- 日本語 / 英語、System / Light / Dark、文字サイズ 10–32 の設定保存
- 生成中の表示切り替えと停止
- v0.1ではローカルチャット、Thinking、添付ファイル、32kのコンテキスト長に集中

Thinking のtraceは会話入力へ混ぜません。表示を隠しても推論自体は止まらず、表示設定と実行設定は別々に扱います。モデルによって制御できる範囲は異なるため、能力を確認できない選択肢はUIに制御可能とは表示しません。

## 必要環境とセットアップ

- macOS 13.0 以降（Apple Siliconでの利用を主対象）
- Rust 1.92 以降（開発・ビルド時）
- ローカル推論バックエンドとして [Ollama](https://ollama.com/) を別途インストールし、既定の loopback エンドポイント `http://127.0.0.1:11434` で起動

Taceta は Ollama 本体やモデルを同梱・再配布しません。モデルの取得・削除・設定変更は、利用者がバックエンド側で明示的に行ってください。各モデルのライセンスはモデルごとに異なるため、利用するモデルの配布元の条件を確認してください。

## ビルドと起動

ソースから開発実行する場合:

```bash
cargo run
```

通常利用向けの prebuilt native app bundle は、リポジトリのルートで次を実行して生成します。

```bash
./scripts/build-macos-app.sh
open ./dist/Taceta.app
```

このスクリプトは release binary を `dist/Taceta.app/Contents/MacOS/Taceta` に配置し、`Info.plist` を生成します。`target/` と `dist/` は生成物であり、ソース成果物ではありません。署名・公証・インストーラー作成はこのスクリプトの範囲外です。

## 公開範囲と免責

Taceta は Ollama または OpenAI / Codex と公式に提携・承認・後援された製品ではありません。Ollama は交換可能なローカルバックエンドとして利用しています。名称・商標の権利はそれぞれの権利者に帰属します。Taceta 自体のコードは MIT License で提供しますが、接続先のモデル、依存ソフトウェア、macOS はそれぞれ固有のライセンスまたは利用条件に従います。

## 将来の拡張境界

v0.1の実装範囲は local chat / Thinking / attachments / context length です。将来のCodex harness統合は未実装であり、現在の機能として扱いません。詳細な責務分離と段階的なロードマップは [`docs/architecture.md`](docs/architecture.md) を参照してください。

## 謝辞

ローカル推論接続の実装では Ollama の公開 API 仕様を参照しています。Ollama プロジェクトとモデル作者の皆さまに感謝します。

## ライセンス

Copyright (c) 2026 Makoto Suzuki。Taceta のコードは [MIT License](LICENSE) で公開します。

---

# Taceta (English)

Think quietly, answer locally. Taceta is a macOS-only native chat client for local inference. Built with Rust and `eframe` / `egui`, it keeps conversation and inference controls in one calm workspace.

## Features

- Stream responses from a local model
- Control Thinking generation separately from Thinking trace visibility
- Keep Thinking active while keeping its trace off screen
- Offer Thinking on/off or levels according to the selected model's capabilities
- Attach UTF-8 text files
- Attach images only to models with vision capability
- Persist Japanese / English, System / Light / Dark, and font size 10–32 settings
- Change visibility while generating and stop generation
- v0.1 focuses on local chat, Thinking, attachments, and a 32k context length

Thinking traces are never added to the next conversation input. Hiding a trace does not stop generation; execution and presentation are separate settings. Available controls are capability-driven and are not advertised for models whose behavior has not been confirmed.

## Requirements and setup

- macOS 13.0 or later (Apple Silicon is the primary target)
- Rust 1.92 or later for development builds
- Install [Ollama](https://ollama.com/) separately as the local inference backend and run it at the default loopback endpoint, `http://127.0.0.1:11434`

Taceta does not bundle or redistribute the Ollama application or any model. Retrieve, remove, and configure models explicitly on the backend side. Model licenses differ by model; review the terms from each model's distributor before use.

## Build and launch

For development:

```bash
cargo run
```

To materialize the prebuilt native app bundle for normal use, run this from the repository root:

```bash
./scripts/build-macos-app.sh
open ./dist/Taceta.app
```

The script places the release binary at `dist/Taceta.app/Contents/MacOS/Taceta` and creates `Info.plist`. `target/` and `dist/` are generated artifacts, not source. Signing, notarization, and installer creation are outside this script's scope.

## Public-use boundary and disclaimer

Taceta is not officially affiliated with, endorsed by, or sponsored by Ollama or OpenAI / Codex. Ollama is used as a replaceable local backend. Names and trademarks remain the property of their respective owners. Taceta's code is released under the MIT License; connected models, dependencies, and macOS remain subject to their own licenses and terms.

## Future extension boundary

The v0.1 scope is local chat / Thinking / attachments / context length. Future Codex harness integration is not implemented and must not be read as a current feature. See [`docs/architecture.md`](docs/architecture.md) for the responsibility boundary and staged roadmap.

## Acknowledgements

The local inference connection refers to Ollama's public API specification. Thanks to the Ollama project and the authors of the models used with Taceta.

## License

Copyright (c) 2026 Makoto Suzuki. Taceta's code is released under the [MIT License](LICENSE).
