# 録音文字起こしアプリ 実装計画

## Context

会議・インタビューの録音を文字起こしするアプリケーションを新規に作る。

解決したい課題:
1. **処理速度** — 長時間の録音を実用的な時間で処理したい。コアは Rust。
2. **話者分離・識別** — 「誰が話したか」を自動で付けたい。**最重要**。
3. **専門用語・独自用語** — 事前に登録して認識精度を上げたい。
4. **修正 UI** — 音声を再生しながらテキストを直せるようにしたい。

| 項目 | 決定 |
|---|---|
| クラウド化 | 将来オプション。デスクトップ先行、`asr-core` を UI 非依存 crate にして退路を確保 |
| 外部 ASR API への音声送信 | 許容（精度と手間を優先） |
| 対象音声 | 日本語の会議・打合せ、インタビュー・1対1 |
| MVP の優先事項 | 話者分離・識別の精度を先に追い込む |
| UI フレームワーク | **GPUI**（`gpui-kit` 経由。WebView は不使用） |

環境: macOS 26 / Apple M1 / 16GB / Rust 1.92 / ffmpeg 8

---

## 設計上の核心的な判断

1. **話者分離は自前、ASR はクラウドに逃がす** — クラウドASRのdiarizationは録音をまたいだ人物同定ができない（`speaker_0`しか返さない）ため、話者埋め込みは自前で保持する。ASR本体は日本語精度・実装コストの点でクラウドが有利。

2. **話者分離パイプラインは自前実装せず `sherpa-onnx` に委ねる** — 当初案（`ort`上にpyannote 3.xを移植）は取り下げ。powersetデコード・窓またぎの順列不変結合・Kaldi特徴量パリティ・閾値較正、いずれも自前実装だと「なんとなく下手」という観測性の悪い失敗を生む。公式`sherpa-onnx`（v1.13.7、k2-fsa本家）が全工程実装済み。
   > 非公式`sherpa-rs`はアーカイブ済み・非推奨。`pyannote-rs`も停滞——依存しない。

3. **パイプライン順序は話者分離が ASR より前**（一般的な「ASRで全文書き起こし→diarizationとマージ」の逆）:
   ```
   音声 → デコード(16kHz mono) → VAD → 話者分離 → 同一話者区間の結合(無音<0.5sで連結、最大30-60秒)
        → 区間ごとにASR API（前後200-500msの実音声パディング、重複トークンはトリム）→ 用語補正 → セグメント列
   ```
   利点: 話者境界がASRより先に確定するので構造的に正確、再生同期タイムスタンプがAPI精度に依存しない、区間単位で並列化できる。短チャンクの精度低下は「そもそも話者/VAD境界で切っている」ことと前後パディングで対策。

4. **UI は GPUI ネイティブ一本、WebView は不使用** — `gpui-wry`はz順序なし・クリッピングなしという構造的制約がありこのアプリには使えない。`gpui-kit`がIME問題を解決するので、そもそもWebViewに逃げる必要がない。

---

## 技術選定（検証済み）

| 役割 | 採用 | 備考 |
|---|---|---|
| 話者分離・話者ID・VAD | `sherpa-onnx` 1.13.7 | 公式バインディング |
| 音声デコード | `symphonia` 0.6.1 | 0.6は破壊的リライト、0.5系の実例は使えない |
| リサンプル | `rubato` 5.0 | `Vec<Vec<f32>>`の旧APIは無い、`audioadapter`経由 |
| 形態素解析・読み | `lindera` 6.0 + `lindera-unidic` | |
| 階層クラスタリング | `kodama` 0.3 | 会議をまたぐ話者統合用 |
| ASR 既定 / 2番手 | ElevenLabs Scribe v2 (batch) / Azure AI Speech Fast Transcription | |
| UI フレームワーク | `gpui-kit` 0.6（Apache-2.0） | 実体は`gpui-pre`（Zedの最新gpuiの再公開）。crates.io公式`gpui 0.2.2`は約11ヶ月停滞していて別物 |
| テキスト編集・IME | `gpui-kit::Textarea` | |
| 仮想スクロール | `gpui::list` + `ListState` | 可変行高対応。`uniform_list`/`v_virtual_list`は不可（一様高さ前提） |
| 音声出力・タイムストレッチ | `cpal` + `signalsmith-stretch` 0.1.3 | トランスポートは自前で持つ |

