# speaker-identification

日本語の会議・インタビュー録音を文字起こしするアプリ。話者分離・識別の精度を
最優先に据えた設計。詳しい背景と技術選定の根拠は [docs/plan.md](docs/plan.md)
を参照。

## 構成

```
crates/
  asr-core/   UI非依存のコア。音声デコード・話者分離・話者ID・ASRプロバイダ抽象・
              用語辞書・SQLite永続化
  asr-cli/    評価・バッチ用CLI（DER計測はここ）
  asr-audio/  再生エンジン（Phase 3、未実装 — scaffoldingのみ）
apps/
  desktop/    GPUIアプリ本体（未着手）
models/       ONNXモデル（.gitignore対象、fetch-models.shで取得）
scripts/      補助スクリプト
```

## 現在の状態（Phase 0〜1 相当）

- ✅ 音声デコード（symphonia 0.6）・16kHzモノラルへのリサンプル（rubato 5.0）・
  波形ピークピラミッド生成
- ✅ 話者分離（`sherpa-onnx` 公式バインディング）・話者ID（SQLite永続化つき
  enrollment）
- ✅ `asr-cli` に `info` / `peaks` / `diarize` / `eval`（DER計測、内訳つき）
- ✅ 用語辞書の読み仮名ベース補正（lindera + UniDic埋め込み）の骨組み
- ⛔ ASRプロバイダ（ElevenLabs / Azure）の実HTTP実装 — Phase 2、要APIキー
- ⛔ 再生エンジン（cpal + signalsmith-stretch）— Phase 3、要スパイク検証
- ⛔ GPUIエディタ本体 — Phase 3、日本語IMEスパイクが先

## セットアップ

```sh
# ONNXモデルを取得（初回のみ）
./scripts/fetch-models.sh models

# ビルド
cargo build --workspace

# 音声ファイルの情報を見る
cargo run -p asr-cli -- info path/to/meeting.wav

# 話者分離を実行してRTTM出力
cargo run -p asr-cli -- diarize path/to/meeting.wav --format rttm --out hyp.rttm

# 正解ラベル(RTTM)と比較してDERを計測
cargo run -p asr-cli -- eval --reference ref.rttm --hypothesis hyp.rttm
```

## テスト

```sh
cargo test --workspace
```
