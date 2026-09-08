# 録音文字起こしアプリ 実装計画

## Context

会議・インタビューの録音を文字起こしするアプリケーションを新規に作る。リポジトリは空（コミットなし）のグリーンフィールド。

解決したい課題:

1. **処理速度** — 長時間の録音を実用的な時間で処理したい。コアは Rust。
2. **話者分離・識別** — 「誰が話したか」を自動で付けたい。**最重要**。
3. **専門用語・独自用語** — 事前に登録して認識精度を上げたい。
4. **修正 UI** — 音声を再生しながらテキストを直せるようにしたい。

ヒアリング結果:

| 項目 | 決定 |
|---|---|
| クラウド化 | 将来オプション。デスクトップ先行、`asr-core` を UI 非依存 crate にして退路を確保 |
| 外部 ASR API への音声送信 | 許容（精度と手間を優先） |
| 対象音声 | 日本語の会議・打合せ、インタビュー・1対1 |
| MVP の優先事項 | **話者分離・識別の精度を先に追い込む** |
| 評価データ | 実録音を数本用意できる |
| UI フレームワーク | **GPUI**（`gpui-kit` 経由。WebView は不使用 — 理由は後述） |

環境: macOS 26 (Darwin 25.6) / Apple M1 / 16GB / Rust 1.92 / Node 24 / Python 3.14 / ffmpeg 8

---

## 設計上の核心的な判断

### 1. 話者分離は自前、ASR はクラウドに逃がす

クラウド ASR の diarization は毎回 `speaker_0` / `speaker_1` を返すだけで、**録音をまたいだ人物同定ができない**。「田中さん」として識別し続けるには話者埋め込みを自前で保持するしかない。ここが自前実装の価値。

ASR 本体は日本語精度・実装コストの点でクラウド API が有利（機密性の制約なしと確認済み）。

### 2. 話者分離パイプラインは自前実装せず `sherpa-onnx` に委ねる

**検証の結果、当初案（`ort` の上に pyannote 3.x を移植）は取り下げる。**

公式 `sherpa-onnx` crate（v1.13.7 / 2026-09-01 / Apache-2.0 / k2-fsa 本家発行）が pyannote 3.x の全工程を実装済み。自前実装が多週間の泥沼になる理由:

- **powerset デコード** — segmentation-3.0 は話者ごとの確率ではなく3話者のべき集合7クラスのロジットを出す。クラス順序は pyannote のコード内の慣習で ONNX メタデータには無い。間違えると「もっともらしいが微妙に違う」出力になる（最悪の失敗モード）
- **窓をまたぐ順列不変な結合** — 10秒窓を約90%重ねるため窓ごとの話者番号に連続性がない。重複区間で割当問題を解く必要があり、再実装が話者チャーンを生む最頻出ポイント
- **特徴量のパリティ** — Kaldi 形式 fbank のフレーム長・シフト・dither・pre-emphasis・CMN が厳密一致していないと、埋め込みは出るがクラスタリングが静かに劣化する
- **閾値の較正**、**最終セグメントへの再構成**

いずれもエラーではなく「なんとなく人の区別が下手なアプリ」として現れ観測性が悪い。ここを削って**精度チューニングに時間を全振りする**。

> ⚠️ 非公式の **`sherpa-rs` はアーカイブ済み**（README に「非推奨、公式バインディングを使え」と明記）。`lxxyx-sherpa-rs` などのフォークが crates.io に残るが全て無視。`pyannote-rs` も約1年停滞 — **読む価値はあるが依存してはいけない**。

### 3. パイプラインの順序：話者分離が ASR より「前」

一般的な実装は「ASR で全文書き起こし → diarization とマージ」だが、境界がずれると発話が誤った話者に付く。本設計は逆にする:

```
音声ファイル
  ↓ デコード → 16kHz mono f32
  ↓ VAD（無音除去）
  ↓ 話者分離（segmentation → embedding → クラスタリング）
  ↓ 話者付き発話区間が確定
  ↓ 同一話者の連続区間を結合（無音 <0.5s で連結、最大 30〜60秒）
  ↓ 区間ごとに ASR API へ（並列 / 前後にオーバーラップ付与）
  ↓ 用語辞書による後処理補正
  ↓ セグメント列
```

利点:
- 話者境界が ASR より前に確定するので「誰が何を言ったか」が構造的に正確
- 各区間の start/end が既知なので、**再生同期のタイムスタンプが API の精度に依存しない**
- 区間単位で並列に叩けるので実時間より大幅に速い

**短チャンクによる精度低下への対策**（OpenAI は「文の途中で切ると精度が落ちる」と公式明記）:
- 話者 + VAD 境界で切るので、そもそも文の切れ目で切れている（これが最大の対策）
- 同一話者の連続区間を結合し、5〜15秒の細切れにしない
- **各チャンクに前後 200〜500ms の実音声を重ねて送り**、返ってきた端の重複トークンを捨てる（無音パディングではない）。この長さなら課金上ほぼ無視できる
- 直前区間のテキストを `prompt` に渡して文脈を維持

短チャンクの弱さはモデル方式依存。自己回帰型 LLM-ASR（OpenAI 系、Whisper 派生）で顕著、transducer/CTC 系で小さい。ElevenLabs は最短 100ms を公式サポートしており、この構成は想定内の使い方。

### 4. UI は GPUI ネイティブ一本。WebView は使わない

WebView が使えること自体は事実（`gpui-wry` が存在）だが、**このアプリには構造的に使えない**。`gpui-wry` の README が自ら明記している:

> This still a **experimental** with limited features…
> **The WebView will render on top of the GPUI window, any GPUI elements behind the WebView bounds will be covered.**
> So, we recommend using the webview in a separate window or in a Popup layer.

実装はネイティブの子 NSView を Metal レイヤの上に重ね、`prepaint` で `set_bounds()` を呼ぶ方式。したがって:

- ❌ **z 順序なし** — ツールバー、プレイヘッド、コンテキストメニュー、補完ポップオーバーを WebView の上に描けない
- ❌ **クリッピングなし** — `with_content_mask` は GPUI 自身の描画にしか効かない。スクロールコンテナに入れると**ビューポートの外にはみ出す**
- ⚠️ ライブリサイズ時の同期は2系統で更新されるため遅延・ティアリングの懸念（未検証）

そして**そもそも逃げ道が不要になった** — 下記の通り `gpui-kit` が日本語 IME を解決するため。

---

## 技術選定（検証済み）

### コア

| 役割 | 採用 | 備考 |
|---|---|---|
| 話者分離・話者ID・VAD | **`sherpa-onnx` 1.13.7** | 公式バインディング。`ort` を依存ツリーに持ち込まない |
| 音声デコード | **`symphonia` 0.6.1** | features に `mp3`,`aac`,`alac`,`isomp4` を明示追加 |
| リサンプル | **`rubato` 5.0** | 48k→16k 固定なら `FftFixedIn` |
| 形態素解析・読み | **`lindera` 6.0** + `lindera-unidic` / `lindera-sudachidict` | 読みは `Schema::get_field_index("reading")` |
| 階層クラスタリング | **`kodama` 0.3**（必要時のみ） | 会議をまたぐ話者統合。`linfa-hierarchical` は cosine 非対応で不可 |
| ローカル ASR（任意） | `whisper-rs` 0.16 + `metal` | **リポジトリは Codeberg に移転済み**。optional feature に隔離 |
| ASR 既定 | **ElevenLabs Scribe v2 (batch)** | |
| ASR 2番手 | **Azure AI Speech Fast Transcription** | |

### UI・音声再生

| 役割 | 採用 | 備考 |
|---|---|---|
| UI フレームワーク | **`gpui-kit` 0.6**（Apache-2.0） | gpui / base / component / assets を再エクスポートする単一依存 |
| テキスト編集・IME | **`gpui-kit::Textarea`** | 専用エンジン。IME 未解決 issue ゼロ |
| 仮想スクロール | **`gpui::list` + `ListState`** | 可変行高対応。`uniform_list` は不可 |
| 音声出力 | **`cpal`** | トランスポートは自前で持つ |
| タイムストレッチ | **`signalsmith-stretch` 0.1.3**（MIT） | 代替: `timestretch`（純Rust） |

### GPUI の依存方法とライセンス（重要）

**crates.io の公式 `gpui` 0.2.2 は 2025-10-22 で約11ヶ月停滞**しており、現在の `main` とは API が乖離している（`main` の `hello_world.rs` は 0.2.2 に存在しない `gpui_platform::application()` を呼ぶ）。

採用するのは **`gpui-kit = "0.6"`**（0.6.0 / 2026-09-03 / **Apache-2.0**）。単一依存で GPUI 本体ごと入る。

> **ライセンスの整理。** Zed はリポジトリ内で crate ごとにライセンスが分かれている:
> - `gpui`, `gpui_macos` → **Apache-2.0**
> - `editor`, `ui_input`, `text`(rope), `audio` → **GPL-3.0-or-later**
>
> **社内向けツールであれば GPL の制約は実質かからない。** GPL の義務は「頒布(convey)」で発動するため、法人内で使い法人外にバイナリを出さない限りソース公開義務は生じない。Zed のライセンスは GPL-3.0-or-later であって **AGPL ではない**ので、将来 `apps/server` で社内 Web 化してもネットワーク利用では発動しない。
>
> ただし境界に注意（最終的には法務判断）:
> - 「社内」の境界は**法人単位**。子会社・関連会社・業務委託先に渡すと頒布に当たり得る
> - **顧客提供・製品化・スピンアウトの時点で遡って義務が発生**する
>
> **それでも `gpui-kit`（Apache-2.0）を選ぶ。** 理由はライセンスではなく技術的なもの:
> - Zed の `editor` は Zed の buffer / project / LSP 基盤に深く結合しており、そもそも外に持ち出しづらい
> - `gpui-kit` は IME の未解決 issue ゼロ + 専用 `Textarea` + 単一依存
> - **今回の構成では GPL の crate を1つも必要としない** — `audio` は使わず `cpal` 直 + `signalsmith-stretch`、`text` の Rope も `gpui-kit` が自前で持つ

GPUI は公式に pre-1.0 で「バージョン間で頻繁に破壊的変更がある」と明記されている。**バージョンを厳密に固定すること。**