**ライセンス**: Zedの`gpui`/`gpui_macos`はApache-2.0、`editor`等はGPL-3.0-or-later（AGPLではない）。社内利用のみなら`convey`が発生せず問題ないが、`gpui-kit`はそもそもGPLクレートを1つも要求しない構成（`editor`不使用、`cpal`+`signalsmith-stretch`直、Ropeも`gpui-kit`側）。

**バージョン地雷**: `symphonia`0.5→0.6で全面書き直し、`rubato`は8ヶ月で1.0→5.0、`lindera`は6週間で4.0→6.0、`ort`と`voice_activity_detector`は同居不可。GPUIはpre-1.0で頻繁に破壊的変更——バージョン厳密固定必須。

**macOS/CoreML**: `sherpa-onnx`既定（静的リンク）ではCoreMLが黙って無効化されCPU実行になる。ANE利用には`shared`featureが必要（署名の手間増）。方針: まず静的リンク・CPUで計測してから判断。

---

## ASR プロバイダ

**既定 ElevenLabs Scribe v2 (batch)** — 日本語品質「Excellent」を公表、`keyterms`1,000語×50文字という実務規模の用語登録、単語単位タイムスタンプ+logprob。$0.22/hr + keyterms $0.05/hr。

**2番手 Azure AI Speech Fast Transcription** — 日本リージョンあり（調達要件になりうる）、`biasing_weight`という用語バイアス強度ツマミ。バッチ転写では`phraseList`不可（Fast Transcriptionのみ）。

**既定にしないもの**: OpenAI `gpt-transcribe`（タイムスタンプ非対応）、Deepgram Nova-3（keyterm上限が低い）、AssemblyAI（keytermsが日本語非対応）、Google Chirp 3（コスト3.5倍）。

**`Transcriber` trait**: 出力は`セグメント + Option<Vec<Word>>`（単語タイムスタンプ必須にするとOpenAI系が挿せなくなる）。用語リストはプロバイダ上限で切り詰め（エラーにせずdegrade）。

### 実装・実測で分かったこと

- `crates/asr-core/src/asr/elevenlabs.rs`で実装。エンドポイント: `POST /v1/speech-to-text`、`multipart/form-data`、WAVコンテナで送信。
- レスポンスの`words`は英語的な「単語」ではなく**日本語では1文字単位**。将来の用語補正は文字単位前提で設計する必要がある。
- **`keyterms`は日本語の固有名詞には安定して効くが、英語由来のカタカナ複合語（例:クラウドゲート）には効果が無い**（「クラウドゲート問題」——単体で渡しても直らない）。これが3層用語補正の第2層が「保険」ではなく「主力」である根拠になった。
- `run_transcription_stage`で話者分離→チャンク分割→ASRの通し実行を実データ（230秒クリップ、51区間）で確認。逐次実行（並列化・`context_prompt`は未実装、後回し）。

未検証: ファイルサイズ上限（ドキュメント間で3GB/5GBの矛盾）、`timestamps_granularity`既定値。

---

## 必要な契約・アカウント

MVP（Phase 0〜1）は契約ゼロで進められる。話者分離・識別・VADは全ローカルで無料。ASRプロバイダ（ElevenLabs or Azure）のみPhase 2で従量課金——1時間の会議で約$0.27〜$0.36、実質無視できるコスト。Apple Developer Program（$99/年）は社内配布時に必要。

---

## リポジトリ構成

```
crates/
  asr-core/    # UI非依存の純Rustライブラリ（audio/diarize/speaker/asr/terms/pipeline/store）
  asr-cli/     # 評価・バッチ用CLI（DER計測）
  asr-audio/   # 再生エンジン（cpal + signalsmith-stretch）
apps/
  desktop/     # GPUIアプリ本体（nightly専用、独立workspace——下記参照）
  server/      # ※将来
```

`asr-core`にUI・GPUI・HTTPの都合を持ち込まない規律を守る。これがクラウド化の退路になる。

## データモデル（SQLite）

