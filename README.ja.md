# ContextWitness

[English](README.md) | 日本語

ContextWitness は、画面上の活動を後から照会できる長期記憶へ変える Windows 常駐デーモンです。前面ウィンドウを一定間隔でキャプチャし、フレームを Windows OCR で読み取り、見えたものを時刻に紐づいたエピソードへまとめて、[Hindsight](https://github.com/vectorize-io/hindsight) のメモリバンクへ配送します — そのバンクにつないだアシスタントは「火曜の午後は何をしてた?」のような質問に答えられるようになります。

Windows 11 24H2(ビルド 26100)以降が必要です。ビルドには MSVC ツールチェーンが必要です。エピソードを Hindsight へ配送するには Hindsight v0.8.6 以降が必要です。

## v1 でできること

- 前面ウィンドウを数秒ごとに Windows Graphics Capture でキャプチャし、実際に十分な量のピクセルが変化したときだけフレームを保存します。他のウィンドウとモニターはキャプチャしません。前面ウィンドウのうち他のウィンドウに隠れた部分は写ります。
- 画面上のテキストを Windows OCR エンジンで抽出します。設定した言語のうちインストール済みエンジンが存在する最初のものを使います(既定は日本語→英語の順)。
- キャプチャをエピソード窓(既定 5 分)へまとめ、時刻に紐づいた活動ログとして描画します。
- すべてをまずローカルへ保存します: エピソードは SQLite に、フレームは WebP 画像として(保持は既定 14 日 / 50 GiB)。
- 永続 outbox を通じてエピソードを Hindsight へ配送します: サーバが落ちていればエピソードは待機し、復帰後に配送されます。
- トレイアイコン付きで常駐します。`pause`/`resume` はトレイからもコマンドラインからも。ログオン時の自動起動は任意です。

## インストール

いずれか:

- **Portable ZIP** — [Releases](https://github.com/Skyzi000/contextwitness/releases) から `contextwitness-vX.Y.Z-windows-x86_64.zip` をダウンロードし、任意の場所へ展開して、ターミナルから `contextwitness.exe` を実行します。
- **ソースから** — `cargo install --git https://github.com/Skyzi000/contextwitness cw-daemon`(Rust の MSVC ツールチェーンが必要)。

バイナリは署名なしのオープンソースビルドのため、初回実行時に Windows SmartScreen が警告することがあります。

## Quickstart

```text
contextwitness setup
contextwitness run
```

`setup` は Hindsight API URL・API トークン(任意)・データディレクトリを尋ねて書き込みます。`run` はトレイアイコンを出してキャプチャを開始し、止められるまで動き続けます。その他のコマンド: `status`(このインストールが何をしているかの 1 画面サマリ)、`pause [30m|2h|...]`、`resume`、`autostart enable|disable`(ログオン時の自動起動)、`capture-once`(パイプライン確認用の手動キャプチャ 1 回。`pause` を無視します)。

## 設定

`%APPDATA%\ContextWitness\config.toml`。`setup` または `run` が最初に必要としたときに、以下の既定値で書き出されます:

| キー | 既定値 | 意味 |
| --- | --- | --- |
| `capture.interval_secs` | `10` | キャプチャ試行の間隔秒数(1–30)。 |
| `capture.change_pixel_threshold` | `8` | ピクセルごとの輝度差がこの値以下なら「変化なし」と数える(0–254)。 |
| `capture.change_area_logical_pixels` | `600` | 変化した論理ピクセル数(表示スケーリング 100% 換算)がこれを超えたらフレームを保存して OCR する。 |
| `capture.webp_quality` | `75` | WebP エンコード品質(0–100)。 |
| `ocr.languages` | `["ja", "en"]` | OCR エンジンへ提示する言語(重要な順)。インストール済みエンジンがある最初のものが使われる(いずれにもインストール済みエンジンが無ければプロファイルの言語)。 |
| `storage.data_dir` | `""` | データディレクトリ。空なら `%LOCALAPPDATA%\ContextWitness`、それ以外は絶対パス。 |
| `storage.image_retention_days` | `14` | キャプチャ画像を保持する日数。 |
| `storage.image_retention_max_gib` | `50` | キャプチャ画像全体の容量上限(GiB)。 |
| `privacy.process_blacklist` | `[]` | キャプチャを無効化するプロセス名。 |
| `hindsight.bank_id` | `"contextwitness"` | エピソードの配送先となる Hindsight バンク。一覧で存在を確認できた bank の設定は変更せず、Hindsight 側で変更した設定はそのまま残る。不在の場合は bank config への PATCH を 1 回だけ送る — `retain_extraction_mode="chunks"`(エピソードを LLM 抽出ではなく原文チャンクとして保存)、`store_document_text=false`、画面OCRの出典を説明する `observations_mission` の 3 項目で、他の設定は Hindsight の継承に任せる。自動初期化には Hindsight の bank config API(`enable_bank_config_api`)が必要。無効な場合は、サーバー側で目的の設定を持つ bank を事前に作成すれば ContextWitness はそれに触れずに配送する。確認後の同時作成には競合が残る。それでも ContextWitness 専用のバンクを与えること。 |
| `hindsight.context_label` | `"Time-stamped OCR text of the foreground window, with its application and window title where known. May contain OCR errors; shows what was displayed, not what the user read, wrote, or did."` | 全エピソードに添えて送られる context ラベル。 |
| `episode.window_minutes` | `5` | エピソード窓の長さ(分、1–1440)。 |

`store_document_text=false` により、Hindsight 側にはこれらのエピソードの文書・source chunk 本文は残らない。そのため、Hindsight での文書・チャンク本文の取得、Reflect の expand、保存原文からの再処理は利用できない。原文チャンクの記憶本体と ContextWitness 側のローカル記録は残る。

Hindsight の資格情報が `config.toml` に置かれることはありません。`setup` はそれらを `%USERPROFILE%\.hindsight\contextwitness.json` へ書き込みます。代わりに環境変数 `CONTEXTWITNESS_HINDSIGHT_URL` と `CONTEXTWITNESS_HINDSIGHT_TOKEN` でも配送を設定できます(URL 変数が設定されているときは環境変数を資格情報一式とみなし、ファイルは読みません。トークン変数だけの設定は、ファイル側の URL と黙って組み合わされるのではなく拒否されます)。

## プライバシー

ContextWitness は画面を記録します。実行する前に、それが何を意味するかを知っておいてください:

- **既定ではすべてを収集します。** 前面ウィンドウは、表示内容に関係なく、他のウィンドウに隠れた部分も含めてキャプチャされます。その中で OCR が認識したテキストはすべて保存され、各エントリには、取得できる限りそのウィンドウのタイトルとプロセス名が記録されます。
- **Hindsight への配送を除き、すべてはあなたのマシンに留まります。** エピソードのテキスト(ウィンドウタイトルとプロセス名を含む)とそのメタデータ(エピソードの時刻、ローカル画像パス)は、あなたが設定した Hindsight サーバへ送られます。それ以外へは何も送信されません。キャプチャ画像そのものがアップロードされることはありません。
- **`privacy.process_blacklist`** は、列挙したプロセス(実行ファイル名で照合、大文字小文字は区別しない)が前面にある間、キャプチャをスキップします。blacklist が設定されているのに前面プロセスを判定できないときは、危険を冒さずその tick をスキップします。
- **Pause** はキャプチャループを止めます: トレイメニューから、または `contextwitness pause 30m`(時間指定なしは `resume` まで)。
- **Retention** は保存画像を経過日数と総容量で削除します。ローカルデータベースのエピソードテキストは無期限に保持されます — それこそがこのツールの築く記憶だからです。
- **ログは機密情報です。** ウィンドウタイトル・プロセス名・OCR 原文といったキャプチャ内容を含むことがあります。たとえば配送診断はサーバが返したエラーテキストを引用し、エピソードを拒否したサーバはそのエピソードを引用し返すことがあります。`contextwitness status` が再表示する最新の影響エピソードの配送エラーも同様に扱ってください。

## ライセンス

[MIT](LICENSE)