macOS ビルド: Xcode Command Line Tools が必要。Metal シェーダは build script がコンパイルする。フォントは自動ではなくアプリ側で読み込む。関連する既知バグ [zed#63768](https://github.com/zed-industries/zed/issues/63768)（text system の feature flag を誤ると**テキストが不可視になる**）に注意。

### バージョン起因の地雷

- **`symphonia` 0.6 は破壊的な全面書き直し**（2026-05）。ネット上の実例・LLM の記憶はすべて 0.5 系でコンパイルが通らない。[migration guide](https://github.com/pdeljanov/Symphonia/blob/main/docs/guides/migration/0p6.md) を読む
- **`rubato` は8ヶ月で 1.0→5.0**。`Vec<Vec<f32>>` の旧 API は消滅し `audioadapter` 経由に
- **`lindera` も6週間で 4.0→6.0**
- **`ort` と `voice_activity_detector` は同居不可** — `ort-sys` の `links = "onnxruntime"` が衝突しビルドが落ちる。sherpa-onnx 内蔵 VAD を使えば `ort` 自体が不要

### macOS / CoreML の落とし穴

`sherpa-onnx` の**既定（静的リンク）では CoreML が黙って無効化される**。`onnxruntime-osx-arm64-static.cmake` が `-DSHERPA_ONNX_DISABLE_COREML` を定義するため、`provider: Some("coreml")` はコンパイルも実行も通り、ログに fallback を出して **CPU で走る**。

ANE を使うには `default-features = false, features = ["shared"]` が必要だが、`libonnxruntime.dylib` と `libsherpa-onnx-c-api.dylib` の同梱と署名が発生する。

**方針: まず静的リンク・CPU で計測する。** LSTM 主体のグラフは CoreML でも CPU にフォールバックしがちで、複雑さに見合わない可能性が高い。計測してから判断する。

### モデル取得

HuggingFace の `pyannote/segmentation-3.0` は gated（利用条件の同意が必要）だが、**sherpa-onnx が GitHub リリースに再ホストしており同意不要**。

- セグメンテーション: `releases/download/speaker-segmentation-models/sherpa-onnx-pyannote-segmentation-3-0.tar.bz2`
- 埋め込み: タグ `speaker-recongition-models` ← **上流のタイプミスがURLに含まれる。修正しないこと**

埋め込みモデル候補（**日本語での優劣は未検証。Phase 1 で実測して選ぶ**）:
- 3D-Speaker `campplus` / `eres2net`（`zh_en_16k-common_advanced` 等の多言語）← 日本語では有望
- WeSpeaker `wespeaker_en_voxceleb_resnet34_LM`（英語 VoxCeleb 学習）
- NeMo `titanet_large`

---

## ASR プロバイダ

### 既定: ElevenLabs Scribe v2 (batch)

必要な3つが同時に成立し文書化されている唯一のプロバイダ:

1. **日本語の品質等級を公表** — 「Excellent（WER 5%以下）」。公表しているプロバイダ自体が少ない
2. **用語登録が実務規模** — `keyterms` 1,000語 × 50文字（1語あたり最大5単語）。Deepgram は約100語で実用にならない
3. **単語単位タイムスタンプ + 単語ごとの `logprob`** — 低信頼箇所を UI でハイライトする設計に直結

最短 100ms 対応でこのチャンク構成が想定内。$0.22/hr + keyterms $0.05/hr ≈ **$0.0045/分**。keyterms が別課金の従量機能である事実は、プロンプト連結ではなく**推論側で実際にバイアスしている**証拠と読める。

### 2番手: Azure AI Speech Fast Transcription

**壊れ方が違う**ものを意図的に選ぶ:

- **日本リージョン（Japan East / West）がある** — 日本企業の会議録音では調達要件になり得る。ElevenLabs も OpenAI も満たせない
- **`biasing_weight`（0.0〜2.0）** — 用語バイアスの強度ツマミを持つ唯一のプロバイダ
- 単語タイムスタンプが同じ形（`offsetMilliseconds` / `durationMilliseconds`）で trait を変形せずに済む
- 注意: **バッチ転写では phraseList が使えない**（Fast Transcription のみ）。$0.006/分で約1.6倍

### 既定にしないもの

- **OpenAI `gpt-transcribe`** — `keywords` パラメータは用途にぴったりで最安クラスだが、**タイムスタンプを一切返さない**。`whisper-1` に落とすしかなく、そちらは短チャンクで hallucination しやすい。第3の選択肢／コスト下限として保持
- **Deepgram Nova-3** — 最安・単語タイムスタンプ良好・短チャンク耐性が構造的に最も高いが、keyterm が **500トークン（約100語）** しか入らず用語集として不足
- **AssemblyAI** — 前世代が keyterms を6言語（日本語を含まない）に限定。未検証のまま選べない
- **Google Chirp 3** — `ja-JP` で adaptation が効き1,000語入るが $0.016/分と3.5倍、単語タイムスタンプに精度劣化の警告あり

### `Transcriber` trait の設計

- 出力は **セグメント + `Option<Vec<Word>>`**。単語タイムスタンプを必須にすると OpenAI 系が永久に挿せなくなる
- バイアスは `terms: Vec<String>` + `Option<f32>` の強度（Azure のみ消費）
- 用語リストは**プロバイダごとの上限で切り詰める**（1000 → 100）。エラーにせず優先度順に degrade

### ElevenLabs 実装・実測（追記）

`crates/asr-core/src/asr/elevenlabs.rs` として実装し、実際のAPIキーで実際の会議音声（23.6秒、単一話者区間）を送信して検証した。

**エンドポイント確定**: `POST https://api.elevenlabs.io/v1/speech-to-text`、`xi-api-key` ヘッダ、`multipart/form-data`（`file`=WAV, `model_id`="scribe_v2", `language_code`="ja", `timestamps_granularity`="word", `keyterms`=同名フィールド繰り返し）。音声は f32 PCM を16bit整数WAVへ変換して送信（`pcm_s16le_16` という生フォーマット名はドキュメント上あるが仕様が曖昧なため、確実なWAVコンテナ方式を採用）。

**実測結果**: 自然な日本語ビジネス会話として通る書き起こしが返り、2.02秒（実時間の約11.7倍速）で完了。

**重要な発見**: レスポンスの `words` は英語的な「単語」ではなく**日本語では1文字（かな1文字）単位**で返ってくる（例:「さ」「せ」「て」「い」「た」「だ」「く」...）。テキスト再構成（文字を連結するだけ）は単位に関わらず正しく動作するが、**将来の用語補正（読みベースのファジーマッチ）は「単語」ではなく「文字」単位で来る前提で設計する必要がある**——lindera側で形態素解析した単語境界と、ElevenLabs側の文字単位の結果を突き合わせる層が要る。

**`keyterms` の実測（TTSで作った制御されたテストケース）**: 「アシタカ総研」「クラウドゲート」「シャインテック」という3つの固有名詞を含む文をTTSで生成し、keytermsあり/なしを比較。

| 固有名詞 | keytermsナシ | keytermsアリ |
|---|---|---|
| アシタカ総研（純日本語） | ❌ 明日化総研 | ✅ アシタカ総研（安定して再現） |
| クラウドゲート（英語由来カタカナ） | ❌ Cloud Gate | ❌ Cloud Gate / CloudGate（単体でも直らず） |

**keytermsは確かに効くが万能ではない。** 純粋な日本語の固有名詞には安定して効く（同一リクエストの再実行でも再現）一方、**英単語由来のカタカナ複合語（日本のビジネス用語に多いパターン）には効果が無かった**——単体で渡しても英語表記のままだった。これは用語補正の**第2層（読みベースの後処理補正）が単なる保険ではなく、この種の用語には主力になる**ことを裏付ける実データ。

**未検証のまま残っている点**（docsの記載が曖昧、または未テスト）:
- ファイルサイズ上限（ドキュメント間で3GBと5GBの表記が矛盾）
- `timestamps_granularity` のデフォルト値
- 実際の会議音声（TTSでなく人間の自然発話）でのkeytermsの効果は未測定

### パイプライン全体（話者分離→チャンク分割→ASR）を実データで通しで実行（追記）

`asr_core::pipeline::run_transcription_stage` を実装し、`asr-cli transcribe-pipeline` で実際の230秒クリップに対して確定モード診断→チャンク分割（前後300msの実音声パディング）→ElevenLabs逐次呼び出し→オーバーラップのトリムを一気通貫で実行した。

**結果**: 51区間（cluster_threshold=0.9）を61.5秒で文字起こし。出力は自然な日本語のビジネス会話として通り、短い相槌（「ん」「うーん」）も含めて話者ごとに正しく割り当てられていた。「アシタカ」という単体テストで使った実在の社名も会話中に登場し、正しく認識されていた。

話者の内訳: `{0: 11, 3: 34, 4: 2, 5: 2, 6: 2}` — 話者3（長く話す側）と話者0（質問する側）が主要2話者、4/5/6は短い相槌のみの断片（実際の参加者数に対してやや過剰、DER 19.6%の実測と整合）。

**現時点の実装の制約**（既知、意図的）:
- **逐次実行**（並列化は別途の設計課題として後回し——プロバイダの同時実行数制限・チャンクごとのリトライ/エラー処理が絡むため）
- `context_prompt`（直前チャンクのテキストを次のプロンプトに渡す文脈維持）は未実装——ElevenLabs Scribe v2 batch にその用途のフィールドが見当たらなかったため
- 用語辞書（`terms`）との統合はまだ——`--terms` フラグはあるが `crate::terms` の読みベース補正とはまだ繋がっていない

---

## 必要な契約・アカウント

**MVP（Phase 0〜1）は契約ゼロで進められる。**

| | 必要なもの | 費用 | いつから |
|---|---|---|---|
| 話者分離・識別・VAD | **不要**（全ローカル、モデルは GitHub リリースから DL） | 無料 | — |
| ASR プロバイダ（どれか1つ） | ElevenLabs API または Azure サブスクリプション | 従量 | Phase 2 |
| Apple Developer Program | 社内配布するなら実質必須（署名 + notarization） | $99/年 | 配布時 |
| HuggingFace アカウント | pyannote 評価ベースラインを使う場合のみ（利用条件に同意） | 無料 | Phase 1（任意） |

契約不要なもの: `gpui-kit`, `sherpa-onnx`, `symphonia`, `rubato`, `lindera`, `cpal`, `signalsmith-stretch`, SQLite, ONNX モデル一式 — すべて OSS。

**最重要要件である話者分離の精度追い込みは1円も払わずに始められる。** 着手順序としても都合が良い。

ASR のコスト感: ElevenLabs で 1時間の会議 ≈ **$0.27**（40円程度）。Azure は $0.36/時間だが日本リージョンあり。100時間分処理しても $27 程度で、実質無視できる。

> 無料枠の有無と条件は未確認。契約前に各自で確認すること。

---

## リポジトリ構成

```
crates/
  asr-core/          # UI 非依存の純 Rust ライブラリ。ここが資産
    src/
      audio/         # symphonia デコード・rubato リサンプル・波形ピーク
      diarize/       # sherpa-onnx ラッパー（VAD + 分離 + 埋め込み）
      speaker/       # 話者 enrollment・照合・SQLite 永続化
      asr/           # Transcriber trait, elevenlabs.rs, azure.rs
      terms/         # 用語辞書・読み仮名補正・lindera ユーザー辞書生成
      pipeline/      # オーケストレーション
      store/         # Storage / Database trait と SQLite・ローカルFS 実装
  asr-cli/           # 評価・バッチ用 CLI（DER 計測）
  asr-audio/         # 再生エンジン（cpal + signalsmith-stretch）
apps/
  desktop/           # GPUI アプリ本体
  server/            # ※将来: axum + ジョブキュー
models/              # ONNX（初回起動時DL、gitignore）
eval/                # 評価データとスクリプト
```

`asr-core` に UI・GPUI・HTTP の都合を持ち込まない規律を守る。これがクラウド化の退路になり、MVP を UI の完成を待たず CLI だけで回せるようにする。

---

## データモデル（SQLite）

```
projects(id, name, created_at)
media(id, project_id, path, duration_ms, sample_rate, channels, sha256)
segments(id, media_id, start_ms, end_ms, speaker_id, text, original_text, asr_confidence, edited_at)
words(id, segment_id, start_ms, end_ms, text, confidence)
speakers(id, name, is_enrolled)                                  -- プロジェクト横断
speaker_embeddings(id, speaker_id, vector BLOB, source_media_id) -- 複数保持し重心を更新
terms(id, project_id, surface, reading_kana, category, weight, enabled)
term_aliases(id, term_id, surface, reading_kana)                 -- よくある誤変換
corrections(id, media_id, before, after, accepted, created_at)   -- 編集履歴 → 辞書の自動学習
```

`speakers` をプロジェクト横断にすることで会議をまたいだ人物同定ができる。sherpa-onnx の `SpeakerEmbeddingManager`（`add` / `search(emb, threshold)` / `verify`）は永続化機能を持たないので、**起動時に SQLite から読み込んで manager に流し込む**。

---

## 専門用語対応（3層）

単純な文字列置換では日本語の誤変換に勝てない。読み（かな）を軸に3段構えにする。

1. **ASR 前 — 用語をプロバイダに渡す**
   ElevenLabs なら `keyterms`（1,000語）、Azure なら `phraseList` + `biasing_weight`。重み順に並べプロバイダ上限で切り詰める。

2. **ASR 後 — 読み仮名ベースのファジーマッチ補正**
   - lindera で認識結果の読み（カタカナ）を取得、辞書側も `reading_kana` を持つ
   - N-gram 単位で読みの編集距離を取り、閾値超えを置換候補にする
   - 表記違いの誤変換を拾える（「機械学習」↔「機会学習」など）
   - 高信頼は自動置換、低信頼は UI にハイライトして人が確認。**ElevenLabs の単語 `logprob` が低い箇所を優先的に補正対象にする**

3. **編集からの学習**
   `before → after` を `corrections` に蓄積し、繰り返すパターンを `term_aliases` に自動登録提案する。

### lindera の OOV 問題と、その解決が用語辞書と一致すること

**lindera は辞書に無い語の読みを返さない**（`UNK` になる。形態素解析器一般の限界で実装を変えても同じ）。専門用語こそまさに OOV なので、素のままでは第2層が機能しない。

対策は**ユーザー辞書**（lindera は CSV でのユーザー辞書構築を正式サポート）。そして**これは `terms` テーブルが持つデータそのもの**なので、以下が自然に成立する:

```
terms テーブル（ユーザーが surface + reading_kana を登録）
  ├→ ASR API の keyterms として送る（第1層）
  └→ lindera ユーザー辞書 CSV を生成 → 読み取得が効く（第2層）
```

**用語を1回登録すれば、認識時のバイアスと後処理の照合の両方に効く。** 辞書は `lindera-unidic` か、6.0 で新規追加された `lindera-sudachidict`（OOV 耐性が最も高い見込み）。それでも UNK な語にはカタカナ／ローマ字のヒューリスティックを最終フォールバックに置く。

### 用語補正の実装・実測（追記）

**表記／読みの分離登録**: `Term { surface, reading_kana, ... }` に加え、`TermAlias { term_surface, alias_surface }` を新設した。「クラウドゲート」の keyterms 実測（前節）で判明した「英語由来カタカナ語は keyterms が効かない」問題は、そもそも読みが一致しない（ASR が `Cloud Gate` と英字のまま返す）ため第2層のファジーマッチも構造的に効かない。この場合だけ第3層（`apply_alias_corrections`、表記の完全一致による置換）が必要になる。

**lindera ユーザー辞書 CSV の形式が実装時の想定と違った**: 当初 IPADIC 風の13フィールド行を書いていたが、実行時に `LinderaError(kind=Content, source=Row 1 has 13 fields...)` で拒否された。`lindera-dictionary` 6.0 のソース（`user_dictionary.rs`）を読むと、UniDic の既定 `user_dictionary_schema` は単純な3フィールド `surface,reading,pronunciation` だった。存在しないコスト・品詞フィールドを書いていたのが原因。`build_user_dictionary_csv` を3フィールド出力に修正。

**単一トークンの読み一致では多形態素の誤変換を拾えない**: 「アシタカ総研」の実際の誤認識「明日化総研」を lindera でトークン化すると、前者は2トークン（アシタカ／総研）、後者は3トークン（明日／化／総研）に割れ、個々のトークンの読み（アス・カ・ソウケン）はどれも登録語の読み全体とは似ていない。**連結して初めて**近似する。単一トークン比較しかしない実装ではこの種の誤変換を原理的に拾えないと判明し、**スライディングウィンドウ**（`MAX_WINDOW_TOKENS = 6`、開始位置ごとに1〜6トークンの連結窓を全探索し、スコア降順・窓長優先でタイブレークして非重複に貪欲選択）を実装した（ユーザーが alias 登録のみで済ませる簡易策ではなくこちらを明示的に選択）。

**スライディングウィンドウが実際に既存の正しいテキストを壊すバグ**: 「本日はアシタカ総研の新製品です」（既に正しい）を通すと「本日アシタカ総研の新製品です」と「は」が消えた。原因は、候補生成ループが「窓の表記が登録語の表記と完全一致する場合はスキップ」していたこと——「もう正しいので何もしない」という意図だったが、これにより**完全一致の窓が競合候補として登録されなくなり**、たまたま隣の「は」まで飲み込んだ一回り大きい不完全一致の窓（読み類似度 約0.889、当時の閾値0.85は超える）が対抗馬なしで採用されてしまった。修正: 完全一致でもスキップせず候補化する（自然にスコア約1.0になるため、不完全な重複候補を正しく上回って阻止する）。修正後、この2つのテスト（多形態素誤変換の検出／既存の正しいテキストの非破壊）は両立して通る。

**実 API 相手に通しで実行して発覚した閾値のギャップ**: ユニットテストは上記の多形態素誤変換の検出を閾値0.7で実証していたが、CLI の既定 `correction_threshold` は0.85のままだった。実際に ElevenLabs API を叩いた `transcribe-pipeline` の通し実行（「本日は明日化総研の新製品クラウドゲートについてご説明します。」という実レスポンス）で確認したところ、この文における最良候補（`明日/化/総研` → 読み連結「アスカソウケン」、登録語の読み「アシタカソウケン」との類似度 **0.75**）は 0.85 を下回り、**実運用では一度も補正が発火していなかった**——スライディングウィンドウ実装のまさに動機になった実例が、本番設定では機能していなかったことになる。閾値を0.7（ユニットテストで安全性を確認済みの値）に下げて再実行し、「本日はアシタカ総研の新製品クラウドゲートについてご説明します。」と、「は」を保ったまま両方の固有名詞が正しく補正されることを確認した。

> 教訓は `cluster_threshold` のときと同型: **アルゴリズムが単体テストで正しく動くことと、本番の既定値でその効果が実際に発火することは別物であり、実データで通しの検証をしないと分からない。** 現在の 0.7 も単一の実例から選んだ値であり、より広いコーパスでの偽陽性率は未測定——今後の課題として残る。

### 用語登録あり／なしの再現率比較（追記）

Phase 2 の残タスクだった「登録あり/なしで再現率を比較」を実施した。macOS `say`（Kyoko, ja_JP）で「アシタカ総研」「クラウドゲート」を異なる文脈で計5文に散りばめたテスト音声（26.9秒）を新規合成し（`debug-data/recall_test/`、実録音ではなくTTS——既存の keyterms A/B テストと同じ手法）、同じ音声を `transcribe-pipeline` に**用語登録なし**と**登録あり**（keyterms + 読みベース補正 + alias補正のフル3層）の2条件で通した。

対象語の出現数は文の設計上、各4回（「アシタカ総研」4回・「クラウドゲート」4回、計8回）。

| 条件 | アシタカ総研 | クラウドゲート | 合計再現率 |
|---|---|---|---|
| 登録なし | 0/4（全て「明日化総研」）| 0/4（全て「CloudGate」）| **0/8（0%）** |
| 登録あり | 4/4 | 4/4 | **8/8（100%）** |

**内訳が設計通りに層ごとに分かれて機能したことも確認できた**: 「アシタカ総研」を救ったのは第2層（読みベースのスライディングウィンドウ——「明日化総研」の読み連結「アスカソウケン」が登録語の読みと0.7超で一致）。「クラウドゲート」を救ったのは第3層（`apply_alias_corrections`）——ASR の生出力が登録済みalias「CloudGate」（スペース無し）と**完全一致**したため。第2層は英字表記には構造的に効かない（クラウドゲート問題、前々節）ので、ここで第3層が実際に効いている実例が取れたことになる。

> サンプルサイズの注記: TTS音声・1話者・n=8出現という小規模な制御実験であり、実際の人間の自然発話・複数話者・より多様な言い回しでの再現率は未測定。それでも「用語を登録すると効く」ことを対照実験（同一音声・条件のみ変更）で定量的に示せたのは初めてで、0%→100%という差は3層設計全体の価値を裏付ける。

### 逆方向の登録（正しい表記がローマ字、読みが複数ありうるカタカナ）も無改修で動く（追記）

「正しい表記は `Cloud Gate`（英字）で、カタカナ読みが「クラウドゲート」「クラウドゲイト」のように複数考えられる場合も登録できるか」という疑問を検証した。`Term { surface: "Cloud Gate", reading_kana: Some("クラウドゲート") }` を登録するだけで、コード変更なしに両方とも正しく補正されることを確認した:

```
本日はクラウドゲートの件です → 本日はCloud Gateの件です
本日はクラウドゲイトの件です → 本日はCloud Gateの件です
```

理由は、lindera が「ゲイト」の読みを内部辞書上「ゲート」に正規化していたため（同一語彙として扱われる）。「クラウドゲイト」というテキストを解析しても、第2層が使う読みは「クラウド」+「ゲート」＝「クラウドゲート」となり、登録した読みとスコア1.0で完全一致する。表記のゆれ（カタカナ側）は「読み」を軸にする第2層が自然に吸収する——**alias（第3層）は英字⇄カタカナのように読みそのものが取れない場合の補完であり、カタカナの表記ゆれ同士なら基本的に不要**、という設計の理解が実例で裏付けられた。

**副次的に見つけた未使用のカラムを削除した**: `term_aliases` テーブルと `add_alias()` は alias 自身の `reading_kana` を受け取れるようになっていたが、`load_aliases()` は選択しておらず `TermAlias` 構造体にもフィールドが無い、書くだけで一度も読まれない死んだ列だった。全呼び出し元も実際には常に `None` を渡していた。alias は「登録済み表記へ丸ごと置換される、既知の間違った文字列」であって、alias自身に「正しい読み」という概念はそもそも当てはまらないため、機能を足すのではなく列とパラメータごと削除した（`add_alias(conn, term_id, alias_surface)` に簡素化、スキーマから `reading_kana` カラムを除去）。

---

## 話者識別（enrollment）

1. 分離した各クラスタの重心 embedding を計算
2. `SpeakerEmbeddingManager.search(emb, threshold)` で既知話者と照合、閾値超えなら自動命名
3. ユーザーが UI で命名したら embedding を `speaker_embeddings` に追加して重心を更新（使うほど当たる）
4. 閾値未満は `話者1` `話者2` のまま提示し、UI で命名を促す
5. 会議をまたいだ話者統合を後から行う場合のみ `kodama` で AHC する

### 実装後の実測で判明した事実（追記）

Phase 0/1 のコードを実際に動かして分かった、計画時点では見えなかった事実。

**`cluster_threshold` のデフォルト 0.5 は明確に悪い。** 実際の54分の会議録音（参加者4人）で検証したところ:

| cluster_threshold | 検出された話者数（先頭5分、正解は2〜3人）|
|---|---|
| 0.3 | 36 |
| 0.5（sherpa-onnx のデフォルト）| 22 |
| 0.7 | 11 |
| 0.8 | 6 |
| 0.9 | 5 |
| 0.95 | 4 |
| 0.99 | 3 ← 正解と一致 |

デフォルト値のまま54分の会議全体を処理すると **194種類の話者ID**（`speaker_419` のような数百番台まで出現）に分裂した。`0.5` という数字は sherpa-onnx のソース上のプログラム的デフォルトであって、根拠のある推奨値ではない。付属モデルの README・スクリプトにも推奨値の記載は無い。

**この閾値は埋め込みモデル固有のパラメータであり、普遍的な「標準値」は存在しない。** コサイン類似度の分布は埋め込みモデルごとのスケール・キャリブレーションに依存するため、`3dspeaker_speech_campplus` で見つけた「0.95〜0.99あたりが良い」という値は、埋め込みモデルを変えたら測り直しが必要。→ Phase 1 で複数モデルを比較する際は、モデルごとに閾値を再チューニングする前提で計画する。

**「UIで閾値スライダーを設ける」設計は、今の sherpa-onnx API では成立しない。** `OfflineSpeakerDiarization::process()` は「音声区間検出→埋め込み抽出→クラスタリング」を1回のC++呼び出しに不可分に束ねており、クラスタリング前の埋め込みを取り出す入口が無い（`set_config()` で閾値は変更できるが、次の `process()` はニューラルネット推論を含めて全部やり直しになる）。したがって:

- 閾値を1つ動かすたびに、54分の会議なら数分〜十数分のフル再計算が発生する
- これは「pyannoteパイプラインを自前実装しない」判断と表裏一体のトレードオフ——分離しようとすると結局そこに踏み込むことになる

**Phase 3 の UI 設計: ライブスライダーは「別の軽量な分離バックエンド」を作れば実現できるが、確定結果とは自動連携させない。**

`sherpa-onnx` の bundled API を諦めれば、以下の構成でライブスライダーは技術的に実現できる:

```
VAD で無音除去（sherpa-onnx の VAD は独立API、既に利用中）
  → 発話区間を固定長窓（例: 1.5秒窓・0.75秒ずらし）に分割
  → 窓ごとに埋め込みを抽出（SpeakerEmbeddingExtractor も独立API）
  → (窓の時間, 埋め込み) を一度だけキャッシュ ← ここだけが重い処理
  → 閾値を動かすたび: キャッシュ済み埋め込みを kodama で再クラスタリング ← 数百ベクトルの
    数値計算なので実質瞬時
```

ただし窓の切り方が pyannote の学習済み区間検出モデルより単純なため（無音を挟まない割り込み・複数人の同時発話の検出精度が劣る）、**確定モード（pyannote, 今の実装）と探索モード（VAD+固定窓）は別アルゴリズムであり、結果が微妙にずれ得る。**

検討の結果、**この2つを「プレビュー→自動確定」のように暗黙に繋げるのはやめる。** 探索モードの閾値を確定モードへソフトウェアが自動的に引き継ぐと、「プレビューで見た人数と本番の結果が違う」という不信感を生む（違うアルゴリズムなので当然ズレ得るのに、繋げてしまうと同じものであるかのように見えてしまう）。

**採用する設計**: 2つを完全に独立した選択式のモードとして UI に出す。

- **探索モード（速い・目安）**: VAD+固定窓、ライブスライダー。「だいたい何人くらいいそうか」を人間の目で確認するための道具。この結果を確定モードにソフトウェアが自動反映することはしない
- **確定モード（正確・低速、今の実装）**: pyannote 区間検出。閾値を変えると毎回フル再計算（数分〜十数分）
- 探索モードで見た数字を確定モードの初期値にしたい場合は、**人間が自分の判断で手入力する**（ソフトウェアが「これが最適です」と保証する体裁は取らない）

この設計なら実装も単純になる（閾値の自動変換ロジックが不要）。`Transcriber` trait と同じパターンで `Segmenter` trait を切り、`Accurate`（既存）/ `Fast`（新規、未実装）の2実装にする。

**スコープ注記**: 探索モード（VAD+固定窓+kodama再クラスタリング）は計画外の新規実装で、数日規模。Phase 1 の本題（確定モードでの精度チューニング）と並行するか後回しにするかは要判断。

### 探索モードを実装・実測した結果（追記）

`crates/asr-core/src/diarize/fast.rs` として実装し、実際の54分の会議録音で計測した。

**キャッシュ構築（重い処理・1回だけ）**: 127秒（実時間の25.8倍速）。同じ54分の確定モード（1254秒・2.6倍速）と比べて**約10倍速い**。3,516窓・192次元embeddingを生成。

**再クラスタリング（閾値を動かすたび）**: 実装1回目は **2,297〜4,530ms** で、これは「ライブ」と呼べる速さではなかった。原因はコサイン距離計算でベクトルのノルムを毎回のペア比較で再計算していたこと（n=3,516で約618万ペア）。

対策として①各埋め込みを事前に単位ベクトルへ正規化（内積だけでコサイン類似度になり、都度のノルム計算・除算・平方根が消える）②`rayon` で距離行列の構築を並列化、の2つを実施。結果、**213〜305msまで短縮**（約10倍）。

正直な評価: 真の「瞬時」（100ms未満）にはまだ届かないが、数秒待たされる致命的な遅さは解消した。デバウンス処理（動かし終わってから200-300ms後に確定）と組み合わせれば実用範囲。さらに詰める余地（距離行列を1本の配列に直接書き込み、中間 `Vec<Vec<f64>>` の確保を避ける等）はあるが、現時点で「実用に耐えるか」の判断材料としては十分。

副産物として、この探索モードでも**閾値0.8で話者数が実際の参加者数（4人）に収束**した。確定モードの最適値（0.95〜0.99）とは別の数値だが、これは窓の切り方が違うため閾値も別物になるという事前の想定通り。

### 正解ラベル作成機能と、実測DERによる確定モードのチューニング（追記）

これまでの実測は全て「話者数を目視で数える」水準だった。DERベースの厳密なチューニングにはground truthが要るので、`tools/diarization-viewer.html` に**正解ラベル作成モード**を追加した：動画を再生しながら 1〜9 キーで話者を切り替え（=その瞬間で区切る）、**0キーで無音を明示**、Backspaceで直前の区切りを取り消し、RTTMとしてダウンロード。

> 無音を明示するキー(0)は後から追加した。最初のバージョンは「キーを押した瞬間に前の話者が終わり次の話者が始まる」という作りで、**無音区間を表現できなかった**。これは実際に人がラベル付けしてみて初めて気づいた欠陥で、無音を表現できない正解データは「見逃し（missed detection）」を水増しする（VADベースの実際の診断は無音を正しく検出するのに、正解側は無音が存在しない体になっているため）。

**実測**: 実際の会議録音の先頭約230秒に人手でラベルを付け（無音明示あり）、`asr-cli diarize` の結果と比較。

| cluster_threshold | 見逃し | 過検出 | 話者混同 | **DER** |
|---|---|---|---|---|
| 0.5（旧デフォルト）| 24.48s | 11.63s | 25.32s | **29.2%** |
| 0.6〜0.7 | 24.40s | 10.82s | 21.11s | 26.8% |
| 0.8 | 24.26s | 10.89s | 10.75s | 21.8% |
| 0.9 | 24.26s | 10.91s | 6.18s | 19.6% |
| 0.95〜0.99 | 23.36s | 11.10s | 5.27s | 18.9%（頭打ち） |

閾値を上げるほど**話者混同が 25.3秒 → 5.3秒に激減**し、目視でつかんでいた傾向がそのまま厳密な数値で裏付けられた。0.95で頭打ちになる点も、全54分での目視結果（0.95〜0.99で4人に収束）と一致。

**`DiarizeParams::default()` の `cluster_threshold` を 0.5 → 0.9 に変更した**（プラトーの下端を採用——0.95よりわずかに分離側に振ることで、本当に別人の話者を過剰統合するリスクを避ける）。根拠は実測DERであり、目視ではない。

**次にチューニングすべき対象が入れ替わったこと**: 閾値最適化後は、残る誤りの主因が話者混同（5.3秒）から**見逃し（23.4秒）に交代**した。見逃しは閾値では直せない——短い相槌（「はい」等）を区間検出そのものが拾えていない可能性が高く、次は `min_duration_on` や VAD 側の感度が対象になる。

**`min_duration_on` を実測したが、ほぼ効かなかった。** CLIに `--min-duration-on`/`--min-duration-off` を露出し、cluster_threshold=0.9 固定で 0.1〜0.3 の範囲をスイープ:

| min_duration_on | 見逃し | DER |
|---|---|---|
| 0.1 | 23.84s | 19.9% |
| 0.15 | 24.08s | 19.8% |
| 0.2 | 24.26s | 19.8% |
| 0.25〜0.3 | 24.26s | 19.6% |

見逃しは23.8〜24.3秒でほぼ横ばい。「短い発話がduration filterで弾かれている」という仮説は外れた。

**見逃しの正体についての現時点の仮説**: ラベリングツール自体の限界（人間がキーを押すまでの反応遅延）が主要因である可能性が高い。短い発話・無音境界が多いほど、この遅延が積算されて見逃しとしてカウントされる。つまり残っている約23秒（全体の11%）の相当部分は**diarizerの欠陥ではなく測定側の誤差**である疑いが強い。

**方針**: `cluster_threshold=0.9`・**DER 19.6% を現時点のベースラインとして受け入れ、ここで一旦区切る。** これ以上このground truth（230秒・1本）で削ろうとしても、ラベリング精度の限界にぶつかるだけで実りが薄い。次にDERを追い込むなら、埋め込みモデルの比較（campplus vs WeSpeaker vs TitaNet）か、より丁寧に取り直したground truthが必要になる。

---

## 音声再生アーキテクチャ

要件は「シーク・区間ループ・**ピッチ保持のまま 0.5〜2.0倍速**・60fps でプレイヘッド」。

**WebView 案は仕様レベルで詰むので却下:**

| API | ピッチ保持 | 正確な区間ループ |
|---|---|---|
| `<audio>` | ✅ `preservesPitch` 既定 true | ❌ ポーリング + シークで不安定 |
| `AudioBufferSourceNode` | ❌ **リサンプルする**（0.5倍で22,050Hz） | ✅ `loopStart`/`loopEnd` |

両立できない。

**ネイティブ側も `rodio`・`kira`・`cpal` はいずれもピッチ保持の速度変更を持たない**（rodio: "Increasing the speed will increase the pitch by the same factor"、kira: "will change both the speed and pitch"）。`rubato` はサンプルレート変換であってタイムストレッチではない。

ただし **WSOLA を自分で書く必要はない**:

- **`signalsmith-stretch` 0.1.3**（MIT、C++ FFI）— `process()` / `seek()` に加え**プレイヘッド補正に必須の `output_latency()`** を持つ。上流は「0.75〜1.5倍が最も良い音」と明記しており、0.5倍/2.0倍は保証範囲外。音声（音楽でなく）なら実用上問題ない見込みだが**実際に聴いて判断する**
- **`timestretch` 0.15**（純Rust、MIT）— C++ ツールチェーン不要。ただし自称「EDM 最適化」で音声での挙動は未検証

```
symphonia（デコード）
  └→ PCM キャッシュ（ディスクに mmap。1時間 mono f32 @48k ≈ 690MB で RAM に載らない）
       └→ 自前 Transport（位置・ループ区間・レートの単一の真実）
            └→ signalsmith_stretch::process()（ここでレート適用、ピッチは 1.0 固定）
                 └→ cpal 出力ストリーム
```

**設計上の要点:**
- **トランスポートは自前で持つ。** タイムストレッチは時間軸そのものを変えるのでミキサのエフェクトとして置けず、ソースの内側に居る必要がある。rodio/kira ではこの位置に挟めない。`cpal` 直 + 自前リングバッファで 300〜500行
- **プレイヘッド** — オーディオスレッドが入力側サンプル位置を `AtomicU64` で公開し、UI スレッドは毎フレーム読んで **`output_latency()` を引く**。これを省くと波形カーソルが数十ms ずれてユーザーに気づかれる
- **毎フレームのポーリングはしない。** 再生開始時に一度読み、以降は単調増加クロックからローカルに外挿、500ms ごとに再同期
- **ループ** — 読み位置が終端を越えたら先頭に巻き戻し、`stretch.reset()` で継ぎ目の滲みを防ぐ
- **スクラブ** — `stretch.seek(preroll, rate)` で文脈を与えてから再生し、過渡アーティファクトを防ぐ
- **波形ピーク** — 取り込み時にズームレベルごとの min/max ピラミッドを事前計算。レンダーループで PCM を読まない

---

## 修正 UI の仕様（GPUI）

文字起こし修正は「聞き取り → 打鍵 → 少し戻す」の高速ループなので、キーボード操作が生命線。

**レイアウト（追記: `tools/diarization-viewer.html` の検証用レイアウトを正として採用）**

当初案は「上部:波形、中央:リスト、右:パネル」という縦積みだったが、検証用HTMLツールの2カラムレイアウトの評判が良かったため、そちらを踏襲する:

```
[ツールバー: ファイル/プロジェクト操作]
本体（横2カラム）
  ├─ 左カラム（固定幅目安42%）: 動画/音声プレイヤー
  └─ 右カラム（残り全部、縦積み）
       ├─ タイムライン（横スクロール、話者色分け区間 + プレイヘッド）— 固定高さ、小さめ
       ├─ 話者凡例（スウォッチ・リネーム・表示トグル）
       └─ セグメントリスト（`flex:1`で残り全部、縦スクロール、話者ラベル/時刻/テキスト）
            再生位置に追従してハイライト・自動スクロール。ヘッダはsticky。クリックでシーク
```

話者パネル・用語パネルは検証用HTMLには無かった要素——別タブ/オーバーレイにするか、右カラムをさらに分割するかは実装時に判断。

### キーボード — `Cmd` ベースに寄せる

IME が `Ctrl` 系キーを食う既知の問題（[zed#62764](https://github.com/zed-industries/zed/issues/62764) 複数ストロークキーバインドの横取り、[#54980](https://github.com/zed-industries/zed/issues/54980)）があるため、当初案の `Ctrl+Space` 等は危険。**macOS の IME は `Cmd` 修飾を消費しない**のでそちらに寄せる。

| キー | 動作 |
|---|---|
| `Cmd+Space` は OS 予約 → **`Cmd+Enter`** | 再生 / 一時停止（テキスト編集中でも効く） |
| `Cmd+←` | 数秒戻して再生 |
| `Cmd+↓` / `Cmd+↑` | 次 / 前のセグメントへ |
| `Enter` | カーソル位置でセグメント分割 |
| 行頭 `Backspace` | 前のセグメントと結合 |
| `Cmd+1`〜`Cmd+9` | 話者を割り当て |
| `Cmd+[` / `Cmd+]` | 再生速度 0.5x〜2.0x |

### 仮想スクロール — `gpui::list` を使う（`uniform_list` ではない）

- ❌ **`gpui::uniform_list`** — ドキュメント曰く「**uniform height** のアイテム用」。日本語テキストは折り返して行高が変わるので不可
- ✅ **`gpui::list` + `ListState`** — 可変高対応。各アイテムを個別に測定し、可視領域 + 調整可能な **overdraw** 領域を描画（スクロール時のちらつき防止に調整する）。`splice_focusable()` が存在することが、**編集可能な子要素を想定した設計**である直接の証拠
- ⚠️ **`gpui_base::v_virtual_list`（gpui-kit 側）は不可** — 全行の高さを事前に渡す必要があり、折り返しのある日本語テキストでは全セグメントの事前シェイプとウィンドウ幅変更ごとの再計算が発生する
- ⚠️ `ListState` は遅延測定なので**スクロールバーのつまみサイズが概算**になる。`measure_all()` は数千セグメントでは高コスト。文字数からの推定で代用するか、概算を許容する

### 行のパターン — フォーカス行だけを実エディタにする

N セグメントに N 個の `TextareaState`（`Rope` + undo manager を持つ）を作ってはいけない。

- 非フォーカス行 → 静的なスタイル付きテキスト（`SharedString` を持つ `div`）
- フォーカス行のみ → 実際の `gpui-kit::Textarea`
- クリック / `Cmd+↓` での移動時に、旧エディタを破棄してテキストをモデルに書き戻し、新しい行にエディタを生成してフォーカスを移す

これでトランスクリプトの長さによらず毎フレームのコストが一定になり、**IME は常に1つのエディタにだけ接続すればよくなる**。

> ⚠️ この行入れ替え時のフォーカス／IME 再接続は GPUI での前例が確認できなかった。**最初にスパイクで潰す対象**。

その他:
- **波形は GPUI のカスタム `Element` で自前描画**。事前計算したピークピラミッドを話者色付きで描く。GPU 直描画なので長時間音声でも滑らか
- `original_text` を保持し、編集済みセグメントを視覚的に区別する

---

## 評価基盤（MVP の中心）

「精度を追い込む」には測れることが前提。パイプライン実装が sherpa-onnx で済む分、ここに時間を使う。

1. **ラベリング UI** — 実録音に正解の話者ラベルを付ける。修正 UI の話者パネルを流用できるので追加コストは小さい
2. **`asr-cli eval`** — DER を算出し、**誤り内訳（false alarm / missed detection / speaker confusion）を分けて出す**。どれが効いているか分からないと改善できない
3. **Python の pyannote をベースラインとして併走** — `uv` で別 venv に入れ、同じ音声の DER を比較。自前パイプラインが正しく動いているかの判定に使う。**評価時のみ。製品には同梱しない**
4. 公開データセットで健全性を確認してから実録音でチューニング

**チューニング対象**（Phase 1 の実作業）:
- **埋め込みモデルの選定** — 3D-Speaker campplus/eres2net vs WeSpeaker vs TitaNet。**日本語での優劣は未検証なので実測で決める**
- クラスタリング閾値（`FastClusteringConfig.threshold`）
- `min_duration_on` / `min_duration_off`
- `window_shift_ratio`

---

## 実装フェーズ

### Phase 0 — 土台
- ワークスペース構成、`asr-core` の骨組み、モデル取得スクリプト
- symphonia 0.6 でデコード → rubato で 16kHz mono、波形ピークピラミッド生成
- `asr-cli` で音声情報を出せるところまで

### Phase 1 — 話者分離・識別（MVP の主眼）
- `sherpa-onnx` の VAD + `OfflineSpeakerDiarization` を配線（上流の Rust 例は約45行）
- `SpeakerEmbeddingManager` + SQLite 永続化による enrollment
- `asr-cli eval` で DER と誤り内訳を算出、pyannote ベースラインと比較
- **実録音で埋め込みモデル選定と閾値較正を回す** ← ここが本番
- 静的リンク・CPU で処理速度を計測し、CoreML(`shared`) が必要か判断

### Phase 2 — ASR と用語辞書
- `Transcriber` trait、ElevenLabs 実装、Azure 実装
- 同一話者区間の結合、前後オーバーラップ付与と重複トークン除去、並列リクエスト
- `terms` → keyterms 送出、`terms` → lindera ユーザー辞書生成、読みベース補正
- 用語ありなしで登録用語の再現率を比較

### Phase 3 — 編集 UI（GPUI）

**最初の1週間はスパイク2本だけに使う。製品コードを書き始めない。**

- **3a. 日本語 IME スパイク（2〜3日）— go/no-go 判定**
  `gpui-kit` の `Textarea` をウィンドウに置き、**macOS ことえり・Google 日本語入力・ATOK** の3つで検証する。確認項目: 未確定文字列の下線表示、変換候補ウィンドウの位置（`bounds_for_range`）、Space での変換、Shift+←/→ での文節再変換、`Enter` 確定、`Esc` 取消、undo の粒度。
  **[zed#62661](https://github.com/zed-industries/zed/issues/62661) の回避策を先回りで適用する** — `Application::new()` の前に `NSApplication sharedApplication` を強制する（AppKit 初期化順序によって候補ウィンドウが出ないまま `insertText:` に劣化するバグ。**単体の GPUI アプリで再現報告あり**）。
  併せて**行入れ替え時のフォーカス／IME 再接続**（前例なし）と**日本語フォントのフォールバック**（[zed#57777](https://github.com/zed-industries/zed/issues/57777) がグリフアトラスの bilinear フィルタが CJK に不利と指摘）も見る。

  #### 3a 実装計画（実ソースでAPIを検証済み）

  `apps/desktop` crate を新設し `gpui-kit = "0.6"` を依存に追加、ワークスペースメンバーに登録（`Cargo.toml`・`apps/desktop/Cargo.toml` 作成済み、`src/bin/ime_spike.rs` は空の雛形のみ）。`cargo fetch` して `~/.cargo/registry/src/.../gpui-kit-0.6.0` 以下を実際に読み、想定との差分を確認した:

  - **`gpui-kit` は内部で `gpui = { package = "gpui-pre", version = "0.3.1" }` に依存**しており、crates.io の停滞した `gpui 0.2.2` ではない実体を再エクスポートしている。計画時点の「`gpui-kit` 経由なら古い `gpui` を避けられる」という判断が実ソースで裏付けられた。
  - 起動パターン（`gpui-kit` の doc example通り、`gpui-kit-0.6.0/src/lib.rs` に実例あり）:
    ```rust
    gpui_kit::application().run(|cx| {
        gpui_kit::init(cx);
        cx.spawn(async move |cx| {
            cx.open_window(WindowOptions::default(), |window, cx| {
                let view = cx.new(|cx| ImeSpike::new(window, cx));
                cx.new(|cx| gpui_kit::component::Root::new(view, window, cx))
            })
            .expect("failed to open window");
        })
        .detach();
    });
    ```
    ウィンドウは `gpui_component::Root` でラップする必要がある（コンポーネントライブラリのポップアップ／コンテキストメニュー等がこれに依存する、doc example通りの構成）。
  - `Textarea`（`gpui_component::input::Textarea`、`gpui-component-0.6.0/src/input/textarea.rs`）は `Entity<TextareaState>` を受け取るだけの薄いラッパー。`TextareaState` の実体は `gpui_base::input::InputBaseState<TextareaMode>` の type alias（`gpui-base-0.6.0/src/input/textarea/mod.rs`）で、生成はウィンドウのある `Context` 内で `cx.new(|cx| TextareaState::new(window, cx))`（`window` は `open_window` のクロージャから借用）。
  - `gpui-kit-assets` はアイコン SVG のみ埋め込み（`RustEmbed`）。**日本語フォントはここに含まれない** — 計画通りアプリ側の対応が必要。まずは何も指定せず OS のフォールバックに任せて起動し、グリフが鮮明に出るか（zed#57777 の懸念）を先に目視確認する。
  - **zed#62661 の回避策を「先回りで適用」する当初方針を変更する**: `gpui-pre-macos`（`gpui-pre-macos-0.3.4/src/platform.rs`）は独自の `GPUIApplication`（`NSApplication` のサブクラス、`ClassDecl::new("GPUIApplication", class!(NSApplication))` で起動時に登録）を使っており、zed の元バグが報告された頃の実装から変わっている可能性がある。**投機的な修正を実測前に埋め込むのは「実測してから直す」というこのプロジェクトの一貫した方針に反する**ため、まず素の状態で動かして実際に候補ウィンドウが出るか確認し、出なければそのときに回避策を追加する。

  **実装・検証ステップ**:
  1. `apps/desktop/src/bin/ime_spike.rs` に上記の起動パターンで最小ウィンドウを実装。中身は `Textarea` 1つ＋現在のバッファ内容を表示するラベル（目視確認用）。
  2. `cargo run -p desktop --bin ime_spike` でビルド・起動を確認（ここまではこちらで自動検証できる）。
  3. **ここから先はユーザー自身による手動検証が必要**（IME変換は実際のキー入力とOS側の変換エンジンに依存し、ターミナルから自動操作できない）。チェックリスト:
     - 未確定文字列の下線表示
     - 変換候補ウィンドウの位置
     - Space での変換
     - Shift+←/→ での文節再変換
     - Enter確定 / Esc取消
     - undo の粒度
     - 日本語フォントのフォールバック・グリフの鮮明さ
     - ことえり・Google日本語入力・ATOK の3種で通しでチェック
  4. 結果に応じて go/no-go 判断。候補ウィンドウが出ない等の致命的な問題があれば zed#62661 型の回避策を追加して再検証。問題なければ 3b（再生エンジンスパイク）へ進む。

  #### 3a 実測結果 — go 判定

  **ビルドで別の実問題に遭遇した**: `gpui-pre-util 0.3.4`（`gpui-kit` が依存する Zed スナップショット系列の1つ）が、安定化されていない標準ライブラリ機能 `slice::as_array`（[rust-lang/rust#133508](https://github.com/rust-lang/rust/issues/133508)、`#![feature(...)]` 宣言なし）を直接呼んでおり、**stable Rust ではビルド不可能**だった。公開されている全バージョン（0.3.0〜0.3.4）で同じ行が存在することを実際に取得して確認済み——最近のリグレッションではなく最初から一貫している。

  対応として `apps/desktop` を nightly スコープにした:
  - ルートの `Cargo.toml` から `apps/desktop` を除外（`exclude = ["apps/desktop"]`）、`apps/desktop/Cargo.toml` に `[workspace]` を追加して独立したワークスペースルートにした（`crates/*` は引き続き stable のまま）
  - `apps/desktop/rust-toolchain.toml` で `channel = "nightly"` を指定
  - `rustup toolchain install nightly` 実行済み

  **一時的な誤診断とその訂正**: 最初にこの nightly 環境でビルドした際も同じ `E0658` エラーが出たため「nightly だけでは不十分で `RUSTC_BOOTSTRAP=1` が要る」と誤って判断した。しかし `target` を完全に削除してクリーンな状態から `cargo +nightly build` を実行し直したところ、`gpui-pre-util` は問題なくコンパイルされ、ビルド全体が成功した。**結論: `RUSTC_BOOTSTRAP` は不要。nightly ツールチェーンを `apps/desktop` にスコープするだけで十分。**

  > **根本原因が後日確定した**: このセッションの環境には `RUSTUP_TOOLCHAIN=1.92.0` という環境変数が設定されており、これが `apps/desktop/rust-toolchain.toml` によるnightly自動選択を毎回サイレントに上書きしていた（rustupの優先順位は 環境変数 > toolchainファイル > default）。`cargo +nightly build`（`+toolchain`は環境変数より優先される）を明示したときだけ本当にnightlyが使われていたため、「nightly単体では直らない」という一度目の誤診断も、後の「実は直る」という訂正も、**両方とも観測としては正しく、原因の特定だけが後回しになっていた**。**`apps/desktop` を触るときは常に `cargo +nightly ...` を明示すること**（`rust-toolchain.toml` の自動検出に任せない）。

  > 寄り道として、代替案も実測した。**`gpui-ce`**（Zed の gpui を外部消費者向けに再実装したコミュニティフォーク、1000+ stars、`cargo add gpui-ce` で使う設計）は実際に stable Rust で素直にビルドが通った（`gpui-pre` 系列に一切依存しない独立実装）。ただし `gpui-component`（Textarea 等の IME 対応済みウィジェット）は `gpui-pre` にしか対応しておらず、`gpui-ce` と組み合わせられない。また、ユーザーの別プロジェクト（Streaid）が `gpui = "0.2.2"`（素の公式crate）+ `gpui-component = "0.5.1"` で問題なく動いていることも確認した——`gpui-component` は 0.5.1 から 0.6.0 の間のどこかで依存先を素の `gpui 0.2.2` から `gpui-pre` 系列に切り替えており、これが両者の違いの直接の原因だった。今回は最終的に nightly スコープで `gpui-kit 0.6` / `gpui-component 0.6.0` をそのまま使う方針を継続した（最新の実装 + 解決済みの IME 対応を両取りできるため）。

  **IME 実機検証（ユーザー自身が実施）**: ことえり・Google日本語入力・ATOK で、未確定文字列の下線表示・変換候補ウィンドウの位置・Space変換・Shift+←/→ の文節再変換・Enter確定/Esc取消・undo の粒度・日本語フォントのフォールバックとグリフの鮮明さ、**全項目で問題なし**。zed#62661 の回避策は不要だった（先回りで適用せず様子見にした判断が正しかった）。

  **→ go。3b（再生エンジンスパイク）へ進む。**

- **3b. 再生エンジンスパイク（3〜4日）**
  `cpal` + `signalsmith-stretch` と `timestretch` を**実際の日本語音声**で 0.5×/0.75×/1.0×/1.5×/2.0× A/B する。レイテンシ補正後のプレイヘッド精度と、ループ継ぎ目のクリーンさを確認。

  #### 3b 実測結果 — go 判定

  `crates/asr-audio/src/bin/playback_spike.rs` を実装。`signalsmith-stretch`（`Stretch::process()`を cpal コールバックごとに呼ぶリアルタイム経路、ループ境界での`reset()`、`output_latency()`補正済みプレイヘッドのコンソール表示）と、`timestretch`（リアルタイムAPIが無い純オフライン変換——全体を`stretch()`で一括変換してから単純再生、あくまで音質A/B用で本番採用はしない）の両方を実データ（`debug-data/recall_test/recall_test.wav`）で動作確認。

  自己テストでは1.0倍速でのリアルタイム再生・ループ境界での巻き戻し・`timestretch`のオフライン変換が正しく動くことを確認（コールバックが`finished`後も空処理を繰り返しループ回数カウンタが暴走する小さなバグを発見・修正済み）。

  **人手による0.5×〜2.0×のA/B自体は、3aのIME検証ほど網羅的なチェックリスト消化ではなく「多分大丈夫」という軽めの確認だった。** 3aとは異なり項目ごとの明示的な合否は取っていない。**→ go とみなして本実装に進むが、後続の作業で違和感があれば再度ここに戻って比較する。**

その後に本実装:
- ~~波形 `Element`（ピークピラミッド + 話者色）~~ → **波形（振幅）自体は不要と判断、話者色分けタイムラインのみ実装（下記）**
- セグメントリスト（`list()`/`ListState` + フォーカス行入れ替えパターン、`overdraw` 調整）← **読み取り専用の第一弾を実装済み（下記）。フォーカス行入れ替えでの編集は未着手**
- 話者パネル / 用語パネル、`Cmd` ベースのキーマップ
- 書き出し（テキスト / SRT / VTT / Markdown）

#### 話者色分けタイムライン（追記、波形から差し替え）

最初に `apps/desktop/src/waveform.rs` としてピークピラミッド（振幅）ベースの波形を実装したが、**ユーザー判断で振幅波形そのものは不要**と分かった。実際の修正作業（テキストを直す）に必須なのはセグメントリスト側であり、波形は元々「シーク/プレイヘッドの視覚的手がかり」「話者色分けでの全体俯瞰」という補助目的でしかなかった——後者（話者色分け）だけ欲しい、単色ブロックで良い、という結論になったため、`waveform.rs`/`waveform_demo.rs` を削除し `apps/desktop/src/speaker_timeline.rs` に差し替えた。

**実装**: `asr_core::pipeline::TranscriptSegment`（`speaker: i32, start_ms, end_ms, text`。`Deserialize` を追加して実データJSONから読めるようにした）のリストと総尺を受け取り、時間軸に比例した位置に話者ごとの単色矩形を並べるだけ。ピークピラミッドは使わない。

**Element実装の知見（波形の時に確認済み、そのまま流用）**: フルの `Element` トレイトを自前実装する必要はない — `gpui-pre` の `canvas(prepaint_fn, paint_fn) -> Canvas<T>`（`Styled` 実装済み、`gpui-pre-0.3.4/src/elements/canvas.rs`）で低レベル描画だけ書けば済む。矩形描画は `window.paint_quad(fill(bounds, color))`。`Bounds<Pixels>` の `Pixels` は内部 `pub(crate) f32` なので `f32::from(pixels)` で変換する。色は `Hsla { h, s, l, a }`（自由関数 `hsla()` は無く、フィールドが `pub` なので構造体リテラルで直接組み立てる）。

`apps/desktop/src/bin/speaker_timeline_demo.rs` で実データ（`debug-data/pipeline_transcript.json`、実際の230秒クリップの本番パイプライン出力——51セグメント・5話者）を読み込んで描画するデモを作成・起動確認済み（後に `editor_layout_demo.rs` に統合、下記）。

#### セグメントリスト 第一弾 + 2カラムレイアウトの実装（追記）

`apps/desktop/src/segment_list.rs` として実装。**編集（フォーカス行入れ替えパターン）は次段階——今回は読み取り専用の行（話者ラベル/開始時刻/テキスト）まで。**

**`list()`/`ListState` の実API**（`gpui-pre-0.3.4/src/elements/list.rs` で確認）: `ListState::new(item_count, alignment: ListAlignment, overdraw: Pixels)` を自分のビュー構造体に保持し、`list(state, render_item)` の `render_item` は `FnMut(usize, &mut Window, &mut App) -> AnyElement` — 日本語テキストの折り返しで行高が変わっても問題ない可変高対応（計画通り `uniform_list` は使っていない）。セグメントデータは `Rc<Vec<TranscriptSegment>>` としてクロージャにキャプチャさせている（`ListState` 自体はデータを持たない、レイアウト計算用の中間状態のみ）。

**2カラムレイアウトへの差し替え**: 直前まで「上部:タイムライン、下:リスト」の縦積みだけで進めていたが、ユーザーから「検証用HTML（`tools/diarization-viewer.html`）のレイアウトが良かったので参考にしてほしい」との指摘があり、実際のCSSを読み直して**左カラム(42%幅): プレイヤー / 右カラム: タイムライン(固定高) + セグメントリスト(flex:1)**という2カラム構成に修正した（本セクション冒頭の「レイアウト」項目も更新済み）。`apps/desktop/src/bin/editor_layout_demo.rs` としてこの構成を実装——プレイヤーはまだプレースホルダー（asr-audioの再生統合は別タスク）。

実データ（同じ230秒クリップ・51セグメント）で起動確認済み、`cargo +nightly clippy --all-targets` も無警告。話者パネル/用語パネルは検証用HTMLに無かった要素なので、実装時に別途配置を判断する。

**テキストの折り返し（追記）**: 長いセグメントのテキストがタイムスタンプ列を押し込み、かつ見切れる問題をユーザーが発見。`.whitespace_normal()`（既定は `Nowrap` 相当）をテキスト列にのみ適用し、併せて `min_w(px(0.0))` を指定（flexアイテムは既定で `min-width: auto` になり、内容の幅より縮まない・折り返さないという定番の詰まりがあるため）。話者/時刻列は `flex_none()` で幅固定のまま維持。

#### 行編集パターン（フォーカス行入れ替え）— 前例なしと判定していたものを実装・検証（追記）

計画時点で「⚠️ この行入れ替え時のフォーカス／IME再接続はGPUIでの前例が確認できなかった。最初にスパイクで潰す対象」と明記していたパターンを、`editor_layout_demo.rs` 内で実装した（独立した専用スパイクではなく、動いているセグメントリストに直接組み込む形）。

**設計**: `EditorLayoutDemo`（ビュー本体）が `segments: Vec<TranscriptSegment>`・`focused_row: Option<usize>`・`editor: Option<Entity<TextareaState>>` を直接保持。非フォーカス行はクリックハンドラ付きの静的テキスト、フォーカス行だけ実際の `Textarea` — 計画通り「N個の`TextareaState`を同時に作らない」設計になっている。

**詰まった点とその解決**（実ソースで都度確認）:
- `on_click` は `Div` ではなく `Stateful<Div>` にしか生えていない（`StatefulInteractiveElement: InteractiveElement` トレイト）。`.id(("segment-row", ix))` で先に `Stateful<Div>` に変換してから `.on_click(...)` を呼ぶ必要がある
- クリックハンドラ（`Fn(&ClickEvent, &mut Window, &mut App)`）から**自分のビューを更新する**には、`render()` 内で `cx.weak_entity()` を取得してクロージャにキャプチャし、ハンドラの中で `weak.upgrade()` → `entity.update(cx, |this, cx| {...})` する（`Entity<T>::update` は `cx: &mut C where C: AppContext` を取るので `&mut App` から直接呼べる）。GPUIの標準的なコールバック→エンティティ更新の作法で、想定通り機能した
- 新しい `TextareaState` の作成時、既存テキストを流し込むのは `state.set_value(text, window, cx)`（undo履歴を汚さない専用メソッドが用意されている）。読み出しは `state.text()`（`&Rope`）→ `.to_string()`
- if/elseの両分岐で異なる具象型（`Div` vs `Stateful<Div>`）を返すと型エラーになるため、各分岐の中でそれぞれ `.into_any_element()` してから返す必要がある

**コミットのタイミング**: 現状は「別の行をクリックした瞬間」にのみ編集内容を確定する（`focus_row` が呼ばれた時に前の編集を書き戻す）。Enter確定・Esc取消・クリックアウト（リスト外クリック）での確定はまだ未実装——将来のキーマップ実装（`Cmd`ベース、計画の「キーボード」節）と合わせて追加する。

**ビルド・起動は確認済み（`cargo +nightly clippy --all-targets` 無警告）だが、実際にクリックして入力・IME変換・行の切り替えが問題なく動くかはユーザーによる手動確認が必要**——ここが本来「前例がない」と警戒していた核心部分なので、3aのIME検証と同様、実機での動作確認が欠かせない。

**→ ユーザーが実機確認し、動作は問題なし。ただし見た目の改善要望が2点あった（下記）。**

#### 行編集の見た目の改善（実装済み）

## Context

実機確認の結果、行編集そのもの（クリックでの切り替え・IME入力）は問題なかったが、見た目に2つの問題が見つかった:

1. **編集中のTextareaが背景白・テキストがグレーで視認性が非常に悪い**。原因は、アプリがダークテーマを明示的に有効化していないこと。`gpui-component` はテーマを明示しない限り既定でライトテーマになっており、我々の自前の黒背景UI（`bg(rgb(0x1e1e1e))` を各所で指定）の中に、テーマ由来のライトな `Textarea` だけが浮いて見える状態になっている。
2. **Textareaの高さが固定で、内容に応じて自動的に伸び縮みしない**。

いずれもピンポイントな修正で、`gpui-component-0.6.0` のソースで実際に該当APIを確認済み:

- **ダークテーマ**: `gpui_component::Theme::change(ThemeMode::Dark, None, cx)`（`gpui-component-0.6.0/src/theme/mod.rs`）を `gpui_kit::init(cx)` の直後に一度呼ぶだけでよい。テストコード内の実例（`init(cx); Theme::change(ThemeMode::Dark, None, cx);` という順序）で確認済み。個々のウィジェットに背景色を都度指定するより、テーマを揃える方が今後増えるコンポーネントにも一貫して効く。
- **自動高さ**: `InputBaseState<TextareaMode>` に `pub fn auto_grow(mut self, min_rows: usize, max_rows: usize) -> Self`（`gpui-base-0.6.0/src/input/base/state.rs:5222`）というビルダーメソッドが用意されている。`TextareaState::new(window, cx)` の直後に `.auto_grow(1, 20)` のように連結するだけ（`min_rows`〜`max_rows`の行数の範囲で内容に応じて伸びる）。

## 変更するファイル

- `apps/desktop/src/bin/editor_layout_demo.rs`
  - `main()` 内、`gpui_kit::init(cx);` の直後に `gpui_kit::component::Theme::change(gpui_kit::component::ThemeMode::Dark, None, cx);` を追加
  - `focus_row()` 内、`TextareaState::new(window, cx)` を `TextareaState::new(window, cx).auto_grow(1, 20)` に変更（`min_rows=1, max_rows=20` — セグメントは長くても数行程度なので余裕を持たせる）
- `apps/desktop/src/bin/ime_spike.rs`
  - 同じ根本原因（ダークテーマ未設定）が影響しているはずなので、一貫性のため同様に `Theme::change` を追加（任意だが安価な修正）

## 検証方法

`cd apps/desktop && cargo +nightly build --bin editor-layout-demo` でビルドし、既存プロセスを再起動して実機で見た目を確認してもらう。`cargo +nightly clippy --all-targets` も併せて確認。

**実装結果**: 上記2点を実装・確認（ダークテーマ、自動高さ）。ユーザーから追加で2点の指摘があり、同じ実装セッション内で追って修正した:

1. **クリックでTextareaに切り替わってもフォーカスが当たっていない**（もう一度クリックしないと入力できない）。`InputBaseState::focus(&self, window, cx)`（`gpui-base-0.6.0/src/input/base/state.rs:1184`、内部で`focus_handle.focus(window, cx)`を呼ぶ）を、`focus_row()`内で新しい`TextareaState`を`cx.new(|cx| {...})`する際に`set_value`の直後で呼ぶだけで解決——エンティティ生成クロージャの`cx`はそのエンティティ自身の`Context<Self>`なので、`state.focus(window, cx)`をそのまま呼べる。
2. **Textareaのフォントサイズが静的テキストより小さい**。原因は`gpui-component-0.6.0/src/sizing.rs`の`StyleSized::input_text_size()`——`Size::Medium`（Textareaの既定）が`.text_sm()`にマップされており、静的な`div()`テキストより小さい。個々のウィジェットの既定サイズに依存せず**両方に同じ明示的な`.text_size(px(14.0))`を指定**（`ROW_TEXT_SIZE`定数）して解決。`Textarea`は`Styled`を実装しているので`.text_size(...)`をそのままチェーンできる。

両方とも`cargo +nightly clippy --all-targets`無警告、実機で動作確認済み。

**追加修正: フォーカス時のカーソル位置を末尾にする**。`set_value()`のドキュメントに明記されている通り「複数行入力では選択範囲を`0..0`にリセットする」仕様のため、フォーカス直後はカーソルが先頭に来ていた。`InputBaseState`には`move_to_end`相当の直接呼べるpublicメソッドが無い（`movement.rs`の`move_to`/`move_to_end`はいずれも`pub(crate)`）ため、public APIの`set_cursor_position(position: impl Into<Position>, window, cx)`（内部で`focus`も呼ぶ）を使い、`RopeExt::offset_to_position(...)`で「テキスト末尾」のPositionを計算して渡す形で解決した。`RopeExt`は`gpui_component::input`から再エクスポートされている。

> **一度目の実装は日本語で末尾にならないバグを持っていた**（ユーザーが実機確認で指摘、「マルチバイトが原因では」という指摘も的中）。`offset_to_position(offset)`の`offset`は**バイトオフセット**（実装は`line_to_byte_idx`/`char_to_byte_idx`を使用）だが、最初`ropey::Rope::len_chars()`（**文字数**）を渡していた。UTF-8で1文字3バイトの日本語では、文字数はバイト数のおよそ1/3——結果、カーソルは末尾ではなく本文の途中（およそ1/3の位置）に置かれていた。`Rope::len()`（バイト長、ropeyのドキュメントに明記）に修正して解決。

**追加修正: フォーカスすると行の高さが変わる**。

一度目の対応（`.p_0()`をTextareaに追加）は**効果が無かった**——ユーザーの実機確認で判明。原因を`gpui-component-0.6.0/src/input/input.rs`で再確認すると、複数行入力（`is_multi_line()`）のpaddingは`Styled`の`refine_style`チェーンとは**別経路**で決まっていた: `Input::render()`が毎フレーム`state.set_editor_paddings(Edges { top: self.size.input_py(), ..., left: self.size.input_px(), ... }, cx)`を無条件に呼び、`self.size`（既定`Size::Medium`）に基づく固定paddingを**状態オブジェクトに直接**書き込んでいる。`input_px()`/`input_py()`は`Size::Medium`で各8px（`sizing.rs`）。この経路は外側の`Styled`チェーン（`.p_0()`等）が一切届かない——加えて`Textarea`自体（`textarea.rs`の構造体）は`Size`を変更する公開ビルダーメソッドを持たない（`Sizable`未実装）ため、**Textarea側の固定paddingを消す・上書きする手段が公開APIには存在しない**。

方針を反転し、**静的テキスト側にTextareaの固定padding（上下8pxずつ）と同じ量を追加**することで揃えた（`editor_layout_demo.rs`の静的テキストdivに`.py(px(8.0))`を追加）。行全体の高さは、編集/非編集どちらの状態でも同じ量のpaddingを持つことで安定する。

> **これでも高さが一致しなかった**（ユーザー確認: 「テキストエリアの方が行が狭くなった」）。原因は padding だけでなく、`Input::render()`が`.line_height(LINE_HEIGHT)`（`LINE_HEIGHT: Rems = Rems(1.25)`、`input.rs:369`）も自前で設定しており、静的テキスト側は line-height を明示していなかった（既定値のまま）ため、行の実高さがズレていたと判明。併せて水平方向（`input_px(Size::Medium) = 10px`）も揃っていなかった（ユーザーから追加指摘）。静的テキストdivに`.px(px(10.0))`と`.line_height(Rems(1.25))`を追加して両方揃えた。

> **それでもごくわずかにズレた**（ユーザー確認・「border分かも」という指摘も的中）。`Input`は既定で`appearance(true)`かつ`bordered(true)`のため`.border_1()`（1px枠線）が自前で付いており、枠線を持たない静的テキスト側との間にpx単位の非対称が残っていた。border付きの静的テキスト版を作るより、**`Textarea::new(&state).appearance(false)`でTextarea側の枠線・背景ごと消す**方を選んだ（ドキュメント通り「appearance=falseでborder/backgroundなし」になる）——今回のゴールは「行を編集モードでも同じ見た目にする」ことなので、装飾を足すより引く方が筋が良い。

#### フォーカス直後に高さが一瞬短くなる現象の解消（実装済み、`textarea-height-precompute`ブランチ→マージ済み）

編集モードに切り替わった瞬間、`auto_grow`が`min_rows`（＝1行）からスタートしてから実際の折り返し行数に更新されるまでの1フレーム分、行の高さが一瞬短くなる現象があった。原因は`InputBaseState::update_auto_grow(&mut self, display_map: &DisplayMap)`（`gpui-base-0.6.0/src/input/base/mode.rs:131`、実際の折り返し行数`display_map.wrap_row_count()`を使って`rows`を更新する）が**`pub(super)`で外部から呼べず**、かつ`DisplayMap`自体がその時点でまだ存在しない（最初のレイアウトパス後に作られる）ため。

**検討したが採用しなかった案**: 公開されている`set_rows(rows, cx)`で初期値を上書きする方法は、`AutoGrow`モードでは`rows`と同時に`max_rows`も同じ値に固定してしまう実装（`state.rs`）のため、それ以上育たなくなる副作用があり却下。「全行を最初から実Textareaにする」案も検討したが、仮想化リストでは画面外の行がそもそも実体化されないため同じ現象がスクロールのたびに起きるようになる上、全行の双方向同期が必要になり複雑化するだけなので却下。

**採用した方法**: `WindowTextSystem::shape_text()`という実際のテキストシェイピング・折り返しエンジンを使い、`.auto_grow(1, 20)`ではなく**実測に近い折り返し行数**を初期値として渡す（`.auto_grow(measure_wrapped_rows(...), 20)`）。行数は`shape_text()`が返す`WrappedLine`ごとの`wrap_boundaries().len() + 1`の合計から算出（`wrap_boundaries`は`gpui-pre-0.3.4/src/text_system/line_layout.rs`で`pub`）。折り返し幅（`wrap_width`）自体は実測ではなく、`window.viewport_size()`とこのレイアウトが使っている定数（`.p_4()`、左カラム42%、`.gap_4()`、行の`.px_2()`/`.gap_3()`、固定幅2列）から計算した近似値——ピクセル完全一致は保証されないが、文字数ベースの見積もりよりずっと実測に近い行数が出せる。

ユーザーが実機で確認し、体感できるレベルで改善（「バッチリ」）。安全のため`git checkout -b textarea-height-precompute`で別ブランチに切ってから実装し、うまくいったので`phase0-1-diarization-core`にマージした。

#### `Cmd`ベースのキーマップ（実装計画）

## Context

計画の「修正UIの仕様」節で決めていたキーマップのうち、セグメントリスト側で対応できる部分を実装する。実装前に実ソース（`gpui-pre-0.3.4`・`gpui-base-0.6.0`）を調べ、当初の想定と2点ズレが見つかったので、その場でユーザーに確認しスコープを決めた:

1. **再生系のキー（`Cmd+Enter`再生/一時停止、`Cmd+←`、`Cmd+[`/`Cmd+]`速度）はまだ動かせない** — `asr-audio`（signalsmith-stretch再生エンジン、3bスパイクで検証済み）がまだGPUIアプリに統合されていないため。→ **キーバインド自体は今回配線し、ハンドラは`tracing::info!`でログを出すだけのプレースホルダーにする**（再生統合時に中身を差し替えるだけで済む）。
2. **`Enter`でのセグメント分割・`Backspace`での前セグメント結合は、どちらも「普段の編集中に誤発火しやすい」というユーザー指摘で方針変更した**。
   - **`Enter`は技術的には`submit_on_enter`で実現できると分かったが、ユーザーから「これも安易に分割してしまいそうなので、もう少し特殊なショートカットの方が良い」との指摘**。→ **`Cmd+Shift+Enter`に変更**。これによりむしろ実装が単純になった: `submit_on_enter`は不要で、他のActionと全く同じ`.on_action()`パターンで拾える（`Cmd`付きの組み合わせは`Textarea`内部のEnterハンドラが処理しないキーストロークなので、素直に外側までbubbleする）。素の`Enter`・`Shift+Enter`は編集中も普段通り改行として機能し続ける。
   - **`Backspace`は当初「技術的に割り込み不可能」と判断し保留にしたが、`Cmd+Shift+Enter`と同じ理屈で解決できると判明**。素の`Backspace`は`InputBaseState::backspace()`（`gpui-base-0.6.0/src/input/base/state.rs:1479`）が無条件に自分で消費してしまうため確かに割り込めないが、ユーザー提案の**`Cmd+Shift+Backspace`**は事情が違う。実ソースで確認したところ`Textarea`はデフォルトで`"cmd-backspace"`を`DeleteToBeginningOfLine`に予約している（`state.rs:131`、`"backspace"`/`"shift-backspace"`/`"ctrl-backspace"`/`"alt-backspace"`も別のActionに予約済み、`state.rs:124-137`）が、**`"cmd-shift-backspace"`は既存バインディングに存在しない**——他のActionと同じ`.on_action()`パターンでそのまま拾える。→ **今回`MergeWithPrevious`として実装する**（保留の撤回）。

   > **方針**: 構造を変える操作（分割・結合）は、素のキー1つではなく`Cmd+Shift`系の複合キーに寄せる——普段のタイピングで偶発的に発火しないようにする。

## 実装する項目

| キー | Action | 実装内容 |
|---|---|---|
| `Cmd+↓` / `Cmd+↑` | `NextSegment` / `PreviousSegment` | `focused_row`が`Some`なら次/前のインデックスで`focus_row()`を呼ぶ。`None`なら先頭/末尾にフォーカス |
| `Cmd+Shift+Enter`（編集中） | `SplitSegment` | カーソル位置でテキストを2分割し、`segments`に新セグメントを1つ挿入。タイムスタンプは文字数比で按分（word単位のタイムスタンプがUI層に無いための近似、その旨コメントに明記） |
| `Cmd+Shift+Backspace`（編集中） | `MergeWithPrevious` | フォーカス中のセグメントを直前のセグメントに統合（`start_ms`=前者の開始、`end_ms`=後者の終了、`text`=前者+後者を連結、`speaker`=前者を継承）。先頭セグメントでは何もしない |
| `Cmd+1`〜`Cmd+9` | `AssignSpeaker1`..`AssignSpeaker9` | フォーカス中（無ければ実装上は何もしない）のセグメントの`speaker`フィールドを`0`〜`8`に上書き |
| `Cmd+Enter` | `PlayPause` | プレースホルダー（`tracing::info!("play/pause (playback not wired yet)")`） |
| `Cmd+←` | `SeekBack` | プレースホルダー |
| `Cmd+[` / `Cmd+]` | `RateDown` / `RateUp` | プレースホルダー |

## 実装の要点（実ソースで確認済みのAPI）

- **Action定義**: `gpui_kit::actions!(editor, [NextSegment, PreviousSegment, SplitSegment, MergeWithPrevious, AssignSpeaker1, ..., AssignSpeaker9, PlayPause, SeekBack, RateDown, RateUp])`（`gpui-kit-0.6.0/src/lib.rs`のマクロ、ペイロード無しのunit struct群のみ対応——話者番号ごとに9個の型が必要）。
- **キーバインド登録**: `cx.bind_keys([KeyBinding::new("cmd-down", NextSegment, None), KeyBinding::new("cmd-shift-enter", SplitSegment, None), KeyBinding::new("cmd-shift-backspace", MergeWithPrevious, None), ...])`（`App::bind_keys`、`gpui-pre-0.3.4/src/app.rs:2242`）を`main()`の`gpui_kit::init(cx)`直後で一度だけ呼ぶ。キーストローク文字列は`"cmd-down"`方式（`gpui-base`内の既存バインディング`KeyBinding::new("cmd-down", MoveToEnd, Some(CONTEXT))`で実例確認済み）。
- **ハンドラ登録**: ルートの`div()`に`.on_action(|action: &NextSegment, window, cx| {...})`を連ねる（`Interactivity::on_action`、`gpui-pre-0.3.4/src/key_dispatch.rs:333`）。ハンドラ内で`cx.entity()`/`weak`経由で`EditorLayoutDemo`を更新する、既存の`on_click`と同じパターン。`SplitSegment`/`MergeWithPrevious`も同様——どちらも`Textarea`内部のキーマップが処理しない組み合わせなので、素直にルートまでbubbleしてくる。

## 変更するファイル

- `apps/desktop/src/bin/editor_layout_demo.rs`
  - `actions!`でAction型を定義
  - `main()`で`cx.bind_keys([...])`
  - `EditorLayoutDemo`に`split_focused_segment`・`merge_with_previous`・`assign_speaker`・`focus_next`/`focus_previous`メソッドを追加
  - ルート`div()`に`.on_action(...)`を各Actionぶん追加

## 検証方法

`cd apps/desktop && cargo +nightly build --bin editor-layout-demo`（+ `cargo +nightly clippy --all-targets`）でビルド確認後、起動してユーザーに実機確認してもらう:
- `Cmd+↓`/`Cmd+↑`でフォーカス行が移動するか
- 編集中に`Cmd+Shift+Enter`でセグメントが2つに分かれるか（素の`Enter`・`Shift+Enter`は普段通り改行のままか）
- 編集中に`Cmd+Shift+Backspace`で直前のセグメントと結合されるか（素の`Backspace`・`Cmd+Backspace`は普段通りの削除のままか）
- `Cmd+1`〜`9`で話者ラベルが変わるか
- 再生系キー（`Cmd+Enter`/`Cmd+←`/`Cmd+[`/`Cmd+]`）はコンソールログが出るだけで実際には再生されないことを確認（想定通り）

#### 実装結果（追記）

上記の計画通りに実装（`apps/desktop/src/bin/editor_layout_demo.rs`）。ビルド時のエラーは2件のみ、いずれも些細:
- `apps/desktop`に`tracing`クレートが無かった（他の依存に無い）ので、再生系プレースホルダーのログ出力はこのファイルの既存スタイルに合わせて`println!`に変更
- `list()`のrender_itemクロージャが`move`で`weak`を完全に消費するため、ルート`div()`側の`.on_action()`群で同じ`weak`を再利用しようとして借用エラー。`root_weak = weak.clone()`を`list`構築より前に用意して分離することで解決

`SplitSegment`/`MergeWithPrevious`の実装で、タイムスタンプの扱いは計画通り単純な近似で済ませた: 分割は文字数比、結合は前セグメントの`start_ms`+後セグメントの`end_ms`をそのまま採用。`cargo +nightly build`/`clippy --all-targets`とも無警告。

**実機確認でバグ発見・修正**: 分割した際、新しい（2つ目の）行は正しくカーソル以降のテキストになるが、**元の（1つ目の）行が分割前の全文のままで内容が重複する**という報告。原因は`split_focused_segment()`内で、`segments[ix].text`を手動で分割済みテキストに書き換えた**後**に`self.commit_focused(cx)`を呼んでいたこと——`commit_focused`は「フォーカス中のTextareaの現在の内容を`segments`に書き戻す」処理なので、まだ古い（分割前の）全文を持ったままの生きたTextareaから読み直してしまい、直前に書き込んだ分割後のテキストを上書きしてしまっていた。修正: 分割後は`commit_focused`を呼ばず、`focused_row`/`editor`を直接`None`にしてから`focus_row(ix + 1, ...)`で新規Textareaを作る（`merge_with_previous`は逆に`commit_focused`を`segments`を触る**前**に呼んでいるため、この問題は無い）。

**追加要望: 分割後、新しい行のカーソル位置は先頭にしてほしい**（`focus_row`の既定は末尾——クリックでの通常フォーカス時にはそれが自然なため）。`lsp_types::Position::new(0, 0)`（`gpui_component::input`が`pub use lsp_types::Position;`で再エクスポート）を`state.set_cursor_position(...)`に渡すことで、`split_focused_segment`だけ`focus_row`直後にカーソルを先頭へ上書きする形で対応。`focus_row`本体の既定動作（末尾）は変更していない。

**追加要望: 分割やCmd+↓/↑でフォーカスが画面外の行に移った時、スクロールして見えるようにしてほしい**。`ListState`に`pub fn scroll_to_reveal_item(&self, ix: usize)`（`gpui-pre-0.3.4/src/elements/list.rs:677`、「アイテムが完全に見えるようスクロールする」）という公開APIがあり、`&self`だけで呼べる（`&mut`不要）。`focus_row()`の末尾に`self.list_state.scroll_to_reveal_item(ix);`を1箇所追加するだけで、クリック・`Cmd+↓/↑`・分割・結合——フォーカスが変わる全経路に一括で効く（`focus_row`が唯一の合流点になっていることを活かした）。

### Phase 4 — 任意
- クラウド版（`apps/server`）
- 編集履歴からの用語自動学習
- ローカル ASR フォールバック（`whisper-rs` + `metal`、optional feature）

---

## 検証方法

- **Phase 1**: `asr-cli eval --media <実録音> --ref <正解ラベル>` で DER と内訳を出し、pyannote ベースラインとの差が許容範囲か確認。処理時間が実時間の何倍速かも計測
- **Phase 2**: 用語辞書を有効/無効にして同じ音声を処理し、登録用語の再現率を比較。プロバイダを切り替えて同じ trait で動くことを確認
- **Phase 3**: スパイク2本を先に通す。その後、実録音を UI で開いて再生しながら**修正作業を通しで行う**。数千セグメントでスクロールが詰まらないこと、日本語入力が破綻しないことを確認
- 各フェーズで実録音を使った通し確認を行う。合成データだけで進めない

### 着手前に実測で潰すべき未検証事項

**ASR プロバイダ**（公式文書に記載がなく実際に叩くしかない）
1. ElevenLabs `keyterms` が日本語の漢字・カタカナ固有名詞に実際に効くか
2. Azure `phraseList` + `biasing_weight` の日本語での効き方
3. ElevenLabs のプラン別 STT 同時実行数

**話者分離**
4. 日本語での埋め込みモデルの優劣（3D-Speaker vs WeSpeaker vs TitaNet）
5. `FastClusteringConfig` で閾値モードを選ぶ方法（上流 C-API の慣習は `num_clusters = -1` だが Rust 側に記載なし）と各既定値
6. M1 での CoreML/ANE の実効果（LSTM 主体のためフォールバックの可能性）

**UI・音声**
7. ~~`gpui-kit` の**日本語**固有の IME 挙動~~ → **検証済み・go**（ことえり/Google日本語入力/ATOKの3種で全項目問題なし。「3a 実測結果」参照）
8. `gpui::list` で数千の可変高・フォーカス可能行を扱った際の実コスト（ベンチマーク前例なし）
9. `signalsmith-stretch` の 0.5×/2.0× での音質（上流の保証は 0.75〜1.5×）、および `.crate` が C++ サブモジュールを同梱しているか（オフラインビルドで確認）
10. `timestretch` の音声での品質（自称 EDM 最適化）
11. 日本語フォントのフォールバックとグリフの鮮明さ

**その他**
12. `lindera::Token::details()` の出力と `Schema::default()` 13フィールドの索引対応