```
projects(id, name, created_at)
media(id, project_id, path, duration_ms, sample_rate, channels, sha256)
segments(id, media_id, start_ms, end_ms, speaker_id, text, original_text, asr_confidence, edited_at)
words(id, segment_id, start_ms, end_ms, text, confidence)
speakers(id, name, is_enrolled)                                  -- プロジェクト横断
speaker_embeddings(id, speaker_id, vector BLOB, source_media_id)
terms(id, project_id, surface, reading_kana, category, weight, enabled)
term_aliases(id, term_id, surface, reading_kana)
corrections(id, media_id, before, after, accepted, created_at)   -- 編集履歴 → 辞書の自動学習
```

`speakers`をプロジェクト横断にすることで会議をまたいだ人物同定ができる。`SpeakerEmbeddingManager`は永続化を持たないので起動時にSQLiteから読み込んで流し込む。

---

## 専門用語対応（3層・実装済み）

1. **ASR前** — `keyterms`/`phraseList`として送出（重み順、プロバイダ上限で切り詰め）
2. **ASR後・読みベースのファジーマッチ** — lindera読み取得 + スライディングウィンドウ（`MAX_WINDOW_TOKENS=6`、開始位置ごとに1〜6トークン連結窓を全探索、非重複貪欲選択）
3. **編集からの学習**（未着手） — `corrections`蓄積 → `term_aliases`自動登録提案

**lindera OOV対策**: ユーザー辞書 = `terms`テーブルそのもの（1回登録すればkeyterms送出とlindera読み取得の両方に効く）。UniDicのユーザー辞書CSVは3フィールド`surface,reading,pronunciation`（IPADIC風13フィールドではない、実装時に判明）。

**実測での主な教訓**:
- 単一トークンの読み一致では多形態素の誤変換（例:「アシタカ総研」→「明日化総研」）を拾えない → スライディングウィンドウで解決
- 候補生成で完全一致窓を早期スキップすると、隣接する不完全一致窓（助詞を巻き込む等）に負ける実バグを発見・修正済み
- CLIの`correction_threshold`既定0.85は実データで発火しないケースがあり0.7に変更（`cluster_threshold`と同型の教訓: 単体テストで動くことと本番既定値で発火することは別物）
- 登録あり/なし対照実験: TTS音声・n=8出現で**0%→100%**の再現率改善を確認（第2層と第3層がそれぞれ異なる語で実際に効いている内訳も確認）
- 表記がローマ字・読みが複数ありうるカタカナの逆方向登録も無改修で動作確認済み（linderaの読み正規化のおかげ）
- 未使用だった`term_aliases.reading_kana`カラムを削除（書くだけで読まれない死んだ列だった）

---

## 話者識別（enrollment、実装済み）

各クラスタの重心embeddingを`SpeakerEmbeddingManager.search()`で既存話者と照合、閾値超えで自動命名。ユーザーが命名したらembeddingを追加し重心を更新。

**DERチューニングの実測結果**（230秒・人手ラベル、無音明示あり）:

| cluster_threshold | DER |
|---|---|
| 0.5（旧デフォルト）| 29.2% |
| 0.8 | 21.8% |
| **0.9（採用）** | **19.6%** |
| 0.95〜0.99 | 18.9%（頭打ち） |

`cluster_threshold`のデフォルトを0.5→0.9に変更済み。`min_duration_on`はほぼ効かないと実測済み（残る見逃し約23秒はラベリング側の反応遅延が主因の疑い）。この閾値は埋め込みモデル固有——モデルを変えたら測り直しが必要。

**探索モード（`crates/asr-core/src/diarize/fast.rs`、実装済み）**: VAD+固定窓+kodama再クラスタリングによる高速プレビュー。キャッシュ構築127秒（確定モードの約10倍速）、再クラスタリングは正規化+rayon並列化で213〜305ms（当初2.3〜4.5秒から10倍改善）。確定モードとは別アルゴリズムなので閾値も別物（探索0.8 vs 確定0.9〜0.99）——**2つのモードは意図的に自動連携させない**設計（プレビューと本番がズレて見える不信感を避けるため）。

---

## 音声再生アーキテクチャ

要件: シーク・区間ループ・ピッチ保持のまま0.5〜2.0倍速・60fpsでプレイヘッド。WebView案（`<audio>`/`AudioBufferSourceNode`）はピッチ保持と正確なループの両立ができず却下。`rodio`/`kira`/`cpal`もピッチ保持速度変更を持たない。

**採用**: `signalsmith-stretch` 0.1.3（`process()`/`seek()`/`output_latency()`）。3bスパイクで実音声A/B確認済み（「多分大丈夫」水準、pixel-perfectな網羅チェックはしていない）。

```
symphonia（デコード）→ PCMキャッシュ → 自前Transport（位置・ループ・レートの単一の真実）
  → signalsmith_stretch::process()（レート適用、ピッチ1.0固定）→ cpal出力ストリーム
```

**設計要点**: トランスポートは自前（タイムストレッチはソースの内側に必要、rodio/kiraでは挟めない）。プレイヘッドは`output_latency()`補正必須（省くと数十msずれて気づかれる）。毎フレームポーリングせず外挿+500ms再同期。ループは`stretch.reset()`で継ぎ目防止。スクラブは`stretch.seek(preroll, rate)`。

---

## 修正 UI の仕様（GPUI）

**レイアウト**（`tools/diarization-viewer.html`の検証用レイアウトを正として採用——当初の縦積み案から変更）:
```
[ツールバー]
本体（横2カラム）
  ├─ 左（42%幅）: プレイヤー
  └─ 右: タイムライン（話者色分け、固定高） + セグメントリスト（flex:1、話者/時刻/テキスト）
```
話者パネル・用語パネルは検証用HTMLに無い要素——未着手、実装時に配置決定。

**キーマップ**（`Cmd`ベース、IMEが`Ctrl`系を食う問題を回避——実装済み分は✓）:

| キー | 動作 | 状態 |
|---|---|---|
| `Cmd+↓`/`Cmd+↑` | 次/前のセグメントへ | ✓ |
| `Cmd+Shift+Enter` | カーソル位置でセグメント分割 | ✓（当初案の素の`Enter`は誤発火しやすいため変更） |
| `Cmd+Shift+Backspace` | 前のセグメントと結合 | ✓（同上、素の`Backspace`は`Textarea`内部で消費されるため技術的にも不可だった） |
| `Cmd+1`〜`Cmd+9` | 話者を割り当て | ✓ |
| `Cmd+Enter` | 再生/一時停止 | ✓ |
| `Cmd+Option+←`/`Cmd+Option+→` | 5秒戻す/進める | ✓（当初`Cmd+←`のみだったが、下記の衝突により変更・実機確認済み） |
| `Cmd+[`/`Cmd+]` | 再生速度 0.5x〜2.0x | ✓ |

**Textareaキーマップとの衝突**（`gpui-base-0.6.0/src/input/base/state.rs`で実測）: `gpui_kit::init(cx)`の後に`cx.bind_keys()`する自前バインド（`context=None`）は、Textarea自身のcontext付きバインドに**勝つ**。つまり衝突した組み合わせはTextarea側の編集機能を黙って奪う（Textareaにフォーカスがある状態で`Cmd+←`がシークとして発火したことで実測確認）。

| 自前バインド | Textareaから奪うもの | 判断 |
|---|---|---|
| `Cmd+↑`/`Cmd+↓` | `MoveToStart`/`MoveToEnd`（テキスト全体の先頭/末尾へ） | 許容——セグメント移動の方が価値が高い |
| `Cmd+[`/`Cmd+]` | `Outdent`/`Indent` | 許容——文字起こし編集にインデントは不要 |
| `Cmd+Enter` | `Enter{secondary:true}` | 無害——`submit_on_enter`未設定なので実質何もしない |
| **`Cmd+Option+←/→`** | **なし**（Textareaのキーマップに存在しない） | ✓ 採用 |
| ~~`Cmd+←/→`~~ | `MoveHome`/`MoveEnd`（行頭/行末移動） | 却下——日本語編集で頻用 |
| ~~`Cmd+Shift+←/→`~~ | `SelectToStartOfLine`/`SelectToEndOfLine` | 却下——同上 |

> GPUIのキーストローク表記は`alt`のみ有効（`option`は`InvalidKeystrokeError`）——`"cmd-alt-left"`と書く。

**仮想スクロール・行編集パターン（実装済み）**: `gpui::list`+`ListState`で可変行高対応。非フォーカス行は静的テキスト、フォーカス行だけ実`Textarea`（N個の`TextareaState`を同時に作らない設計——実装・実機検証済み、前例のないパターンだったが問題なく動作）。

---

## 評価基盤

1. ラベリングUI（`tools/diarization-viewer.html`、実装済み）
2. `asr-cli eval` でDER算出＋誤り内訳（実装済み）
3. pyannoteベースライン併走（評価専用、製品には同梱しない）

**残っているチューニング対象**: 埋め込みモデルの選定（campplus vs WeSpeaker vs TitaNet、日本語での優劣は未検証）。

---

## 実装フェーズ

### Phase 0/1 — 土台・話者分離（完了）
`sherpa-onnx`配線、SQLite永続化enrollment、DERチューニング（cluster_threshold=0.9採用）、探索モード実装。

### Phase 2 — ASR と用語辞書（ほぼ完了）
`Transcriber` trait・ElevenLabs実装・3層用語補正・通しパイプライン、全て実データで実測・検証済み。**未着手**: Azure実装、リクエスト並列化。

### Phase 3 — 編集 UI（GPUI、進行中）

**3a. 日本語IMEスパイク → go**。実装は`apps/desktop`（nightly専用、独立workspace）。
> `gpui-kit`が依存する`gpui-pre-util`が未安定Rust機能を使用しておりstableではビルド不可——`apps/desktop`だけ`rust-toolchain.toml`でnightly指定（`RUSTUP_TOOLCHAIN`環境変数がこれを上書きすることがあるため、常に`cargo +nightly ...`を明示すること）。ことえり/Google日本語入力/ATOKの3種で全項目確認、zed#62661の回避策は不要だった。

**3b. 再生エンジンスパイク → go**。`crates/asr-audio/src/bin/playback_spike.rs`でsignalsmith-stretch（採用）とtimestretch（参考A/B用、リアルタイムAPI無し）を実データで検証。

**本実装（状態）**:
- ~~波形Element~~ → 話者色分けタイムラインのみ実装（振幅波形はユーザー判断で不要と判定）。`speaker_timeline.rs`、`gpui-pre`の`canvas()`ヘルパーで実装（フルElement自前実装は不要）
- セグメントリスト（読み取り専用→編集→見た目調整、全て完了）。`segment_list.rs` + `editor_layout_demo.rs`
- `Cmd`ベースのキーマップ（上表参照、完了）
- asr-audio再生エンジンのGPUI統合（実装済み、下記）
- タイムラインの強化＋再生中セグメントのハイライト（実装・実機確認済み、下記）
- 話者パネル（改名・統合・行単位修正、行単位ピッカーのフローティング表示含む） — 実装済み（下記）
- 用語パネル・書き出し（テキスト/SRT/VTT/Markdown） — **未着手**

#### asr-audio 再生エンジンのGPUI統合（実装済み）

`crates/asr-audio/src/transport.rs`（新規）— 3bスパイクの`play_signalsmith`を`Transport`構造体に一般化。`cpal::Stream`（保持のみ、ドロップで再生停止）+ `Arc<Shared>`（samples/位置`AtomicUsize`/`playing: AtomicBool`/レート`AtomicU32`（f32ビット）/`Mutex<Stretch>`）。公開API: `new`/`play`/`pause`/`toggle_play_pause`/`seek`/`seek_back`/`set_rate`/`rate`/`is_playing`/`position_ms`/`duration_ms`。

- `position_ms()`は`output_latency()`補正込み
- `seek()`は`Stretch::seek(preroll, rate)`にプリロール（`input_latency()`フレーム分）を与えて継ぎ目を減らす
- 終端到達で自動的に`playing=false`＋以降は無音を返す（スパイクで踏んだ「終了後もコールバックが回り続ける」バグと同型の対策を最初から組込み）
- `play()`は終端で止まっていたら位置0に戻してから再生

`apps/desktop/src/bin/editor_layout_demo.rs` — CLI引数`[json_path] [media_path]`（既定`../../debug-data/clip_230s.mp4`）、`transport: Arc<Transport>`保持、左カラムを▶/⏸＋`mm:ss / mm:ss`＋倍速表示に差し替え（クリックでも再生切替）、`cx.spawn`＋`cx.background_executor().timer(200ms)`で再生中のみ`cx.notify()`。

**実測**: ビルド・clippy警告ゼロ。`clip_230s.mp4`のデコード＋出力デバイス確保まで確認（`loaded audio: 230037ms`——JSONの229902msとほぼ一致）。ただしmp4全体のデコードに10秒以上かかる（起動が遅い）——将来的にストリーミングデコードか事前キャッシュを検討。

**follow-up**: シークのキーマップ修正（`SeekForward`未実装＋`Cmd+←`のTextarea衝突、`Cmd+Option+←/→`へ変更）は実装・実機確認済み（完了）。残る未着手: ループ再生UI。タイムラインのプレイヘッド表示・クリックシーク・再生中セグメントの自動ハイライトは次のタスクとして着手する（下記）。

#### タイムラインの強化＋再生中セグメントのハイライト（実装・実機確認済み）

検証用HTML（`tools/diarization-viewer.html`）を移植: ルーラー付きタイムライン（`timeline_ruler`、`niceTickInterval`移植）、ズームボタン（`pxPerSec`相当の`pixels_per_second`、既定40・×1.5/÷1.5・[2,800]クランプ）、`overflow_x_scroll`による横スクロール、プレイヘッド線、クリックシーク、セグメントリストの再生中行ハイライト（`rgba(0x5b9cf62e)`）。`apps/desktop/src/speaker_timeline.rs`・`editor_layout_demo.rs`に実装。

**実機で踏んだバグとその修正**:
- タイムラインの高さは検証時にユーザー希望で半分に縮小（74px→46px、ルーラー18px+ブロック28px）
- **一時停止中にアクティブ行を越えてスクロールできない**バグ——原因は、スクロール自体が`render()`を再実行させ、「アクティブ行が画面外にあるか」だけを条件に`scroll_to_reveal_item`を呼んでいたため、ユーザーが手動でスクロールした瞬間に毎回引き戻していた。`last_highlighted_ix: Option<usize>`を追加し、**現在再生中のセグメント自体が変わった時だけ**（再生が進んだ/シークした時）追従するよう修正——手動スクロールでは`current_ix`が変化しないため引き戻されなくなった。

#### 話者パネル（改名・統合・行単位の修正）（実装済み、実機確認中）

話者に名前を付けられるようにし（話者ごとに常時編集可能な`Textarea`を保持する`speaker_name_states: HashMap<i32, Entity<TextareaState>>`——`speaker_label()`がここから表示名を解決、空なら`S{id}`にフォールバック）、誤って分裂した話者の統合（`SpeakerMergeUi::PickingTarget`→`ConfirmingMerge`の2段階、確認なしでは実行されない——不可逆操作のため）、セグメント行の話者バッジをクリックした行単位の修正（`row_speaker_picker_open_for`、Cmd+1〜9とは独立、人数上限なし）を実装。両ピッカーは`speaker_option_row()`という共通コンポーネントで実装。左カラムはプレイヤー（`flex_none`）＋話者パネル（`flex_1`・`overflow_y_scroll`）の縦積みに変更。

**変更したファイル**: `apps/desktop/src/speaker_timeline.rs`（`speaker_color`を`pub`に）、`apps/desktop/src/bin/editor_layout_demo.rs`。ビルド・clippy警告ゼロ、実機起動確認済み。

**スコープ外（follow-up）**: SQLite永続化（`speakers`テーブルへの反映、プロジェクトをまたいだ話者名の記憶）、統合のUndo、話者の新規追加（現状は既存セグメントに出現する話者のみが対象）。

#### セグメント行レイアウトの修正（実装・実機確認済み）

時刻・話者名を1行目、本文（または編集中のTextarea）を2行目に配置する2行レイアウトに変更（話者名が可変長になったため、旧来の横3セル固定幅レイアウトでは長い名前が入り切らなくなったことへの対応）。`approximate_text_column_width()`（`focus_row`の初期行数プリ計算）も本文が単独行になったことに合わせて単純化。`apps/desktop/src/bin/editor_layout_demo.rs`のみ変更。ビルド・clippy警告ゼロ、実機起動確認済み。

#### 行単位修正ピッカーのフローティング化（実装済み、位置は実測補正値・原因不明のまま許容）

`deferred()`+`gpui_kit::base::Positioner::side()`でバッジ直下に浮かせる実装を行い、ビルド・clippyは通ったが、実機確認で位置がズレる上に**表示された瞬間に正しい位置へワープする**バグが発生（ユーザーがスクリーンショット付きで報告）。

**原因**: バッジの実座標を書き込む`badge_bounds: Rc<Cell<Bounds<Pixels>>>`への書き込みを`if row_speaker_picker_open_for == Some(ix) { ... }`で**開いている行だけ**に絞っていたため、クリックして開いた**その1フレーム目**は、まだ一度もこの行の座標が書かれていない状態（前回値＝ゼロ/デフォルト、または全く無関係な別行の値）でポップアップを位置決めしてしまい、誤った位置で一瞬表示 → 次に何らかの理由で再描画が起きた瞬間（このフレームのprepaintで初めて正しい座標が書き込まれるため）に正しい位置へ飛ぶ、という2段階の表示になっていた。

ユーザーへの確認の結果、**gpui-component側の既成コンポーネント（`Popover`/`DropdownMenu`は`Positioner::corner`でバッジに重ねる方式、`Select`/`Combobox`は`Positioner::side`を使うが専用の重い`SelectState`が必要）はどれも今回の用途に合わないため不採用**とし、**自前のフローティング実装のまま、座標書き込みのタイミングだけ修正**する方針で合意。

**修正設計**: 「開いている行だけ書き込む」条件をやめ、**表示されている全ての行が毎フレーム無条件に自分の座標を書き込む**ようにする——`badge_bounds`を`Rc<Cell<Bounds<Pixels>>>`（単一・条件付き書き込み）から`Rc<RefCell<HashMap<usize, Bounds<Pixels>>>>`（行indexごと・常時書き込み）に変更:
```rust
// バッジの on_prepaint（条件なし、全行毎フレーム）
.on_prepaint({
    let badge_bounds = badge_bounds.clone();
    move |bounds, _window, _cx| {
        badge_bounds.borrow_mut().insert(ix, bounds);
    }
})
// ポップアップ位置決め時の読み出し
let bounds = badge_bounds.borrow().get(&ix).copied().unwrap_or_default();
```
これでクリックされた時点で「その行は既に画面に表示されている＝直前のフレームで座標が書き込み済み」という状態が保証されるため、初回フレームからワープなしで正しい位置に表示されるはず（`gpui::list`は仮想化されているが、クリックできる＝現在レンダリングされている＝毎フレーム座標を書き込んでいる、という前提が常に成り立つ）。スクロール中はバッジの座標が動き続けるため1フレーム遅れが理論上あるが、連続的な動きなので体感できない見込み（今回のバグは「開いた瞬間の1回限りのワープ」という不連続な見え方だったのが問題で、連続的な追従ラグとは性質が異なる）。

`deferred`/`Positioner::side`/`.occlude()`/`on_mouse_down_out`によるフローティング自体の組み立てとdismiss処理は変更しない（バグの原因ではないため）。マップは行数分（現状51件程度）しか増えず、スプリット/マージでindexがズレても次のフレームで上書きされるだけなので実害はない。

**変更するファイル**: `apps/desktop/src/bin/editor_layout_demo.rs`のみ（`badge_bounds`の型変更、バッジの`on_prepaint`から条件分岐を削除、ポップアップ位置決めの読み出し先変更）。

**この修正後もまだズレていた（ユーザー実機報告・スクリーンショット添付）**: バッジのテキストに明示`text_size`/`line_height`を付けても改善せず、位置は依然「本文の下」——スクリーンショットで確認すると、ピッカーが**ヘッダー行の直下ではなく行全体（本文込み）の一番下、次の行の直前**に出ている。これは「バッジ自身の座標」ではなく「行全体の座標」を掴んでしまっていることを示す。

**根本原因（特定）**: `gpui_kit::base::ElementExt::on_prepaint`は内部で対象要素に`canvas(...).absolute().size_full()`を子として追加し、そのcanvasのprepaint時の実座標を読む仕組み（`gpui-base-0.6.0/src/element_ext.rs`）。`size_full()`は「親のサイズの100%」だが、バッジ自身はテキスト内容に応じた可変幅・自動高さ（`flex_none()`のみで明示的な`.h(px(_))`を持たない）——**明示的な確定サイズを持たない要素は、absolute配置の子にとって有効な基準（containing block）として扱われず、taffyのレイアウト解決が確定サイズを持つ祖先まで遡ってしまう**。この行では`row`（`.w_full()`）が最初に確定サイズを持つ祖先になるため、`size_full()`が指す範囲が「バッジ」ではなく「行全体」になり、結果としてバッジ自身ではなく行全体の下端を座標として拾っていた。

**修正（試みたが不十分）**: バッジ（および揃えるため時刻ラベルも）に明示的な`.h(px(16.0))`を追加し、確定サイズを持つ要素にした。ビルド後、実際に`badge_bounds`へ書き込まれる値をログ出力（`eprintln!`）で確認したところ、**捕捉座標自体は今回は正しくなっていた**（例: `origin: (715, 275), size: (16, 16)` — バッジ相当の小さい妥当な値）。それでもユーザー実機では依然ポップアップが約1行分下にズレて表示された——つまり「入力（バッジの実座標）は合っているのに出力（実際の描画位置）だけがズレる」状態。

**内部実装まで調査**: `gpui-base-0.6.0/src/positioner.rs`の`Positioner::prepaint`を読み、`offset = position.bounds.origin - bounds.origin`という差分を`window.with_element_offset`で子に適用する仕組みを確認。理論上は`Positioner`自身の`bounds`も、`trigger_bounds`（バッジの`on_prepaint`が捕捉した値）も、同じ`Window::layout_bounds()`（ローカル座標＋その時点の`element_offset()`）で計算されるため、`gpui::list`の仮想スクロールの中でも数式上は自己整合するはずだった。しかし実機ではズレが残り、GPUI本体のソースを読むだけではこれ以上の特定はできなかった——`deferred`+`Positioner::side`が`gpui::list`の仮想化スクロールと組み合わさる場面はライブラリ自身もテストしていない未検証の組み合わせであり、そこに何らかの前提の崩れがあると考えられる。

**最終対応（ユーザー合意）**: 根本原因の特定（`gpui-base`をローカルにパッチしてinternal printを仕込む等）は工数が大きいため見送り、**実測に基づく補正値で妥協**する方針で合意。`.offset(px(2.0))`を`.offset(px(-70.0))`に変更——観測された行の高さ（65〜85px程度）から「約1行分下にズレる」ぶんを差し引いた値。原因不明の経験的な補正であり、行の高さは内容によって変わるため全ての行で完全には合わない可能性がある——ウィンドウサイズやフォント設定を変えた場合は再調整が必要になりうる。デバッグ用の`eprintln!`は削除済み。

**変更するファイル**: `apps/desktop/src/bin/editor_layout_demo.rs`のみ（`badge_bounds`の型変更、バッジの`on_prepaint`から条件分岐を削除、ポップアップ位置決めの読み出し先変更、`Positioner::side`の`offset`を実測補正値に変更）。

**検証方法**: `cd apps/desktop && cargo +nightly build --bin editor-layout-demo`（+`clippy --all-targets`）後、実機で——(1) バッジをクリックした瞬間、ワープせず一発でバッジの近くに表示されるか (2) リストをスクロールしながら開閉しても位置が大きく崩れないか (3) 候補選択・外側クリックでの閉じる動作が引き続き機能するか。**実測済み・ユーザー承認済み（2026-09-08）**。

### Phase 4 — 任意（未着手）
クラウド版（`apps/server`）、編集履歴からの用語自動学習、ローカルASRフォールバック（`whisper-rs`）。

---

## 未検証のまま残っている項目

1. Azure `phraseList` + `biasing_weight` の日本語での効き方
2. ElevenLabsのプラン別STT同時実行数
3. 日本語での埋め込みモデルの優劣（3D-Speaker vs WeSpeaker vs TitaNet）
4. M1でのCoreML/ANEの実効果
5. `gpui::list`で数千の可変高行を扱った際の実コスト（ベンチマーク前例なし、現状51セグメントでは問題なし）
6. `signalsmith-stretch`の0.5×/2.0×での音質の網羅的な検証（3bは「多分大丈夫」水準の軽い確認のみ）
7. `timestretch`の音声での品質（自称EDM最適化、参考A/B用）
