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

1. 話者分離は自前（`sherpa-onnx` 1.13.7、公式バインディング。自前実装は不採用）、ASRはクラウド（ElevenLabs）——クラウドASRのdiarizationは録音をまたいだ人物同定ができないため。
2. パイプライン順序は**話者分離→ASR**（一般的な逆）。話者境界が先に確定するので構造的に正確、区間単位で並列化できる。
3. UIはGPUIネイティブ一本、WebView不使用（`gpui-wry`はz順序・クリッピングの構造的制約あり、`gpui-kit`がIME問題を解決するので不要）。

## 技術選定

| 役割 | 採用 | 備考 |
|---|---|---|
| 話者分離・話者ID・VAD | `sherpa-onnx` 1.13.7 | |
| 音声デコード | `symphonia` 0.6.1 | |
| リサンプル | `rubato` 5.0 | |
| 形態素解析・読み | `lindera` 6.0 + `lindera-unidic` | |
| 階層クラスタリング | `kodama` 0.3 | 会議をまたぐ話者統合用 |
| ASR | ElevenLabs Scribe v2 (batch) | Azureは対応しない方針に確定（ElevenLabs一本で運用） |
| UI フレームワーク | `gpui-kit` 0.6（Apache-2.0） | 実体は`gpui-pre`。crates.io公式`gpui`とは別物 |
| 仮想スクロール | `gpui::list` + `ListState` | 可変行高対応 |
| 音声出力・タイムストレッチ | `cpal` + `signalsmith-stretch` 0.1.3 | |

**バージョン注意**: GPUIはpre-1.0で頻繁に破壊的変更——バージョン厳密固定必須。`ort`と`voice_activity_detector`は同居不可。

---

## ASR プロバイダ

**ElevenLabs Scribe v2 (batch)**（`crates/asr-core/src/asr/elevenlabs.rs`）——`keyterms`1,000語×50文字、$0.22/hr。Azure AI Speechは2番手候補として検討していたが、対応しない方針に確定した（ElevenLabs一本で運用）。

**実測で判明した重要な制約**: レスポンスの`words`は日本語では1文字単位。**`keyterms`は英語由来のカタカナ複合語（例:クラウドゲート）には効かない**——3層用語補正の第2層が「保険」ではなく主力である根拠。

---

## リポジトリ構成

```
crates/
  asr-core/    # UI非依存の純Rustライブラリ（audio/diarize/speaker/asr/terms/pipeline/store）
  asr-cli/     # 評価・バッチ用CLI（DER計測）
  asr-audio/   # 再生エンジン（cpal + signalsmith-stretch）
apps/
  desktop/     # GPUIアプリ本体（nightly専用、独立workspace）
```

`asr-core`にUI・GPUI・HTTPの都合を持ち込まない規律を守る。これがクラウド化の退路になる。

## データモデル（SQLite）

```
projects(id, name, created_at)                                   -- 1プロジェクトにmediaは複数可（下記「今回の対応範囲」参照）
media(id, project_id, path, duration_ms, sample_rate, channels, sha256)
segments(id, media_id, start_ms, end_ms, speaker_id, text, original_text, asr_confidence, edited_at)
words(id, segment_id, start_ms, end_ms, text, confidence)
speakers(id, name, is_enrolled)                                  -- プロジェクト横断
speaker_embeddings(id, speaker_id, vector BLOB, source_media_id)
terms(id, project_id, surface, reading_kana, category, weight, enabled)
term_aliases(id, term_id, surface, reading_kana)
corrections(id, media_id, before, after, accepted, created_at)   -- 編集履歴 → 辞書の自動学習（未着手）
```

---

## 専門用語対応（3層）

1. ASR前 — `keyterms`として送出
2. ASR後・読みベースのファジーマッチ — lindera読み取得 + スライディングウィンドウ（`MAX_WINDOW_TOKENS=6`）
3. 編集からの学習（未着手） — `corrections`蓄積 → `term_aliases`自動登録提案

**ユーザー辞書 = `terms`テーブルそのもの**（登録すればkeyterms送出とlindera読み取得の両方に効く）。`correction_threshold`は0.7（実データでは既定0.85だと発火しないケースがあった）。`terms.project_id`は「プロジェクト」（＝用語辞書＋複数の文字起こしを束ねる上位概念、下記「今回の対応範囲」参照）にスコープされる——**用語は必ずASR実行前、既存プロジェクトに対して登録する**設計（プロジェクトはアップロード前から存在するので、この順序制約は自然に満たされる）。

---

## 話者識別（enrollment）

重心embeddingを`SpeakerEmbeddingManager.search()`で既存話者と照合、閾値超えで自動命名。**`cluster_threshold` = 0.9**（実測: 0.5=DER29.2% → 0.9=DER19.6%、0.95以降は頭打ち）——埋め込みモデル固有の値、モデルを変えたら測り直しが必要。

探索モード（`crates/asr-core/src/diarize/fast.rs`）は別アルゴリズム・別閾値（0.8）——確定モードと意図的に自動連携させない。

---

## 音声再生アーキテクチャ

要件: シーク・区間ループ・ピッチ保持のまま0.5〜2.0倍速。**採用**: `signalsmith-stretch` 0.1.3（自前`Transport`で位置・ループ・レートを一元管理、`rodio`/`kira`はピッチ保持速度変更を持たないため不採用）。プレイヘッドは`output_latency()`補正必須。ループは`stretch.reset()`で継ぎ目防止。

---

## 修正 UI の仕様（GPUI）

**レイアウト**: ツールバー＋横2カラム（左: プレイヤー、右: タイムライン＋セグメントリスト）。

**キーマップ**（`Cmd`ベース、IMEが`Ctrl`系を食う問題を回避）:

| キー | 動作 |
|---|---|
| `Cmd+↓`/`Cmd+↑` | 次/前のセグメントへ |
| `Cmd+Shift+Enter` / `Cmd+Shift+Backspace` | セグメント分割/結合 |
| `Cmd+1`〜`Cmd+9` | 話者を割り当て |
| `Cmd+Enter` | 再生/一時停止 |
| `Cmd+Option+←`/`Cmd+Option+→` | 5秒戻す/進める |
| `Cmd+[`/`Cmd+]` | 再生速度 0.5x〜2.0x |
| `Escape` | 編集モードを抜ける（テキストはコミット）／開いているピッカーを閉じる |
| `Cmd+Z`/`Cmd+Shift+Z` | セグメント単位のUndo/Redo（**編集中は`Textarea`自身の文字単位Undo**） |

> `Textarea`自身のキーマップと衝突する組み合わせ（`Cmd+←/→`等）は避けている——自前バインドが常に勝つため、衝突するとTextarea側の機能を黙って奪う。新しいキーバインドを足す際はこの衝突に注意。GPUIのキーストローク表記は`alt`のみ有効（`option`ではない）。
>
> **もう一つの衝突源: macOS側のシステム/ウィンドウ標準ショートカット**。`Cmd+Shift+[`/`Cmd+Shift+]`はSafari/Terminal含むほぼ全てのCocoaアプリで「前/次のタブに切り替え」の標準ショートカットになっており、ループ再生UI実装時にこれを使ったところ実機で**キーイベントが一切GPUI側に届かない**（`on_action`が発火しない）という形で発覚した——`Textarea`との衝突と違って、こちらはアプリ内のキーマップテーブルを見ても分からず、実機で押してみて初めて気づける類のもの。ブラケット系（`[`/`]`）の組み合わせを新設する時は特に要注意、代替（例: IN/OUT点なら`I`/`O`）を検討する。
>
> 衝突を「避ける」以外の手段が2つある。(a) **`Some("!Input")`スコープ**——`Cmd+Z`はこれで、Textareaにフォーカスがある間はこちらが不成立になり、Textarea自身のバインドが素通りする。(b) **相手のアクションに相乗りする**——`Escape`はこれで、`gpui-base`側のハンドラが最後に`cx.propagate()`する（`gpui-base-0.6.0/src/input/base/state.rs:1663`）ため、祖先要素に`.on_action::<Escape>()`を置くだけで新しいバインドを足さずに拾える。

**行編集パターン**: `gpui::list`+`ListState`。非フォーカス行は静的テキスト、フォーカス行だけ実`Textarea`（N個の`TextareaState`を同時に作らない設計）。

---

## 評価基盤

ラベリングUI（`tools/diarization-viewer.html`）、`asr-cli eval`でDER算出。**残タスク**: 埋め込みモデルの選定（campplus vs WeSpeaker vs TitaNet、未検証）。

---

## 実装フェーズ

優先度の低い将来機能（クラウド版、編集履歴からの用語自動学習、ローカルASR
フォールバック）は[FEATURE.md](../FEATURE.md)に切り出し済み——このTODO
からは意図的に外してある。

### Phase 2 — ASR と用語辞書
**未着手**: リクエスト並列化。Azure AI Speechは対応しない方針に確定（ElevenLabs一本で運用）。

### Phase 3 — 編集 UI（GPUI、進行中）

完了: セグメントリスト・タイムライン・話者パネル・再生エンジン統合・行単位話者ピッカー・アップロード画面+実パイプライン統合・APIキー設定（Keychain、`Projects`/`Upload`どちらからも開ける独立の`AppScreen::Settings`）。詳細は削除済み——`run_full_pipeline`のAPIは`crates/asr-core/src/pipeline/mod.rs`、APIキーは`apps/desktop`の`load_api_key`/`save_api_key`参照。

**まだ手を付けていないスコープ**（将来）: 話者統合のUndo時にDBの削除済み行を再作成しない既知のギャップ（「話者の追加機能」参照、同種のトレードオフとして許容）、cross-project話者enrollmentのGUI配線。話者の新規追加は実装済み（上記「実機確認済み」参照）。

---

## 実機確認済み（完了）

以下は全て実装済みで、ユーザーによる実機確認も完了している。

### 話者の追加機能（実装済み）

**Context**: 話者分離が本来2人に分けるべきところを1人にまとめてしまった場合の対策として、ユーザーが手動で「セグメント0件の新しい話者」を追加できるようにし、その後は既存の行ピッカー（バッジクリック）や`Cmd+1`〜`9`で個別のセグメントをその新しい話者へ再割り当てできるようにする。

**現状の制約（調査で判明、設計に直結する）**:

1. **`distinct_speakers`は完全にセグメント由来**（`app.rs:1008-1038`、`render()`内）——`self.segments`に実際に登場する話者indexだけを`BTreeSet`で集めて`Vec<i32>`にしている。この**同じループの中で**、まだ`speaker_name_states`に無い話者のTextareaStateを遅延生成している（`:1010-1035`）。パネル表示（`:1664`の`for &speaker in &distinct_speakers`）はこの`speaker_name_states`に対して`.expect("ensured earlier in render()")`（`:1666`）で直接アクセスしているため、**この集合を広げずにパネル側だけ広げるとパニックする**——集合を広げる変更は「遅延生成ループ」と「消費側」の両方に同時に効く1箇所（`:1009`の`BTreeSet`構築）で行う必要がある。
2. **Cmd+1〜9（`assign_speaker`）は`distinct_speakers`を経由せず、index N-1に直接割り当てている**（`:587`、`AssignSpeaker1`〜`9`のキーバインド）。つまり新しい話者のindexが9以内に収まっていれば、**Cmd+Nでの再割り当ては今のままで既に動く**——変更不要。行ピッカー（`speaker_option_row`、バッジクリック）は`distinct_speakers_for_list`（`distinct_speakers`のクローン）を使っているので、1.の集合を広げれば自動的にここにも反映される。
3. **`speakers`テーブルに`project_id`列が無い**（`crates/asr-core/src/store/mod.rs:52-56`、`id, name, is_enrolled`のみ）——話者とプロジェクトを結ぶ唯一の経路は`segments.speaker_id → media → project`。`load_project`（`crates/asr-core/src/store/projects.rs:161-`）は`segments`とのJOINだけで話者を再構築しており、**セグメントが1件も無い話者はこのクエリに一切現れない**——つまり「追加したがまだどのセグメントも割り当てていない話者」でアプリを再起動/プロジェクトを開き直すと、その話者はUIからもDBからも見えなくなる（DBには孤立した`speakers`行として残り続けるが、それ自体は実害の無いゴミ行）。
   - これを完全に解決するには`speakers`にプロジェクトへの紐付け列を追加するマイグレーションが要る（`migrate`は現状`CREATE TABLE IF NOT EXISTS`のみで、バージョン管理された`ALTER TABLE`の仕組みが無い）——今回はこの機能の要求に対して過剰な変更だと判断し、**あえてこの制約を受け入れる**。実際の想定ユースケース（追加してすぐ既存セグメントを付け替える）ではほぼ問題にならない。
4. **話者の追加（新規index採番）自体は今アプリに一つも実装されていない操作**——既存の`push_undo_snapshot`/`confirm_merge`（統合＝削除）はあっても、その逆（新規追加）は無い。`speaker_db_ids`の運用は「採番・削除は即時（同期）、名前とセグメントの中身は2秒おきの自動保存タイマーで遅延同期」という一貫した方針（`confirm_merge`の`delete_speaker`呼び出しが同期的な例）——新規追加もこれに倣い、DB行の作成は即時に行う。
5. **既存の`UndoSnapshot`の`speaker_db_ids`巻き戻しには、統合をUndoしても削除済みDB行を復活させないという既知のギャップがある**（`restore_snapshot`は`speaker_db_ids`をそのまま上書きするだけで、消えた`speakers`行を再INSERTしない）。今回の「話者追加」もUndo時にDB行を消さない（同じ性質の孤立行が残るだけ、実害は無い）——これは既存のギャップと同じ性質のトレードオフとして許容する。

**設計**:

1. **`crates/asr-core/src/store/projects.rs`に`create_speaker`を新規追加**（既存の`create_speakers`——複数話者を一括採番、プロジェクト作成時専用——とは別に、1件だけ採番する版。`crates/asr-core/src/terms/store.rs`の`add_term`と同じ「1行INSERTしてidを返す」流儀）:
   ```rust
   /// Mints a single new `speakers` row, returning its id. Unlike
   /// `create_speakers` (a batch mint for project creation), this is for
   /// adding one speaker to an already-open project.
   pub fn create_speaker(conn: &Connection, name: &str) -> Result<String> {
       let id = uuid::Uuid::new_v4().to_string();
       conn.execute("INSERT INTO speakers (id, name) VALUES (?1, ?2)", params![id, name])?;
       Ok(id)
   }
   ```
   （`create_speakers`をこの関数を呼ぶループに書き換えてもよいが、必須ではないので触らない）

2. **`EditorScreen`に`extra_speakers: BTreeSet<i32>`フィールドを追加**（`hidden_speakers`の隣、コンストラクタで`BTreeSet::new()`初期化）——「セグメントには未登場だが、手動で追加された話者」のindexを保持する。`distinct_speakers`の計算（`:1009`）を次のように拡張する:
   ```rust
   let mut set: BTreeSet<i32> = self.segments.iter().map(|s| s.speaker).collect();
   set.extend(&self.extra_speakers);
   ```
   これ1箇所の変更で、遅延生成ループ・パネル表示・行ピッカー・統合ピッカーの全てに自動的に反映される（上記「現状の制約」1.参照）。

3. **`UndoSnapshot`に`extra_speakers: BTreeSet<i32>`を追加**し、`capture_snapshot`/`restore_snapshot`でも同様に読み書きする。理由: 話者追加をUndoした時、その話者を`distinct_speakers`の集合から消すには`extra_speakers`側を元に戻す必要があるため（セグメント側には最初から何の変化も無いので、`segments`だけの巻き戻しでは消えない）。
   - `restore_snapshot`の末尾に、`self.speaker_name_states`を「セグメント由来の話者 ∪ 復元後の`extra_speakers`」に含まれないキーを`retain`で除去する処理を追加する（`confirm_merge`が統合済みの話者のエンティティを明示的に`remove`しているのと同じ意図——話者追加をUndoした時に無関係になったTextareaStateエンティティがハッシュマップに残り続けてリークするのを防ぐ）。

4. **`add_speaker`メソッドを新規追加**（`confirm_merge`の隣、実装済み）——`window`引数は不要だった（名前を敢えて設定しないため）:
   ```rust
   fn add_speaker(&mut self, cx: &mut Context<Self>) {
       self.push_undo_snapshot(cx);
       let next_index = self
           .segments.iter().map(|s| s.speaker)
           .chain(self.extra_speakers.iter().copied())
           .max().map(|m| m + 1).unwrap_or(0);
       // dev-shortcut launch path (`project_id: None`) never touches the
       // DB at all -- matches the existing rule ("開発用ショートカットは
       // DBを一切通さない").
       if self.project_id.is_some() {
           match asr_core::store::projects::create_speaker(&self.conn, &format!("S{next_index}")) {
               Ok(speaker_id) => { self.speaker_db_ids.insert(next_index, speaker_id); }
               Err(err) => {
                   self.save_error = Some(format!("話者の追加に失敗しました: {err:#}"));
                   self.undo_stack.pop(); // nothing actually changed; don't leave a no-op undo step
                   return;
               }
           }
       }
       self.extra_speakers.insert(next_index);
       self.dirty = true;
       cx.notify();
   }
   ```
   名前は`initial_speaker_names`を敢えて設定せず、既存の遅延生成ループのフォールバック（`format!("S{speaker}")`）にそのまま任せる——他の話者と全く同じ命名規則になる。DB行の作成に失敗した場合は`extra_speakers`に入れず早期returnする（中途半端な「DBには無いのにUIには出る」状態を作らない）。

5. **UI: 話者パネルのフッターに「+ 話者を追加」ボタンを追加**（`for &speaker in &distinct_speakers`ループの直後）。統合確定ボタン（`merge-confirm`）と同じ「主要操作」スタイル（青背景・濃色文字）を流用済み。

**変更したファイル**: `crates/asr-core/src/store/projects.rs`（`create_speaker`新規追加）、`apps/desktop/src/bin/app.rs`（`extra_speakers`フィールド、`UndoSnapshot`拡張＋`restore_snapshot`の`speaker_name_states`クリーンアップ、`add_speaker`メソッド、`distinct_speakers`計算の拡張、パネルフッターのボタン）。

`cargo build --workspace` / `cargo clippy --workspace --all-targets`（asr-core側）、`cd apps/desktop && cargo +nightly build --bin app` / `clippy --all-targets`とも警告・エラーゼロ。起動スモークテスト（`pipeline_transcript.json`+`clip_230s.mp4`、開発用ショートカット経由、クラッシュ無し）確認済み。

**確認済み**（実機での操作をユーザーが確認、下記1〜7全て問題なし）:

1. 「+ 話者を追加」ボタンで、セグメント0件の新しい話者が話者パネルに現れるか（名前は`S{番号}`）
2. その話者の行ピッカー（バッジクリック）やCmd+1〜9からの選択肢に、追加した話者が出るか
3. 既存の行を新しい話者に再割り当てできるか（元の話者から実際に切り離されるか）
4. `Cmd+Z`で「話者追加」をUndoすると、その話者がパネルから消えるか（まだセグメントを何も割り当てていない状態でUndoした場合）
5. 話者を追加→セグメントを1件割り当て→保存（自動保存を待つ）→アプリを再起動してプロジェクトを開き直す→その話者と再割り当てが正しく残っているか
6. （既知の制約の確認）話者を追加した直後、**何もセグメントを割り当てないまま**プロジェクト一覧に戻って開き直すと、その話者が消えることを確認（想定通りの制約であることの確認、バグではない）
7. 開発用ショートカット（`[json_path] [media_path]`）起動時にボタンを押しても、DBを一切汚染しない（`project_id`が`None`のため`create_speaker`を呼ばない）ことを確認

---

### 編集中の視認性向上とフォーカス不整合バグの修正（実装済み）

前回のEscape/Undo/Redo対応後、実機で2件のフィードバックが届いた: (1) 行のTextareaの枠線を消していた（`appearance(false)`）ため編集中の行が分かりにくい (2) **実際のバグ**——話者一覧の下の空欄をクリックすると「Textareaの実フォーカスは外れているが、編集モード（`focused_row`/`editor`）から抜けていない」状態になり、`Cmd+Z`が本来の編集より一段余分に戻る。

**根本原因（ソースで確認）**: 前セッションでキーマップ修正のためroot divに付けた`.track_focus(&self.focus_handle)`自体が、GPUI標準の「クリックされたら自動でそこにフォーカス移動する」機能を持つ（`gpui-pre-0.3.4/src/elements/div.rs:2717-2730`、マウス**ダウン**のBubbleフェーズで`hitbox.is_hovered(window) && !window.default_prevented()`なら無条件に`window.focus()`）。話者パネル下の空欄には`on_click`が無いため、クリックすると実フォーカスだけがroot側へ移り、`self.focused_row`/`self.editor`は`commit_focused`の明示呼び出し箇所（`focus_row`/`split_focused_segment`/`merge_with_previous`/`leave_edit`）でしかクリアされないため`Some`のまま残る。各行の`.on_click`の`cx.stop_propagation()`はこれを防げない——`track_focus`の自動フォーカス移動はマウスダウン、`on_click`はマウスアップの別イベントで、`stop_propagation`は同フェーズの伝播しか止めないため。結果、直前の編集は一度も`commit_focused`されずUndoスタックにも積まれないまま`Cmd+Z`が呼ばれ、一段余分に戻ったように見える。

**対応**: `EditorScreen`に`editor_blur_subscription: Option<Subscription>`を追加。`focus_row`で新しいTextareaを作った直後、GPUIの`cx.on_blur(&handle, window, listener)`（`gpui-pre-0.3.4/src/app/context.rs:596-613`、原因を問わず実フォーカス喪失を検知する汎用機構）でそのTextarea自身のフォーカスハンドルを購読し、コールバック内で`self.editor`のエンティティIDが一致する時だけ`commit_focused`を呼ぶ。`Subscription`は`Drop`で自動購読解除されるため、次の`focus_row`が新しい`Subscription`を代入するだけで前の行の購読は即座に消え（二重の安全策）、行切り替え時の遅延ブラー通知による誤コミットも起きない。

編集中の行には、高さ・幅に影響しないリング（CSSの`outline`相当）を追加——`.border_1()`はTaffyのレイアウトに参加してしまうため使えず、代わりに純粋な描画エフェクトである**inset** box-shadow（`BoxShadow::new(px(0.),px(0.),color).spread_radius(px(1.)).inset()`）を使用。inset指定により要素の内側に描かれるため、`gpui::list`の仮想化された行スロット境界を越えてにじんだりクリップされたりしない（非insetだと外側ににじむため、この用途には不向きと判明）。

`cargo +nightly build --bin app` / `clippy --all-targets`とも警告・エラーゼロ。起動スモークテスト（クラッシュ無し）確認済み。

**フォーカス不整合バグ（`editor_blur_subscription`）は実機で確認済み** — 問題なし。

**実機のスクリーンショットで2件の見た目の不具合報告があり、1件は修正済み・1件は`gpui-base`側のバグと判明（こちらでは修正不可）**:
1. **リング下部が見えない** → 原因確定・修正済み・実機確認済み。行には常時`border_b_1`（行間の区切り線、`border_color(rgb(0x2a2a2a))`）が付いており、`Style::paint`の描画順は背景→insetシャドウ→子要素→**border**（`gpui-pre-0.3.4/src/style.rs:711-757`で確認）——borderが最後に描かれるため、区切り線がリングの下辺をちょうど同じ1px帯で覆い隠していた。修正: 編集中は同じ`border_b_1`（幅はそのまま1pxで不変、色だけ変更）をリングと同じ色（`0x5b9cf6`）に差し替え、区切り線をリングの一部として取り込む形にした。
2. **編集中のテキストエリアで、折り返された行の末尾の文字が欠ける（`gpui-base`側のバグ・根本原因は未特定・応急処置のみ導入済み）**

   経緯: 最初「右にわずかにはみ出る」という報告だった。`.min_w(px(0.0))`追加では直らず、`.overflow_hidden()`で試しにクリップしたところ、実際の本文の文字（「か」）が欠けて見えなくなることが確認された——**クリップは中身を握りつぶすだけの誤った対応**だったため撤回。

   その後、`~/.cargo/registry`からの`gpui-base`をローカルにコピーし、`apps/desktop/Cargo.toml`に`[patch.crates-io]`で一時的に差し替えて`TextElement::prepaint`（`gpui-base-0.6.0/src/input/base/element.rs`）に`eprintln!`診断を仕込み、実機で実測（診断・パッチとも作業完了後に削除済み、現在のツリーはクリーン）:
   - `wrap_width = bounds.size.width - line_number_width - RIGHT_MARGIN`（`RIGHT_MARGIN = px(10.)`固定）で、問題の行では`bounds.size.width = 1130px` → `wrap_width = Some(1120px)`。数値上は10pxの余裕があり、しかも静的テキスト側の折り返し幅（無条件で1130px使い切り）より**保守的**（狭い）——理屈の上では編集中の方が安全に折り返されるはずだった。
   - 検証のため、Textareaを包む自前のラッパーdivに`.pr(16px)`を追加して`wrap_width`を1065pxまで実際に縮めたが、**欠け方は寸分違わず同じ**だった。
   - `.pr(200px)`まで大きく増やすと、今度は折り返し位置自体は早まった（1行に収まる文字数が減った）が、**新しい折り返し位置でもやはりその行の最後の文字が欠けた**（今回は行の途中、「きちっとや·」のあたり）。
   - 一方、**折り返しではなく文章が自然に終わる最後の行**（カーソルが末尾にある行）は欠けずに正しく表示される。

   欠ける位置は行の右端の固定ピクセル位置ではなく、**その時々の折り返し境界そのもの**に追従する（幅を変えても「効かない」のではなく、境界に来た文字が毎回欠ける）ことがここまでで分かっていたが、原因は「`prepaint`か`paint`か」まで絞り込めていなかった。

   **根本原因を数値で確定（2回目の`[patch.crates-io]`診断）**: `layout_lines`（`element.rs`、各wrapされた行を`shape_line`で実際に整形する箇所）に、整形済み行の実測幅（`ShapedLine::width()`）と、その行に指定された`wrap_width`を比較するログを追加。実機で実測した結果:
   ```
   range=0..241  shaped_width=1052.4px  wrap_width=1018px  → 34.4px オーバー
   range=0..250  shaped_width=1091.2px  wrap_width=1056px  → 35.2px オーバー
   ```
   **`wrap_width`が状況によって異なっていても（1018px/1056px）、実際に整形された行の幅がその`wrap_width`を約34〜35px（全角文字2文字分程度）超えて確定している**——`RIGHT_MARGIN`（10px）が足りないというレベルではなく、**折り返し判定（`wrap_line`、`gpui-pre`側のテキスト整形エンジン）が指定された幅を無視して2文字分ほど余分に詰め込んでいる**、という実測で裏付けられたバグ。このオーバー分がそのまま右にはみ出し、描画/クリップ境界で欠けて見えている。バージョン固定されたベンダー依存クレート（`gpui-pre`）の奥深くの挙動であり、`apps/desktop`側からは修正できない。

   **最終確認用に独立した最小リポジトリを作成**（`apps/desktop`とは別、`~/Repository/gpui-wrap-bug-repro/`——`gpui-kit`のみに依存する完全に独立したCargoプロジェクト。このアプリ固有のコードを一切含まない、素の`Textarea`だけの最小構成）。ここで判明した重要な訂正:
   - **`gpui::list`（仮想化リスト）は無関係**——素のTextarea単体・`gpui::list`の中、どちらでも同じテキストなら同じく再現/非再現した。当初「行が`gpui::list`の中にある」ことが引き金かと疑ったが誤りで、リポジトリからは削除済み。
   - **「読点（、）が原因」という単純な説明も誤り**——フィラー文字（「あ」の繰り返し）に「、」を1個だけ挿入しても、4個散らして挿入しても、**一度も再現しなかった**。一方、自然な文章（元の文字起こし文、および新しく作った「今日は天気が良いので、公園に行って、散歩をして、それから、買い物をしようと思います。」）は`、`がある版は再現し、同じ文章から`、`を全部除去した版・1個だけ残した版はどちらも再現しなかった。つまり**「`、`の有無」も「`、`の個数」も単独では説明できず、特定の自然な文章＋`、`の組み合わせでしか再現しない**、という結論になった。
   - **「横スクロールが起きているのでは」という仮説も実測で否定**——`gpui-base`に`cursor_scroll_offset`/`scroll_size`を出力する診断を追加し、再現中の瞬間を確認したが`cursor_scroll_offset.x`は常に`0px`のままだった。同じ瞬間に「行ごとの実測幅 vs wrap_width」診断も同時に確認し、`shaped_width`が`wrap_width`を確実に超えていること（例: `+27.37px`）を直接確認——**スクロールは無関係で、折り返し判定自体が予算を超えて詰め込んでいる、という最初の結論が独立した2系統の診断で二重に裏付けられた**。
   - 文字種混在（ラテン文字の有無）も無関係と確認済み（前回のセッションで確認）。
   - **結論**: 根本原因は特定できていない（`、`単体でも`gpui::list`でも横スクロールでもない）が、「特定の自然な文章内容＋読点の組み合わせでのみ、折り返し判定が自分のwrap_widthを超えて整形する」という現象自体は独立した最小リポジトリで確実に再現し、実測でも裏付けが取れている。この最小リポジトリはトグルUIとテキストのバリエーション一覧（`TEXT_OPTIONS`テーブル、追加が容易な作り）を備えており、`longbridge/gpui-kit`へのIssue起票時にそのまま添付できる状態。

   **`apps/desktop`側での緩和策は試したが撤回**: `WRAP_MITIGATION_MARGIN`（`px(8.0)`、全角文字の半分程度）を編集中のTextareaの左右に`.px(...)`で追加し、はみ出た分をこの余白内に収める狙いだったが、**実機で試したところ悪化するケースがあった**ため撤回済み（コードは追加padding無しの元の状態に戻してある）。理由の推測: この余白はTextareaの`w_full()`が使える幅を狭めるため、折り返し境界の位置自体がズレる——ズレた先の新しい境界の文字が、たまたま前より大きくオーバーする可能性があり、「幅を変えると新しい境界でまた別の文字が同様に欠ける」という既知の挙動（前回のセッションで`WRAP_SAFETY_MARGIN`実験でも確認済み）と整合する。**確実に改善する保証のある幅調整は無い**、という結論。

   なお、`Textarea`自身が内部で持つ固定padding（`Input::render()`が毎フレーム`Size::Medium`基準で`set_editor_paddings`を呼び直す、10px相当）は`Textarea`の公開ビルダーAPIから変更する手段が無く、外側からは触れない（`Size`を上書きする`.size(...)`のようなメソッドが`Textarea`に存在しないため）。

   **次に着手するなら**: (a) 独立最小リポジトリの実測データ・スクリーンショットを添えて`longbridge/gpui-kit`にIssueを起票する（現状は完全一致するIssueが見つかっていない）、(b) `gpui-pre`（`zed-industries/zed`由来）側の`wrap_line`実装まで踏み込んで原因を特定する、(c) 新しいバージョンで直っているか確認する（このプロジェクトはGPUIを厳密固定しているため、上げる場合は他のAPI変更の影響も要確認）、(d) 最終手段として、このアプリ側でテキストを事前に手動で折り返し、`soft_wrap`を使わない設計に切り替える（大きな変更になるため他の優先度と相談）。

再ビルド・clippyとも警告・エラーゼロ、起動スモークテスト（クラッシュ無し）確認済み。

**現状**: (1)のリング下部は解決・確認済み。(2)は`gpui-base` 0.6.0自体の描画バグと判明——`apps/desktop`側での対応（`.overflow_hidden()`によるクリップ、幅の調整）はどちらも試して撤回済みで、現在のコードは素の状態（`Textarea::new(&state).w_full().min_w(px(0.0))...`のみ）に戻してある。折り返しが発生する行では、境界の文字が欠けて読めなくなる実害が残ったまま——恒久的な回避策なし（上の詳細参照）。

### Undo/Redoを実際に到達可能にする（実装済み）

**Context**: 「Undo/Redoで文字編集内容も戻せるのか？」という質問を調査した結果、**戻せる設計にはなっているが、実際には発火させる手段が無かった**ことが判明した。`commit_focused`は行の編集内容をUndo履歴に積むが、`Cmd+Z`は`Some("!Input")`スコープのためTextareaにフォーカスがある間は発火せず、しかも**編集モードを抜ける手段が存在しなかった**（`focused_row`/`editor`はクリックで次のTextareaが作られるだけで、GPUIには「他所クリック＝blur」という自動挙動が無い）。結果、一度どれかの行をクリックすると以後`Cmd+Z`は永久にその行の文字単位Undoにしか届かなかった。

対応（`apps/desktop/src/bin/app.rs`のみ、`crates/asr-core/src/pipeline/mod.rs`に1行）:

1. **`Escape`で編集モードを抜けられるように** — `leave_edit`メソッドを追加。ピッカーが開いていればそれを閉じ、無ければ`commit_focused`（変更があればUndoスナップショットも積まれる）してから`window.focus(&self.focus_handle, cx)`でフォーカスを自前ハンドルに戻す。配線は2経路: (a) `gpui-base`自身の`Escape`ハンドラ（`Some("Input")`スコープ）が最後に`cx.propagate()`することを利用し、root divに`.on_action::<Escape>()`を足すだけで新しいキーバインド無しに拾う（フォーカスがある時）。(b) 自前アクション`LeaveEdit`を`KeyBinding::new("escape", LeaveEdit, Some("!Input"))`で追加（フォーカスが無くピッカーだけ開いている時）。
2. **Undo/Redoで戻った行をハイライト＋自動スクロール** — `TranscriptSegment`に`PartialEq`を追加し、`first_difference`で復元前後の`segments`を突き合わせて最初に食い違う行を検出。`highlighted_row: Option<usize>`フィールドに保持し、`restore_snapshot`で`list_state.scroll_to_reveal_item`しつつセット、`focus_row`（ユーザーが自分で別の行を選んだ時）と`push_undo_snapshot`（新しい変更をした時）でクリア。描画側は再生中の淡いアクセント（`rgba(0x5b9cf62e)`）とは別の暖色の背景ウォッシュ（`rgba(0xf5a62333)`）を当てる。

`cargo build --workspace` / `clippy --workspace --all-targets`、`cd apps/desktop && cargo +nightly build --bin app` / `clippy --all-targets`とも警告・エラーゼロ。起動スモークテスト（`pipeline_transcript.json`+`clip_230s.mp4`、クラッシュ無し）確認済み。

**確認済み**: (1) 行をクリックして本文を編集→`Escape`→`Cmd+Z`でその編集が元に戻るか（**今回の主目的**） (2) 行Aを編集→行Bをクリック→`Escape`→`Cmd+Z`で行Aの編集が戻り、行Aまでスクロールしてハイライトされるか (3) 編集中の`Cmd+Z`が従来どおり文字単位Undoのままか (4) 日本語入力の変換中に`Escape`を押した時、変換キャンセルだけで済むか (5) 削除・分割・結合・話者割当・話者統合のUndo/Redoでも該当行がハイライトされるか (6) 話者名欄（左ペイン）にフォーカスした状態から`Escape`で抜けて`Cmd+Z`が効くか (7) ピッカーが開いている状態で`Escape`を押すとピッカーだけが閉じるか——全てユーザーが実機で確認済み。

### エディタ画面への5件の追加要望（実装済み）

前回のフィードバック対応（下記6件）の後、実機で使ってみての追加要望5件。全て`apps/desktop/src/bin/app.rs`のみの変更で対応済み:

1. **左ペインを320px固定** — `.w(relative(0.42))`→`.w(px(320.0))`。
2. **話者の表示/非表示トグル** — `hidden_speakers: BTreeSet<i32>`＋`toggle_speaker_visibility`。話者パネルに「表示」⇔「非表示」リンクを追加。タイムラインは`speaker_timeline(...)`に渡す`segments`をフィルタ、セグメントリストは行構築クロージャの先頭で非表示話者なら空`div()`を返して高さ0で除外（`ix`ベースの`focused_row`等は変更不要）。非破壊操作なのでUndo対象外。
3. **セグメント行の削除（即時・確認無し）** — `delete_segment(ix, cx)`。行ヘッダーに「削除」リンクを追加（`.justify_between()`で右寄せ、`cx.stop_propagation()`で行の`focus_row`と分離）。
4. **Undo/Redo（`Cmd+Z`/`Cmd+Shift+Z`）** — スナップショット方式（`UndoSnapshot { segments, speaker_names, speaker_db_ids }`、`undo_stack`/`redo_stack`、上限100件）。`commit_focused`（実変更時のみ）・`split_focused_segment`・`merge_with_previous`・`assign_speaker`・`choose_row_speaker`・`confirm_merge`・`delete_segment`の7箇所で`push_undo_snapshot`。キーバインドは`Some("!Input")`コンテキストでバインド——Textareaにフォーカスがある間は"Input"コンテキストが有効なので発火せず、Textarea自身の内部Undo/Redo（文字単位）が優先される。フォーカスが無い間だけセグメント単位のUndo/Redoが発火する設計（ユーザー要望どおり）。
5. **セグメントリストにもスクロールバー** — タイムラインで撤退した`gpui_component::scroll::Scrollbar`を再導入。今回は`gpui-component`自身の`list.rs`と同じ配置パターン（`.relative()`のラッパーに`list`と`Scrollbar::vertical(&self.list_state)`を直接の兄弟として置く、専用行に分離しない）を踏襲——タイムラインで踏んだ「専用行に分離すると見えなくなる」罠を回避。セグメントリストには「背景クリックでシーク」のような競合するジェスチャーが無いため、タイムラインほどのリスクは無いと判断。

`cargo +nightly build --bin app` / `clippy --bin app --all-targets`とも警告・エラーゼロ。起動スモークテスト（`pipeline_transcript.json`+`clip_230s.mp4`、クラッシュ無し）確認済み。

**確認済み**: (1) 左ペインが320pxで固定表示されるか (2) 話者トグルでタイムラインのバーとセグメントリストの両方が連動して表示/非表示になるか (3) 削除ボタンで即座に行が消えるか (4) `Cmd+Z`/`Cmd+Shift+Z`で削除・分割・結合・話者割当・話者統合が正しく戻る/やり直せるか（Textareaにフォーカス中は文字単位のUndoとして機能しセグメント単位のUndoが誤発火しない） (5) セグメントリストのスクロールバーが表示されドラッグでスクロールできるか、かつ行・バッジ・タイムスタンプのクリックが従来通り機能するか——全てユーザーが実機で確認済み。

### エディタ画面の実機フィードバック6件（実装済み）

(1c)/(1d)/(1e)を一通り実装した後にユーザーから届いた6件のフィードバック。全て対応済み:

1. **タイムラインの中央追従が再生中に効かない** → **原因確定・修正済み・実機ログで動作確認済み**。`#timeline-scroll`自体がflexコンテナではなく、その直下の「左右パディング＋本編」の行が明示的な幅を持たないため、GPUIが行の幅を「子要素の合計」ではなく「ビューポート幅にストレッチ」して測定し、スクロール可能な超過幅をゼロと誤認していた（`max_offset.x == 0`→どんなオフセットを設定してもGPUI自身のクランプで即座に0に戻る）。行に明示的な合計幅（`half_viewport*2 + content_width`）を与えて解決。実機ログで`max_offset`が`0px`→`8340px`等の正しい値になり、実際の再生中に毎フレームのオフセットが連続的に変化する（0にクランプされ戻らない）ことを確認済み。
2. **一時停止中にドラッグ/スクロールバーでスクロールしたい** → `gpui_component::scroll::Scrollbar`を試したが、その内部実装（`gpui-base-0.6.0/src/scrollbar.rs`）はマウスダウンされた領域全体（100%/100%に自己サイズする）を「クリックでジャンプ」として扱い`cx.stop_propagation()`するため、シーククリックと共存できず、さらに専用の行に分離しようとすると今度は完全に非表示になる（コンポーネント自身のサイズ解決の癖に起因、深追いせず撤退）。**最終的に自前のドラッグ実装に切替**——`#timeline-scroll`に`on_mouse_down`/`on_mouse_move`/`on_mouse_up`を実装し、移動量が4px未満なら「クリック＝シーク」、それ以上なら「ドラッグ＝パン」として扱う（`begin_timeline_drag`/`continue_timeline_drag`/`end_timeline_drag`、`EditorScreen::timeline_drag`フィールド）。実機ログで、一時停止中に大きくオフセットが変化した後にシークが行われる、という実際のドラッグ操作に整合するパターンを確認済み。
3. **キーマップがTextareaフォーカス時しか反応しない** → **原因確定・修正済み**。GPUIの`Window::dispatch_action`はフォーカスされた要素からバブリング（上方向のみ）でディスパッチするため、`FocusHandle`を持つ要素がTextarea以外に一つも無いこのアプリでは、何もフォーカスされていない時にキーマップ用`.on_action`群へ絶対に届かなかった。`EditorScreen`に`focus_handle: FocusHandle`を追加し、外側のdivに`.track_focus(&self.focus_handle)`。`render()`の先頭で「何もフォーカスされていなければ自分のハンドルにフォーカスする」（`window.focused(cx).is_none()`のガード付きなので、Textareaフォーカス中は奪わない）。
4. **プロジェクト一覧に戻るボタンが無い（エディタ画面）** → `AppRoot::render`の`AppScreen::Editor`分岐に、`EditorScreen`自体には手を入れずヘッダーを追加（`flex_col`の兄弟として配置、`editor.clone()`は`flex_1().min_h(px(0.0))`でラップするだけで正しく縮む）。離脱前に`editor.update(cx, |view, cx| view.persist(cx))`で強制保存してから(1e)の`back_to_projects`を呼ぶ。
5. **再生速度をボタンでも変更したい** → プレイヤー欄のレート表示の両脇に「−」「＋」ボタンを追加、既存の`rate_down`/`rate_up`をそのまま呼ぶ（新規ロジック無し）。
6. **-30秒/-5秒/+5秒/+30秒のシークボタン** → `seek_back`/`seek_forward`を`ms: u64`引数付きに一般化（既存のキーマップは`Self::SEEK_STEP_MS`=5秒を渡すよう据え置き）、プレイヤー欄に4ボタン追加。

**ついでに対応**: セグメント行のタイムスタンプ表示をクリックするとその時刻へシークするようにした（`cx.stop_propagation()`で行自体の`focus_row`とは独立）。

**変更したファイル**: `apps/desktop/src/bin/app.rs`のみ。ビルド・clippyとも警告ゼロ、診断用の一時的な`eprintln!`は全て削除済み。

**確認済み**: (1)(2)は実機ログでの裏付けに加えユーザー自身の目視確認も完了。(3)〜(6)とタイムスタンプクリックも実機で確認済み。

### (1e) 設定画面をUpload/Projects共通のトップレベル画面に昇格

**Context**: 「設定 (APIキー)」ボタンが`UploadScreen`にしか無く（`UploadMode::Settings`という`Upload`専用のサブモード）、プロジェクト一覧からは「新規作成」を経由しないと辿り着けなかった。ユーザーから「設定は新規にあるのは良くない」と指摘があり修正。

**設計**: `Settings`を`Upload`専用のサブモードから`AppScreen`のトップレベルの一variantに昇格——`Projects`・`Upload`どちらからも開けて元の画面に戻れるようにした:
```rust
enum AppScreen {
    Projects(ProjectsScreen),
    Upload(UploadScreen), // mode: UploadMode フィールドは削除
    Editor(Entity<EditorScreen>),
    Settings { api_key_input: Entity<InputState>, error: Option<String>, return_to: Box<AppScreen> },
}
```
`return_to`に開いた時点の画面（`projects`/`error`や`media_path`/`status`を保持したまま）を丸ごと退避し、「戻る」「保存」で復元（`std::mem::replace`で取り出し／書き戻し）。保存失敗時は`Settings`の`error`フィールドにその場で表示するよう修正——旧実装は`upload.status`にエラーを書いていたが、Settingsサブビューの早期returnに阻まれて**一切表示されない潜在バグ**だったため、ついでに直した。当初`ProjectsScreen`のヘッダーにも同じ「設定 (APIキー)」リンクを追加したが、その後ユーザーから「新規画面から設定ボタン削除してください」との指示があり、**`Upload`側のリンクは削除**——設定へのアクセスは`Projects`画面のみ（`Settings`自体は引き続き`return_to`で任意の画面から開けるトップレベルvariantのまま、Upload側の導線を無くしただけ）。

**変更したファイル**: `apps/desktop/src/bin/app.rs`のみ。ビルド・clippyとも警告ゼロ、起動スモークテスト（クラッシュ無し）確認済み。

**確認済み**: (1) プロジェクト一覧から直接「設定」を開き、保存→一覧に戻れるか（一覧の内容が保持されているか） (2) アップロード画面からも従来通り開けるか（`media_path`/進行状況が保持されているか） (3) 保存に失敗した場合その場にエラーが表示されるか——全てユーザーが実機で確認済み。

---

### (1c) UI調整: プレイヘッドの中央追従＋話者バッジの色分け

**Context**: (1)/(1b)が実機で通ったあと、ユーザーから実際に使ってみての調整要望が2点。(a)「タイムラインの現在バーを常に中央に表示してほしい」——現状は`tools/diarization-viewer.html`から移植した端10%マージンのエッジトリガ追従なので、再生中プレイヘッドが左右に行ったり来たりして視線が安定しない。(b)「文字起こしの各行のスピーカー名に各スピーカーの色をつけて区別しやすくしてほしい。ポップオーバーの選択肢の様な表示がいいのかも」——現状のバッジは話者に関係なく一律`rgb(0x88aaff)`のプレーンテキストで、タイムラインや話者パネルの色分けと繋がっていない。

**ユーザーへの確認結果**: (1) 追従は**プレイヘッドが動いた時だけ**（一時停止中は手動横スクロールを許す。セグメントリスト側で踏んだ「自動追従が手動スクロールを引き戻す」バグと同じ轍を踏まないため） (2) 先頭・末尾でも**空白を足して完全に常に中央**（クランプではなく） (3) バッジは**枠線ピル**（`speaker_option_row`のピルと同じ見た目、枠と文字が話者色）。

---

**(a) プレイヘッドの中央追従**

現行の`follow_timeline_playhead`（`app.rs:463-481`、10%マージンのエッジトリガ）を中央追従に置き換える。

1. **タイムラインのスクロール内容に左右の余白を追加**（`app.rs`の`#timeline-scroll`の子、:1135-1151）。現状は`w(timeline_content_width(...))`の縦積み1枚だけが子になっているので、これを横並びの3要素`[余白][既存の縦積み][余白]`に変える。余白幅は`viewport_width / 2`（`self.timeline_scroll.bounds().size.width`から取る——前フレームの値だが、後述の通りオフセット計算と同じ値を使うので自己整合する）。

2. **オフセットは単純な式になる**。余白が`viewport/2`ちょうどなので、プレイヘッドをビューポート中央に置くオフセットは
   ```
   offset.x = viewport.width/2 - (pad + playhead_px) = -playhead_px
   ```
   に簡約される（`pad == viewport.width/2`）。さらにコンテンツ全幅が`pad + real + pad = real + viewport`になるため`max_offset.x == real`となり、`-playhead_px`は`t=0`で`0`、末尾で`-real`と、**GPUIのクランプ範囲`[-max_offset.x, 0]`にちょうど収まる**（クランプが一度も発動しない＝先頭・末尾でも例外なく中央になる）。GPUIは`div.rs:2429`のprepaintでオフセットを自動クランプするので、こちら側で手動クランプを書く必要は無い。
   ```rust
   /// Returns whether it actually applied (false before first layout).
   fn center_timeline_playhead(&self, playhead_ms: u64) -> bool {
       let viewport = self.timeline_scroll.bounds();
       if viewport.size.width <= px(0.0) { return false; } // no layout yet
       let playhead_px = px(playhead_ms as f32 / 1000.0 * self.pixels_per_second);
       let offset = self.timeline_scroll.offset();
       self.timeline_scroll.set_offset(point(-playhead_px, offset.y));
       true
   }
   ```

3. **呼ぶタイミング**——`last_highlighted_ix`と同じメモ化パターンで「プレイヘッドが動いた時だけ」にする。`EditorScreen`に`last_centered_playhead_ms: Option<u64>`を追加し、`render()`の現行`if self.transport.is_playing() { self.follow_timeline_playhead(...) }`（:560-562）を置き換える:
   ```rust
   if self.last_centered_playhead_ms != Some(playhead_ms)
       && self.center_timeline_playhead(playhead_ms)
   {
       self.last_centered_playhead_ms = Some(playhead_ms);
   }
   ```
   `center_...`が`false`を返した時はメモを更新しない（初回レイアウト前にビューポート幅0で「中央化済み」と記録してしまい、以後プレイヘッドが動くまで永久に中央化されなくなるのを防ぐ）。一時停止中は`position_ms()`が変化しない＝再中央化が走らないので、手動横スクロールが引き戻されない。
   > **ズーム時の追加考慮**: `pixels_per_second`が変わるとプレイヘッドのピクセル位置が変わるのに`playhead_ms`は変わらないため、このメモ化だけだとズーム後に中央からズレたままになる。`zoom_in`/`zoom_out`（:433-441）で`self.last_centered_playhead_ms = None`にして次のrenderで強制的に再中央化する。

4. **クリックシークは変更不要**。`seek_to_timeline_x`（:448-454）はcanvasの実描画座標`timeline_bounds.origin.x`を基準にしており、先頭余白のぶんも自動的に織り込まれる。左余白のクリックは`.max(px(0.0))`で0に、右余白のクリックは`Transport::seek`側が`total_frames`でクランプする（`crates/asr-audio/src/transport.rs:164`）ので、どちらも安全。

**(b) 話者バッジの色分け（枠線ピル）**

`speaker_option_row`のピル（:174-191: `px_1().py(px(2.0)).rounded_md().border_1().border_color(color).text_size(px(11.0))`）と同じ見た目を、セグメント行のバッジ（:618-643）にも適用する。両者が確実に同じ見た目であり続けるよう、スタイルだけを当てる小さなジェネリックヘルパーを1つ切り出して両方から使う（`Div`と`Stateful<Div>`のどちらにも当てられるよう`E: Styled`で受ける——バッジは`.id()`済みで`Stateful<Div>`になっているため）:
```rust
fn style_speaker_pill<E: Styled>(el: E, color: Hsla) -> E {
    el.flex_none().px_1().py(px(2.0)).rounded_md()
        .border_1().border_color(color)
        .text_size(px(11.0)).line_height(Rems(1.0)).text_color(color)
}
```
バッジ側は現行の`.text_color(rgb(0x88aaff))`／`.text_size(px(12.0))`を捨ててこれに差し替え、`speaker_color(seg.speaker)`を渡す。`speaker_option_row`側も同じヘッダー経由にして、文字色が話者色になる（＝ピッカーの選択肢と行のバッジが完全に同一の見た目になる）。これは話者パネルの統合ピッカーにも同時に効くが、タイムライン・話者パネルのスウォッチと同じ`speaker_color`で統一される方向なので望ましい変化。

> **注意（実機で要確認）**: バッジに`py(2px)`＋`border 1px`が付くぶん高さが約6px増え、`badge_bounds`が変わる。行単位ピッカーの位置は原因未特定のまま`.offset(px(-70.0))`という実測補正値に依存している（上の「行単位修正ピッカーのフローティング化」参照）ので、**この変更でピッカーの表示位置が数px下にズレる可能性がある**。実機で確認し、ズレていればこの定数を再調整する。

**変更するファイル**: `apps/desktop/src/bin/app.rs`のみ（余白付きタイムライン構造、`follow_timeline_playhead`→`center_timeline_playhead`、`last_centered_playhead_ms`フィールド、`zoom_in`/`zoom_out`でのメモ無効化、`style_speaker_pill`ヘルパーとバッジ／`speaker_option_row`への適用）。`speaker_timeline.rs`は変更不要（余白はスクロールコンテナ側の兄弟要素として足すため、canvasの座標系は今のまま）。

**検証方法**: `cd apps/desktop && cargo +nightly build --bin app` / `clippy --all-targets` の後、実機（ユーザー操作）で——(1) 再生中、プレイヘッドが常に画面中央に留まるか (2) 先頭・末尾でも中央のままか（左右に空白が出る） (3) 一時停止中にタイムラインを手で横スクロールしても引き戻されないか (4) ズームしても中央が保たれるか (5) タイムラインのクリックシークが従来通り正しい位置に飛ぶか (6) 各行のバッジが話者ごとの色のピルになり、タイムライン／話者パネルの色と一致しているか (7) **バッジをクリックした時のピッカーの表示位置がズレていないか（ズレていれば`-70.0`を再調整）**。

**実装状況**: 実装済み。`style_speaker_pill<E: Styled>`ヘルパーを追加し`speaker_option_row`のピルとセグメント行バッジの両方に適用（バッジは`speaker_color(seg.speaker)`で色分け）。`follow_timeline_playhead`を`center_timeline_playhead`（`offset.x = -playhead_px`、`viewport/2`の左右パディングにより自己クランプ）に置き換え、`last_centered_playhead_ms`によるメモ化で「プレイヘッドが動いた時だけ」再センタリング、`zoom_in`/`zoom_out`でメモをリセットして強制再センタリング。ビルド・clippyとも警告ゼロ、起動スモークテスト（`pipeline_transcript.json`+`clip_230s.mp4`、クラッシュ無し）確認済み。

**確認済み**: 上記「検証方法」の(1)〜(7)全て——(7)のピッカー位置ズレ含め、ユーザーが実機で確認済み。

---

### (1d) プロジェクト履歴（過去の文字起こし結果の一覧・再オープン・編集の自動保存）

**Context**: ユーザーから「履歴機能ってまだ用意てなかったっけ」と質問があり、調査の結果**未実装**と確定した（`app.rs`に`AppScreen::Upload`と`AppScreen::Editor`の2画面しか無く、過去のプロジェクト一覧・再オープン画面は存在しない。さらに調べると、パイプライン完了後の文字起こし結果は**一切ディスクに保存されておらず**、アプリを閉じると完全に消える——実際に課金・時間のかかるASR実行をした直後にウィンドウを閉じたら結果が丸ごと失われる、という見過ごせないギャップだった）。ユーザーへの確認の結果、(1) パイプライン完了後の編集もDBに保存する (2) 起動時にまずプロジェクト一覧を表示する、の両方を実装することで合意。

**スコープ上の判断（ここで決め、実装時に踏襲する）**:
- **DB保存先**: `~/Library/Application Support/speaker-identification/app.db`（Keychainの`KEYCHAIN_SERVICE`と同じアプリ識別子で統一）。`dirs`クレートは追加せず`$HOME`から素朴に組み立てる（このアプリは既にmacOS専用の作りのため）。
- **話者の永続化はプロジェクトローカルに限定**——`speakers`テーブルはスキーマ上「プロジェクト横断」（enrollment用）だが、そのenrollment機構自体がまだGUIに配線されていない（話者パネルのfollow-upとして既に別途スコープ外にしてある）。今回はプロジェクト作成時に、その時点の`distinct_speakers`ごとに新規`speakers`行を1つずつ発行し、以後そのプロジェクトの中でだけ使い回す。プロジェクトをまたいだ「同一人物」の統合は行わない（将来のenrollment配線という別タスク）。
- **`original_text`/`asr_confidence`/`edited_at`列は今回は使わない**——`corrections`テーブルと同様、「編集前後の差分を追跡する」機能は別の未着手タスク（Phase 4「編集履歴からの用語自動学習」）として明確に切り離す。今回は`text`と`original_text`に同じ値を書き、`edited_at`/`asr_confidence`はNULLのままにする。編集の巻き戻し（真のUndo）や「元の文言との差分表示」は提供しない。
- **プロジェクト名は自動**——メディアファイルのファイル名（拡張子抜き）をそのまま使う。リネームUIは無し（未依頼、スコープ外）。
- **開発用ショートカット（CLI引数`[json_path] [media_path]`）はDBを一切通さない**——これはデバッグ用の軽量パスとして意図的に維持し、実プロジェクトDBを汚染しない。この起動経路では自動保存もされない。
- ~~バックナビゲーション無し~~——当初はプロジェクト一覧→アップロード画面を一方向としたが、後日ユーザーから「新規画面に戻るボタンを追加してください」との指示があり追加済み（`back_to_projects`、`AppRoot::open_upload`のすぐ下）。パイプライン実行中は無効化——`start_pipeline`のポーリングループは`self.screen`が`Upload`でなくなった瞬間に`Next::Stop`で抜けるため、実行中に離脱すると結果が保存されずに消える。この無効化ガードは呼び出し側（`render`のクリックハンドラ）だけに置き、`back_to_projects`自体はガードを重複させない。
- **自動保存のトリガーは「dirtyフラグ＋定期タイマー」方式**——個々の変更操作（本文コミット・分割・結合・話者割当×2箇所・話者名変更・話者統合）それぞれに保存呼び出しを埋め込むのではなく、既存の200msポーリングパターン（再生位置更新）と同じ仕組みで2秒間隔の定期タイマーを追加し、`dirty`フラグが立っていれば保存して降ろす、という1本の経路に統一する。各変更操作は`dirty = true`をセットするだけでよく、保存処理自体を各所に重複させない。

**設計**:

1. **`crates/asr-core/src/store/projects.rs`（新規）**——`terms/store.rs`と同じ立て付け（`asr-core`がSQLiteを知る唯一の場所という規律を維持）:
   ```rust
   pub struct ProjectSummary { pub id: String, pub name: String, pub created_at: String, pub media_path: String, pub segment_count: i64 }
   pub fn list_projects(conn: &Connection) -> Result<Vec<ProjectSummary>>;
   // projects + media（1プロジェクト1メディア前提、現行の設計と一致） + segmentsのCOUNTをJOIN、created_at降順

   pub fn create_project(conn: &Connection, name: &str, media_path: &Path) -> Result<(String /*project_id*/, String /*media_id*/)>;
   // projects行 + media行（duration_ms等は今回追跡しないのでNULLのまま）を挿入

   pub fn get_media_path(conn: &Connection, project_id: &str) -> Result<PathBuf>;

   /// (project-local index -> speakers.id) のマップを新規発行する。
   /// プロジェクト作成直後、初回保存の一部として1回だけ呼ぶ。
   pub fn create_speakers(conn: &Connection, names: &HashMap<i32, String>) -> Result<HashMap<i32, String>>;

   /// 話者行の名前をUPDATE、かつ現在のセットに存在しない古いid（話者統合で
   /// 消えた話者）を削除する。呼び出し元が持つ `speaker_db_ids` を単一の
   /// 真実の対応表として使い続けられるよう、削除は行わず返り値も無し
   /// （`speaker_db_ids` からの除去は呼び出し元の責務）。
   pub fn sync_speaker_names(conn: &Connection, speaker_db_ids: &HashMap<i32, String>, names: &HashMap<i32, String>) -> Result<()>;
   pub fn delete_speaker(conn: &Connection, speaker_id: &str) -> Result<()>; // 統合で消えた話者用

   /// media_idの既存segmentsを全削除して`segments`を丸ごと入れ直す
   /// （差分更新ではなく毎回全置換——編集の同一性を追跡する必要が
   /// 無い今回のスコープでは単純さが勝る）。トランザクションで実行。
   pub fn save_segments(conn: &mut Connection, media_id: &str, segments: &[TranscriptSegment], speaker_db_ids: &HashMap<i32, String>) -> Result<()>;

   /// 再オープン用: segmentsをspeakersにJOINして読み込み、出現順に
   /// 0,1,2,...のプロジェクトローカルindexを新規に割り振る。この
   /// indexはDBには存在しない——セッション内だけで完結する一時的な
   /// 割当てで、`TranscriptSegment.speaker`（既存のi32）が要求するもの。
   pub fn load_project(conn: &Connection, project_id: &str) -> Result<(PathBuf /*media_path*/, Vec<TranscriptSegment>, HashMap<i32, String> /*speaker_names*/, HashMap<i32, String> /*speaker_db_ids*/)>;
   ```

2. **`apps/desktop/Cargo.toml`**——`rusqlite = { version = "0.32", features = ["bundled"] }`を追加（`asr-core`と同じ固定バージョン。`bundled`でシステム依存無しに静的リンク）。

3. **`AppScreen`に`Projects`を追加**:
   ```rust
   struct ProjectsScreen { projects: Vec<asr_core::store::projects::ProjectSummary>, error: Option<String> }
   enum AppScreen { Projects(ProjectsScreen), Upload(UploadScreen), Editor(Entity<EditorScreen>) }
   ```
   `AppRoot`に`conn: Connection`を追加（`main()`で`app_db_path()`を`create_dir_all`した上で一度だけ`asr_core::store::open`）。`render()`は現行の「Upload以外はEditorのはず」という2分岐のelse-unreachableパターンをやめ、3方向の明示的な分岐に変更（各画面の描画を`render_projects`/既存のUpload本体/`Editor`委譲に分ける）。

   一覧の各行は既存の話者パネル行と同じ見た目（ボーダー付き横並び、名前・日時・セグメント数）、末尾に「開く」ボタン。上部に常設の「新規作成」ボタン（`AppScreen::Upload(UploadScreen::default())`相当へ遷移）。「開く」は`store::projects::load_project`→`Transport::new(&media_path)`→（開けなければエラー表示、既存の「音声の読み込みに失敗しました」パスと同じ形）→`build_editor_entity`拡張版を呼んで`AppScreen::Editor`へ。

4. **`EditorScreen`に永続化フィールドを追加**: `conn: Connection`（自分専用に同じDBファイルへ再オープン——`store::open`がWALモードにしているため複数コネクションの同時アクセスは安全）、`project_id: Option<String>`、`media_id: Option<String>`、`speaker_db_ids: HashMap<i32, String>`、`dirty: bool`。開発用ショートカット経由の場合は`project_id`/`media_id`を`None`にし、自動保存は常に`project_id.is_some()`をガードして無効化する。

5. **`build_editor_entity`を拡張**——新しい保存先情報（`conn`, 既存プロジェクトなら`project_id`/`media_id`/`speaker_db_ids`、開発用ショートカットなら全部無し）を追加パラメータとして受け取るよう変更。呼び出し元3箇所（開発用ショートカット・アップロード完了後・プロジェクト再オープン後）がそれぞれ適切な値を渡す。

6. **アップロード完了時（`start_pipeline`のポーリングループの`Next::Succeeded`分岐）**——`build_editor_entity`を呼ぶ前に`store::projects::create_project`→初回の`create_speakers`→初回の`save_segments`を実行し、その`project_id`/`media_id`/`speaker_db_ids`を新しい`EditorScreen`に持たせる。この時点の`conn`は`this: &mut AppRoot`が保持するもの（`AppRoot`と`EditorScreen`は別々にDBを開くので、ここでは一時的に`AppRoot.conn`を使って初回保存し、新しい`EditorScreen`は自分用に再オープンする）。

7. **自動保存**——`build_editor_entity`の既存の200msタイマーとは別に、2秒間隔の`cx.spawn`タイマーを追加。`dirty`なら`sync_speaker_names`（統合で消えた話者は`delete_speaker`）→`save_segments`を実行し`dirty = false`、失敗したら`save_error: Option<String>`に記録してどこかに小さく表示（既存のステータス表示パターンを踏襲、大きなダイアログにはしない）。

   `dirty = true`をセットする箇所を全て列挙する（既存コードの正確な行を基準に、漏れなく当てる）:
   - `commit_focused`（本文コミット、:305）
   - `split_focused_segment`（分割、:358-360）
   - `merge_with_previous`（結合、:392-395）
   - `assign_speaker`（Cmd+1..9、:405）
   - `choose_row_speaker`（行ピッカー、:515）
   - `confirm_merge`（話者統合、:554-561——`speaker_db_ids`から`from`のエントリを取り除いておく）
   - 話者名の変更——専用のコミット関数が無く常時ライブバインドの`Textarea`なので、`render()`の「話者ごとにTextareaStateを用意する」ブロック（各speakerの`TextareaState`を`cx.new`する箇所）で`cx.observe(&state, |this, _, cx| { this.dirty = true; cx.notify(); })`を仕込む。

**変更するファイル**: `crates/asr-core/src/store/projects.rs`（新規）、`crates/asr-core/src/store/mod.rs`（`pub mod projects;`）、`apps/desktop/Cargo.toml`（`rusqlite`追加）、`apps/desktop/src/bin/app.rs`（`AppScreen::Projects`、`AppRoot.conn`、`EditorScreen`の永続化フィールド、`build_editor_entity`拡張、`start_pipeline`の保存呼び出し、自動保存タイマー、`main()`の起動分岐）。

**(2) 用語パネルとの関係**: (2)は元々「`EditorScreen`にSQLite接続と`project_id`を追加する」という同じ配線を必要としていた。(1d)でこの配線が先に済むため、(2)の残りスコープは新しいstore関数群（`load_all_terms`等）とパネルUIのみに縮小される。

**検証方法**: `cd apps/desktop && cargo +nightly build --bin app` / `clippy --all-targets` の後、実機（ユーザー操作）で——(1) 起動直後にプロジェクト一覧が出るか（初回は空状態＋新規作成ボタン） (2) 新規作成→パイプライン完了後、一覧に戻ってから開くと同じ内容で再オープンできるか (3) 再オープン後に本文編集・話者名変更・分割・結合・話者割当・話者統合をそれぞれ行い、アプリを再起動してから開き直しても反映されているか (4) 開発用ショートカット（`[json_path] [media_path]`）が相変わらず一覧に出現しない（DBを汚染しない）か (5) メディアファイルを移動/削除した状態で「開く」を押した場合にエラー表示になりクラッシュしないか。

**実装状況**: 実装済み。`crates/asr-core/src/store/projects.rs`（`list_projects`/`create_project`/`get_media_path`/`create_speakers`/`sync_speaker_names`/`delete_speaker`/`save_segments`/`load_project`、単体テスト`round_trips_a_project`込み）、`apps/desktop`（`app_db_path`/`open_app_db`、`AppScreen::Projects`＋一覧UI、`AppRoot.conn`、`EditorScreen`の`conn`/`project_id`/`media_id`/`speaker_db_ids`/`initial_speaker_names`/`dirty`/`save_error`フィールド、`ProjectContext`、`build_editor_entity`拡張、`persist`メソッド＋2秒間隔の自動保存タイマー、`open_upload`/`open_project`、`start_pipeline`成功時のプロジェクト作成、6箇所の`dirty=true`＋話者名`Textarea`への`cx.observe`）。

`cargo build --workspace`/`clippy --workspace --all-targets`（asr-core側の新規テスト込み）、`cd apps/desktop && cargo +nightly build --bin app`/`clippy --all-targets`とも警告・エラーゼロ。起動スモークテスト2パターン——(a) 引数無し→実際に`~/Library/Application Support/speaker-identification/app.db`が生成され、`sqlite3`で全テーブルのスキーマと`projects`が空であることを確認 (b) 開発用ショートカット（`[json_path] [media_path]`）実行後も同DBの`projects`件数が0のまま（DBを汚染しないという設計通り）——ともクラッシュ無し確認済み。

**確認済み**: 上記「検証方法」の(1)〜(5)全て——(2)(3)のAPI課金を伴うパイプライン実行含め、ユーザーが実機で確認済み。


### プロジェクトの上位概念化 ＋ 用語辞書の事前登録対応（実装済み）

**Context**: 前セッションで実装した「用語パネル」（エディタ画面から`terms`テーブルを編集するUI）には致命的な欠陥があった——**用語は本来「ASR実行前に登録してこそ意味がある」**（`keyterms`としてASRに送る＋読みベースの事後補正、両方とも登録済みの用語ありきの機能）にもかかわらず、`terms.project_id`が指す`projects`行は**パイプライン成功後に初めて作られる**設計（`apps/desktop/src/bin/app.rs`の`Next::Succeeded`分岐、"失敗した高額なASR実行の結果を消さない"という意図的な設計）だったため、用語パネルはエディタ画面（＝パイプライン完了後）でしか開けず、**そこで登録した用語がそのASR結果にも次のASR結果にも一切効かない**（`run_full_pipeline`の唯一の呼び出し箇所が`terms_db: None`固定——調査で確認済み、`docs/plan.md`のこのセクションの直前の版に詳細あり）。ユーザー指摘により発覚。

調査の結果、`run_full_pipeline`自身・`terms/store.rs`側の実装は完全に正しく機能する状態で存在しており（`project_id`さえ渡せばkeyterms送出も読みベース補正も動く）、問題は**アプリ側の画面遷移の順序と`terms`のスコープの取り方**だけだった。

対策として、ユーザー案（「プロジェクト」を「用語辞書」と「議事録・文字起こし（複数）」を束ねる上位概念にする）を採用する。これは**スキーマ変更ゼロ**で実現できる——`media`テーブルは元から`project_id`経由でプロジェクトに複数ぶら下がれる作り（1プロジェクト1メディアという制約はアプリ側の思い込みに過ぎず、スキーマ上の制約ではない）で、`terms.project_id`もそのまま使える。既存の実データ（`projects`1件、`media`1件）も「文字起こしが1件だけ入ったプロジェクト」としてそのまま有効なので、マイグレーション不要。

**新しい画面フロー**:
```
Projects（プロジェクト一覧、名前だけで新規作成できる）
  └─ 開く → ProjectDetail（プロジェクト詳細: 用語辞書 ＋ 文字起こし一覧）
       ├─ 用語辞書パネル（旧・用語パネルをここに移設。プロジェクトは
       │  アップロード前から存在するので、ここで登録した用語は次に
       │  このプロジェクトで実行するASRに実際に効く）
       ├─ 文字起こし一覧の各行「開く」→ Editor（既存のまま）
       └─ 「＋ 新規アップロード」→ Upload（project_idを引き継ぐ）
            └─ パイプライン成功 → このプロジェクトに新しい文字起こし
               （media行）を追加 → Editor
```

**設計**:

1. **`crates/asr-core/src/store/projects.rs`を「1プロジェクト1メディア」前提から解放**:
   - `create_project(conn, name) -> Result<String>` に変更（`media_path`引数と`media_id`の返り値を削除——プロジェクト単体を先に作れるようにする）。
   - 新規 `create_media(conn, project_id, media_path) -> Result<String>`（`create_project`が今までやっていた「メディア行を1つ作る」部分を切り出したもの。新しい文字起こしを既存プロジェクトに追加する時に呼ぶ）。
   - `ProjectSummary`から`media_path`/`segment_count`を削除し`transcript_count: i64`を追加。`list_projects`は`LEFT JOIN media ... GROUP BY p.id`に変更（文字起こしが0件のプロジェクトも一覧に出るように）。
   - 新規 `struct TranscriptSummary { media_id, media_path, segment_count }` ＋ `list_transcripts(conn, project_id) -> Result<Vec<TranscriptSummary>>`（`media`に`created_at`列が無いため`rowid DESC`で新しい順——列追加はマイグレーション機構が無いので避ける）。
   - `load_project(conn, project_id) -> LoadedProject` を `load_media(conn, media_id) -> LoadedMedia` にリネーム（`project_id`から`media_id`を逆引きする最初のクエリを削除するだけで、あとの中身は同じ）。
   - `get_media_path`は呼び出し元が無い（確認済み）ので削除。
   - `create_speakers`/`sync_speaker_names`/`delete_speaker`/`save_segments`は無変更（呼ばれるタイミングが「プロジェクト作成時」から「新しい文字起こし＝メディア作成時」に変わるだけで、意味は同じ——話者はこれまで通り**文字起こし単位**で採番し、プロジェクト内の複数文字起こし間で共有はしない。これは既存の「cross-project話者enrollmentはGUI未配線」という既知のスコープ外事項の延長として明示的に許容する）。
   - 既存テスト`round_trips_a_project`を新しいシグネチャに追随させ、「1プロジェクトに2つの文字起こしを追加し、それぞれ独立に`list_transcripts`/`load_media`できる」ケースを追加する。

2. **`apps/desktop/src/bin/app.rs`: `AppScreen`に`ProjectDetail`を追加**:
   ```rust
   enum AppScreen {
       Projects(ProjectsScreen),
       ProjectDetail(ProjectDetailScreen), // 新規
       Upload(UploadScreen),
       Editor(Entity<EditorScreen>),
       Settings { .. },
   }
   struct ProjectDetailScreen {
       project_id: String,
       project_name: String,
       transcripts: Vec<asr_core::store::projects::TranscriptSummary>,
       error: Option<String>,
       // 旧・EditorScreenの用語パネル関連フィールドをそのままここに移設
       term_order: Vec<String>,
       term_meta: HashMap<String, TermMeta>,
       term_editors: HashMap<String, (Entity<TextareaState>, Entity<TextareaState>)>,
       new_term_inputs: Option<(Entity<TextareaState>, Entity<TextareaState>)>,
   }
   ```
   `ProjectsScreen`/`UploadScreen`と同じく`AppRoot`のプレーンなフィールド（`Entity`ではない）——用語Textareaの遅延生成は`AppRoot::render`内で行う（`EditorScreen::render`で今やっているのと全く同じパターンをそのまま移設）。

3. **用語パネルをエディタから`ProjectDetailScreen`へ丸ごと移設**——`add_term`/`toggle_term_enabled`/`delete_term`メソッドは`impl EditorScreen`から`impl AppRoot`へ（`self.screen`の`ProjectDetail`分岐に対して操作するよう書き換え）。`EditorScreen`からは`term_order`/`term_meta`/`term_editors`/`new_term_inputs`フィールド・`TermMeta`構造体・関連するレンダーの遅延生成ループ・パネルUI・`persist`内の用語同期部分を全て削除し、前セッション実装前の状態（プレイヤー＋話者パネルの2枚のみ）に戻す。`project_id.is_some()`によるゲートも不要になる（`ProjectDetailScreen`は常にプロジェクトが存在する前提の画面なので）。
   - 自動保存: `EditorScreen`は`build_editor_entity`内で自前の2秒タイマーを持てたが、`ProjectDetailScreen`は`Entity`ではなく`AppRoot`のフィールドなので、同じ仕組みを`AppRoot`自身に持たせる（`main()`で`AppRoot`を`cx.new`する際に、同じ2秒間隔の`cx.spawn`タイマーを追加——`dirty`フラグは`ProjectDetailScreen`に持たせ、`AppScreen::ProjectDetail`の時だけ意味を持つ）。structural変更（追加・削除・有効/無効）は引き続き即時DB反映、surface/reading_kanaの編集だけこのタイマー経由。

4. **`ProjectsScreen`: 「新規作成」を名前だけのプロジェクト作成に変更**——ファイル選択の代わりに、常設の名前入力欄＋「作成」ボタンを一覧の上に追加（用語パネルの追加フォームと同じ見た目）。クリックで`create_project(conn, name)`→`AppScreen::ProjectDetail{ transcripts: vec![], term_order: load_all_terms(project_id) (空のはず), .. }`。各プロジェクト行の「開く」も`AppScreen::ProjectDetail`へ遷移するよう変更（旧`open_project`は名前を`open_project_detail`に変え、中身を`list_transcripts`＋`load_all_terms`の読み込みに差し替え）。

5. **`ProjectDetailScreen`のUI**: ヘッダー（プロジェクト名＋「← プロジェクト一覧」）／用語辞書パネル（旧実装のUIをそのまま）／文字起こし一覧（各行: ファイル名・セグメント数・「開く」——`open_media(media_id)`という新メソッド、旧`open_project`の後半部分＝`load_media`→`build_editor_entity`→`AppScreen::Editor`をほぼそのまま移設）／末尾に「＋ 新規アップロード」ボタン（`open_upload(project_id)`）。

6. **`UploadScreen`に`project_id: String`を追加**（必須化）。`open_upload`は`project_id`を引数に取るよう変更。`start_pipeline`の`run_full_pipeline`呼び出し（バックグラウンドタスク内）で、それまで`None`固定だった`terms_db`引数を`Some((&conn, &project_id))`に変更——**これが今回の本質的な修正**。バックグラウンドタスク内で使う`Connection`は既存の「WALモードなので複数コネクション可」という前例に倣い、そのタスク専用に`open_app_db()`で新しく開く。パイプライン成功時（`Next::Succeeded`）は`create_project`ではなく`create_media(conn, &project_id, &media_path)`を呼ぶよう変更（プロジェクト自体はもう存在するため）。

7. **「← プロジェクト一覧に戻る」の遷移先を、Editor画面・Upload画面の両方で`ProjectDetail`に変更**（`back_to_projects`を`back_to_project_detail(project_id)`にリネーム、`list_transcripts`＋`load_all_terms`を読み直して再構築）——プロジェクト直下の詳細に戻る方が「作業していたプロジェクトに戻る」という自然な導線になる。トップの`Projects`一覧への導線はプロジェクト詳細画面のヘッダーに残す。

**対象外（意図的なスコープ外）**:
- `crates/asr-cli`は変更しない——既に`--project-id`引数と`default`という仮のプロジェクト行を使う独自のワークフローを持っており、今回のGUI側の変更とは独立に動作する。
- プロジェクト内の複数文字起こし間で話者を共有する機能（cross-transcript話者enrollment）は今回も対応しない——既存の「cross-project話者enrollmentのGUI配線」という将来課題と同じ性質のものとして、そのまま積み残す。
- `media`テーブルへの`created_at`列追加は行わない（`rowid`順で代用、マイグレーション機構を新設するコストを避ける）。

**変更するファイル**: `crates/asr-core/src/store/projects.rs`（`create_project`/`create_media`/`ProjectSummary`/`list_projects`/`TranscriptSummary`/`list_transcripts`/`load_media`のリネーム・分割、`get_media_path`削除、テスト更新）、`apps/desktop/src/bin/app.rs`（`AppScreen::ProjectDetail`＋`ProjectDetailScreen`新設、`EditorScreen`から用語パネル関連を全削除、`UploadScreen.project_id`追加、`start_pipeline`の`terms_db`配線、`open_upload`/`open_project_detail`/`open_media`/`back_to_project_detail`/`add_term`等の`AppRoot`側メソッド、`ProjectsScreen`の新規作成UI変更、`AppRoot`への自動保存タイマー追加）。

**実装状況**: 実装済み。`crates/asr-core/src/store/projects.rs`（`create_project`をname-onlyに変更、`create_media`新設、`ProjectSummary`/`list_projects`を`transcript_count`ベースに変更、`TranscriptSummary`＋`list_transcripts`新設、`load_project`→`load_media`にリネーム、未使用だった`get_media_path`削除、テスト更新＋新規`a_project_can_hold_multiple_transcripts`）、`apps/desktop/src/bin/app.rs`（`AppScreen::ProjectDetail(Box<ProjectDetailScreen>)`新設——`clippy::large_enum_variant`を避けるためBox化、`TermMeta`＋用語パネル一式を`EditorScreen`から`ProjectDetailScreen`へ完全移設、`UploadScreen.project_id`追加、`start_pipeline`のバックグラウンドタスクで`terms_db`を`Some((&conn, &project_id))`に配線、`create_project`/`open_project_detail`/`back_to_project_detail`/`open_media`等の`AppRoot`側メソッド、`ProjectsScreen`の新規作成UIを名前入力＋作成ボタンに変更、`main()`に`ProjectDetail`用の2秒間隔自動保存タイマー追加）。

`cargo test -p asr-core`（37件全てpass、新規テスト含む）、`cargo build --workspace`/`clippy --workspace --all-targets`、`cd apps/desktop && cargo +nightly build --bin app`/`clippy --all-targets`とも警告・エラーゼロ。起動スモークテスト2パターン（開発用ショートカット経由・引数無しのプロジェクト一覧経由）ともクラッシュ無し確認済み。さらに実際の`app.db`（既存の実プロジェクト`clip_230s`1件を含む）に対して`list_projects`/`list_transcripts`/`load_media`と同一のSQLを`sqlite3`で直接実行し、期待通りの結果（`transcript_count=1`、既存のmedia行が正しく引ける）を確認——マイグレーション無しでの後方互換性を実データで確認済み。

**確認済み**: ユーザーが実機で操作し、機能的な完了を確認（「終わってるよ」）。UI上の細かい改善点は別途あるとのことだが、機能自体は完了扱いでよいとの判断——UI改善は次にユーザーから具体的な指摘が来た時点で個別に対応する（現時点では内容未共有）。

### 書き出し機能（実装済み）

**Context**: 文字起こし結果をプレーンテキスト/SRT/VTT/Markdownの4形式でファイルに書き出せるようにする（プランの「将来のタスク」からユーザーが選択）。

**現状の制約（調査で判明）**:
- `TranscriptSegment { speaker: i32, start_ms: u64, end_ms: u64, text: String }`（`crates/asr-core/src/pipeline/mod.rs:55`）。
- `apps/desktop/src/segment_list.rs:8`の`format_ms`は`mm:ss`までしか出さない——SRTの`HH:MM:SS,mmm`・VTTの`HH:MM:SS.mmm`には新しいフォーマッタが要る。
- `EditorScreen`は現在`media_path`を保持していない（`Transport::new(&media_path)`に渡した後は破棄される、`Transport`自身もpathを保持しない）——保存ダイアログの初期ディレクトリ/ファイル名のデフォルトに使うため、新しく保持する必要がある。
- `cx.prompt_for_new_path(directory: &Path, suggested_name: Option<&str>) -> oneshot::Receiver<Result<Option<PathBuf>>>`（`gpui-pre-0.3.4/src/app.rs:1595`）がネイティブ保存ダイアログ。既存の`pick_media`（`app.rs`、`cx.prompt_for_paths`で開くダイアログ）と対になる書き込み版。
- `EditorScreen::save_error`フィールドは**設定はされるが実際にはどこにもレンダリングされていない**（`app.rs`を全文grepして確認——既存の潜在バグ）。書き出し失敗を表示する場所として、ついでにこれを実際に表示するようにする。

**設計**:

1. **`crates/asr-core/src/export/mod.rs`（新規）** -- 純粋なデータ変換（UI非依存、`asr-cli`から将来使える）:
   ```rust
   pub fn to_plain_text(segments: &[TranscriptSegment], speaker_names: &HashMap<i32, String>) -> String;
   pub fn to_markdown(segments: &[TranscriptSegment], speaker_names: &HashMap<i32, String>) -> String;
   pub fn to_srt(segments: &[TranscriptSegment], speaker_names: &HashMap<i32, String>) -> String;
   pub fn to_vtt(segments: &[TranscriptSegment], speaker_names: &HashMap<i32, String>) -> String;
   ```
   - 話者名解決は`speaker_names.get(&speaker)`、無ければ`S{speaker}`にフォールバック（`speaker_label`と同じ規則）。
   - プレーンテキスト/Markdownは`[mm:ss] 話者: text`形式の行（Markdownは`- **話者** [mm:ss]: text`の箇条書き）。
   - SRT/VTTは新しい`format_timestamp(ms, sep: char)`ヘルパー（`,`/`.`の一文字だけが違う）で`HH:MM:SS{sep}mmm`を生成、標準の連番＋空行区切りブロックを構築。
   - 各形式・話者名フォールバック込みで単体テストを書く（既存の`terms/store.rs`等と同じ流儀）。

2. **`crates/asr-core/src/lib.rs`**: `pub mod export;`を追加。

3. **`apps/desktop/src/bin/app.rs`**:
   - `build_editor_entity`に`media_path: PathBuf`引数を追加し、`EditorScreen`に同名フィールドとして保持（3箇所の呼び出し元——`open_media`/パイプライン成功時/開発用ショートカット——はいずれも`Transport::new(&media_path)`の直前で既に`media_path`を持っているので、渡すだけで済む）。
   - `EditorScreen`に`export_picker_open: bool`（初期値`false`）を追加。
   - `enum ExportFormat { PlainText, Markdown, Srt, Vtt }`（`label()`でボタン文言、`extension()`で拡張子）。
   - `fn toggle_export_picker(&mut self, cx)` -- ピッカーの開閉。
   - `fn export(&mut self, format: ExportFormat, cx: &mut Context<Self>)`（`Window`は実装時に不要と判明——Textareaを新規作成しないため）:
     - `self.segments`から distinct な話者indexを集め、`speaker_label`で解決して`HashMap<i32, String>`を構築。
     - `format`に応じて`asr_core::export::to_*`を呼んで文字列を得る。
     - デフォルトディレクトリ＝`self.media_path.parent()`、デフォルトファイル名＝`{stem}.{ext}`。
     - `cx.prompt_for_new_path(dir, Some(&filename))` → `cx.spawn`で待って`Some(path)`なら`std::fs::write`、失敗時は`self.save_error`にセット。
     - ピッカーは書き出し実行時に閉じる。
   - UI: `AppRoot::render`の`AppScreen::Editor`分岐にある既存のヘッダー行（「← プロジェクトに戻る」がある場所）に`.justify_between()`で「書き出し」ボタンを右側に追加——クリックで`editor.update(cx, |v, cx| v.toggle_export_picker(cx))`、開いていれば4つの小さなボタンをインライン表示（フローティングpositionerは使わず、ヘッダー行に直接並べる単純な展開/折りたたみ——ボタン自体の画面位置が固定なので必要ない）、各クリックで`editor.update(cx, |v, cx| v.export(format, cx))`。
   - `AppRoot::render`の同じ`AppScreen::Editor`分岐で`editor.read(cx).save_error`を読み、あれば赤系のエラー行をヘッダーの下に追加（既存の`rgb(0xff8888)`系の色に合わせる）——書き出し失敗を実際に見えるようにする。他の`save_error`のセット箇所（自動保存失敗等）も副次的に表示されるようになる。

**変更するファイル**: `crates/asr-core/src/export/mod.rs`（新規）、`crates/asr-core/src/lib.rs`（`pub mod export;`）、`apps/desktop/src/bin/app.rs`（`build_editor_entity`/`EditorScreen`への`media_path`追加、`export_picker_open`、`ExportFormat`、`toggle_export_picker`/`export`メソッド、ヘッダーへの書き出しUI、`save_error`表示）。

**実装状況**: 実装済み。`crates/asr-core/src/export/mod.rs`（`to_plain_text`/`to_markdown`/`to_srt`/`to_vtt`＋`speaker_name`/`format_mm_ss`/`format_timestamp`ヘルパー、単体テスト6件——各形式のフォーマット・話者名フォールバック・空セグメント）、`crates/asr-core/src/lib.rs`（`pub mod export;`）、`apps/desktop/src/bin/app.rs`（`build_editor_entity`に`media_path: PathBuf`引数追加＋3箇所の呼び出し元更新、`EditorScreen`に`media_path`/`export_picker_open`フィールド、`ExportFormat`列挙型、`toggle_export_picker`/`export`メソッド、`AppRoot::render`のエディタヘッダーに書き出しボタン＋インライン4択＋`save_error`表示行）。

`cargo test -p asr-core`（43件全てpass、新規6件含む）、`cargo build --workspace`/`clippy --workspace --all-targets`、`cd apps/desktop && cargo +nightly build --bin app`/`clippy --all-targets`とも警告・エラーゼロ。起動スモークテスト（開発用ショートカット経由）でクラッシュ無し確認済み。

**確認済み**: ユーザーが実機で操作し完了を確認（「これも確認済みだよ」）。

### ループ再生UI（実装済み）

**設計**: `asr_audio::LoopRegion { start_ms, end_ms }`は既存だったが`Transport`は未対応（3bスパイクの`play_signalsmith`にはループ処理があったが、`Transport`実装時は意図的にスコープ外にした）。

1. **`Transport`にループ対応を追加**（`crates/asr-audio/src/transport.rs`）: `set_loop_region`/`loop_region`を追加し、`Shared`に`loop_region: Mutex<Option<LoopRegion>>`を持たせる。`render`（オーディオコールバック）を3bスパイクの`play_signalsmith`の`while filled < input_frames_needed`ループ（1コールバック内で複数回ラップしても継ぎ目なく詰め切る、ラップ時に`stretch.reset()`）にそのまま置き換え——ループ無効時は単一コピーで停止する元の挙動に自然に退化する。
2. **UI操作**（ドラッグ選択はタイムラインの既存クリックシークと衝突しやすいため見送り、キーマップのみ）: `Cmd+Shift+I`（ループ開始点=in-pointを現在位置に設定）／`Cmd+Shift+O`（終了点=out-point）／`Cmd+Shift+L`（ON/OFF、両方設定済みかつ`start < end`の時のみ有効）。タイムラインに設定済みのループ区間を半透明の帯として表示（`speaker_timeline()`に`loop_region: Option<(u64,u64)>`引数を追加、話者ブロックの上・プレイヘッドの下に描画）。

**変更したファイル**: `crates/asr-audio/src/transport.rs`（`Shared.loop_region`、`render`のループ対応、`set_loop_region`/`loop_region`、単体テスト4件）、`apps/desktop/src/speaker_timeline.rs`（`loop_region`引数＋帯の描画）、`apps/desktop/src/bin/speaker_timeline_preview.rs`（呼び出し元の引数追加のみ）、`apps/desktop/src/bin/app.rs`（`SetLoopStart`/`SetLoopEnd`/`ToggleLoop`アクション＋キーバインド、`EditorScreen`の`loop_start_ms`/`loop_end_ms`/`loop_enabled`フィールド、`set_loop_start`/`set_loop_end`/`toggle_loop`/`refresh_loop_region`メソッド）。

**実装状況**: 実装済み。`Transport::render`は`bin/playback_spike.rs`の`play_signalsmith`の検証済みアルゴリズムをそのまま移植（1コールバック内で複数回ラップする単体テスト`loop_region_can_wrap_more_than_once_in_a_single_callback`込みで4件、`Shared`を`Transport::new`のcpalデバイスオープンを経由せず直接構築してheadlessに実行）。タイムラインの帯は`rgba(0xffffff33)`、`loop_start_ms`/`loop_end_ms`が両方セットされていれば（`loop_enabled`のON/OFFに関わらず）常に表示——「設定済みの区間」を確認できるようにする設計（プラン原文の想定通り）。

`cargo test -p asr-audio`（5件全てpass、新規4件含む）、`cargo test -p asr-core`（43件）、`cargo build --workspace`/`clippy --workspace --all-targets`、`cd apps/desktop && cargo +nightly build --all-targets`/`clippy --all-targets`とも警告・エラーゼロ。起動スモークテスト（開発用ショートカット経由）でクラッシュ無し確認済み。

**実機フィードバックによる修正**: ユーザーから「ショートカットキーを押してもタイムラインに帯が出てこない＝機能そのものが確認できない」と報告。原因として、当初`Cmd+Shift+[`/`Cmd+Shift+]`をキーに割り当てていたが、**macOSでは`Cmd+Shift+[`/`Cmd+Shift+]`がSafari/Terminal等ほぼ全てのCocoaアプリで「前/次のタブに切り替え」というシステム/ウィンドウレベルの標準ショートカットになっている**ため、OS・ウィンドウ側でキーイベントが横取りされ、GPUI側のキーハンドラに一切届いていなかった可能性が高いと判断——`Cmd+Shift+I`（in-point）／`Cmd+Shift+O`（out-point）に変更した（動画/音声編集ソフトの一般的なIN/OUT点の慣習にも合致し、ブラケット文字特有のシフト時の文字化け問題も回避できる）。`Cmd+Shift+L`（トグル）は元々ブラケットと無関係なので変更していない。再度実機での確認をお願いする。

`cd apps/desktop && cargo +nightly build --bin app`/`clippy --all-targets`とも警告・エラーゼロ、起動スモークテスト（開発用ショートカット経由）でクラッシュ無し再確認済み。

**確認済み**: キー変更後、ユーザーが実機で操作し完了を確認（「ok, いい感じです」＋継ぎ目のノイズも気にならないとの報告）。

---

### ループUIをタイムライン側に寄せる＋IN/OUT矢印マーカー（実装済み）

**Context**: ループ再生UI（`Cmd+Shift+I`/`Cmd+Shift+O`/`Cmd+Shift+L`）は実機で動作確認済み。続けて追加したボタンUI（IN/OUT/トグル/範囲ラベル）は当初プレイヤー欄に置いたが、ユーザーから「ループの操作はタイムラインに対しての操作に近いので、そちらに寄せて配置した方がいい」との指摘。さらに「タイムラインの半透明の帯だけでは境界の正確な位置が分かりにくい」→「線を入れたい、プレイヘッドの白線とは被らないように」→「タイムライン下に矢印とかの方が良いのかも」という一連のフィードバックがあり、まとめて対応した。

**設計・実装内容**:

1. **ループ操作UIをプレイヤー欄からタイムラインのズームコントロール行に移設**（`apps/desktop/src/bin/app.rs`）。「タイムライン拡大: − +」の行に`.justify_between()`を追加し、右側に`IN`/`OUT`ボタン・ON/OFFトグル（`loop-toggle`、有効時は青背景の主要操作スタイル）・範囲ラベル（`mm:ss–mm:ss`または「未設定」）を配置。ボタンのスタイルは`zoom_button`（26x26固定）ではなく、プレイヤー欄で使っていたのと同じ「パディング付きの角丸ピル」スタイルを`loop_button`としてこの行のクロージャ内にもう一つ用意（`player_button`/`zoom_button`同様、使用箇所ごとに小さなスタイル用クロージャを都度用意するというこのファイル内の既存の流儀を踏襲）。プレイヤー欄にあった3行目（IN/OUT/トグル/ラベル）は削除。ロジック（`set_loop_start`/`set_loop_end`/`toggle_loop`）は変更なし、呼び出し元が変わっただけ。

2. **タイムラインにIN/OUT矢印マーカーを追加**（`apps/desktop/src/speaker_timeline.rs`）——`PathBuilder::fill()` + `add_polygon(&[apex, base_left, base_right], true)` + `build()` → `window.paint_path(path, rgb(0x5b9cf6))`で小さな上向き三角形（底辺幅8px、高さ6px）をIN/OUT位置それぞれに描画。タイムラインキャンバスの下端を底辺として上向きに頂点を伸ばす形。色はループトグルON時と同じ青系アクセント（`rgb(0x5b9cf6)`）——プレイヘッドの白い全高の縦線とは色・形の両方で明確に区別され、「被らない」という要望を満たす。既存の半透明の帯（区間全体を示す）はそのまま残し、境界の正確な位置を示す追加情報として矢印を重ねる。描画順序: 話者ブロック → 半透明の帯 → IN/OUT矢印 → プレイヘッド（プレイヘッドは常に最前面のまま）。

**変更したファイル**: `apps/desktop/src/bin/app.rs`（プレイヤー欄のループ行を削除、タイムラインのズーム行に移設）、`apps/desktop/src/speaker_timeline.rs`（IN/OUT矢印の描画追加）。

`cd apps/desktop && cargo +nightly build --bin app`/`clippy --all-targets`、`cargo build --workspace`/`clippy --workspace --all-targets`とも警告・エラーゼロ。起動スモークテスト（開発用ショートカット経由）でクラッシュ無し確認済み。

**実機フィードバックによる修正**: ユーザーから「IN/OUTを押したタイミングで矢印を表示してほしい」と報告——当初の実装は`speaker_timeline()`に`loop_region: Option<(u64,u64)>`（開始・終了の両方が揃って初めてSomeになる、半透明の帯と同じ条件）を渡していたため、**IN（開始点）だけ押してOUT（終了点）がまだ未設定の間は矢印もbandも一切描画されない**という設計ミスだった。`speaker_timeline()`の引数を`loop_start_ms: Option<u64>`/`loop_end_ms: Option<u64>`の2つに分離し、半透明の帯は引き続き「両方揃って`start < end`の時だけ」表示する一方、矢印マーカーは**それぞれ独立に、自分の値がSomeになった瞬間から即座に**表示するよう修正（`apps/desktop/src/speaker_timeline.rs`、呼び出し元の`apps/desktop/src/bin/app.rs`・`apps/desktop/src/bin/speaker_timeline_preview.rs`も2引数に追随）。

`cd apps/desktop && cargo +nightly build --all-targets`/`clippy --all-targets`、`cargo build --workspace`/`clippy --workspace --all-targets`とも警告・エラーゼロ。起動スモークテスト（開発用ショートカット経由）でクラッシュ無し再確認済み。

**確認済み**: ユーザーが実機で操作し完了を確認。

---

### 最後のウィンドウを閉じたらアプリを終了（実装済み）

**Context**: GPUIのデフォルト挙動は素のCocoaアプリと同様——`applicationShouldTerminateAfterLastWindowClosed`相当のオーバーライドが無いため、最後のウィンドウを閉じてもプロセスが常駐し続け、ウィンドウを再度開く手段も無い（Dockアイコンが残ったまま何もできない状態になる）。ユーザーから「最後のウィンドウを閉じた時にアプリを終了するようにしてほしい」との依頼。

**設計**: `main()`の`cx.bind_keys([...])`の直後に`cx.on_window_closed(|cx, _window_id| { if cx.windows().is_empty() { cx.quit(); } }).detach()`を追加。`App::on_window_closed`（`gpui-pre-0.3.4/src/app.rs:2387`）はウィンドウが閉じるたびに呼ばれるコールバック、`App::windows()`（同`:1242`）は現在開いている全ウィンドウのハンドル一覧、`App::quit()`（同`:1032`）はプラットフォーム標準の終了処理。

**変更したファイル**: `apps/desktop/src/bin/app.rs`のみ（`main()`に6行追加）。

**実装状況**: 実装済み。`cd apps/desktop && cargo +nightly build --bin app`/`clippy --all-targets`、`cargo build --workspace`/`clippy --workspace --all-targets`とも警告・エラーゼロ。起動スモークテスト（開発用ショートカット経由）でクラッシュ無し確認済み。

**確認済み**: ユーザーが実機で操作し完了を確認。

---

## 実機検証待ち（実装済み・ユーザーによる確認が未完了）

### セグメントリストのスクロール性能改善（実装済み）

**Context**: 3000件の合成データ（`debug-data/pipeline_transcript_stress.json`）でスクロールが「全然駄目、ガクガク」との報告。ユーザーは「見えない所でレンダリングしているのでは、GPUIでVirtualScrollできないか」と推測。調査の結果、**推測は誤りで`gpui::list`自体は正しく仮想化されている**ことが判明した——真因はアプリ側の`EditorScreen::render`が毎スクロールフレームごとに行っているO(n)の重い処理。

**調査結果（詳細）**:

1. **`gpui::list`/`ListState`はきちんと仮想化されている**（`gpui-pre-0.3.4/src/elements/list.rs`）。アイテムの累積高さは`SumTree<ListItem>`（`:62,291-305`）で管理され、スクロール位置からアイテムindexを引く処理はO(log n)のB-treeシーク（`StateInner::scroll_top` `:976-983`、`scroll` `:924-926`、`visible_range` `:886-896`）。`layout_items`（`:1027`）は表示範囲外に達したら即座にループを抜け（`:1066-1068`）、`render_item`は可視アイテム＋前後の overdraw 分しか呼ばれない（`:1074-1077,1137,1186`）。全アイテムを毎回舐める処理（`layout_all_items`、`:985-1025`）は`ListMeasuringBehavior::Measure`時のみ・初回1回きりで、このアプリはそのモードを使っていない（デフォルトの`Visible`のまま）。**3000件程度で仮想化自体がボトルネックになることは無い。**

2. **真因はアプリ側`EditorScreen::render`（`app.rs`）が`ListState`の`cx.notify()`（スクロールのたびに呼ばれる、`list.rs:959`）を受けて毎フレーム再実行され、その中で以下のO(n)処理を行っていること**:
   - `let segments = self.segments.clone();`（`:1202`）—— **3000件の`TranscriptSegment`（`String`のtextフィールド込み）をまるごとディープコピー、3000回のヒープアロケーション/フレーム**。
   - タイムライン呼び出し前の`self.segments.iter().filter(...).cloned().collect()`（`:2324-2329`）—— **同様にもう一度3000件分ディープコピー**（しかも`speaker_timeline`は`text`フィールドを一切使わないので完全に無駄）。
   - 話者パネル描画ループ内で`self.segments.iter().filter(|s| s.speaker == speaker).count()`を話者の数だけ呼んでいる（`:1925`あたり）—— **distinct_speakersの数だけ全セグメントを舐め直す、実質O(n×話者数)**。
   - `distinct_speakers`のBTreeSet構築（`:1235`）はO(n log n)、`current_ix`の`.position(...)`（`:1211`）はO(n)——これら単体は軽いが、上記の重い処理と合わさってフレーム落ちを起こしている。

**設計**:

1. **`EditorScreen.segments`の型を`Vec<TranscriptSegment>`から`Rc<Vec<TranscriptSegment>>`に変更**。読み取り専用の既存コード（`.iter()`・添字アクセス・`.len()`）は`Deref`でそのまま動く。ミューテーション箇所（`push`/`remove`/丸ごと差し替えなど、`commit_focused`/`split_focused_segment`/`merge_with_previous`/`delete_segment`/`restore_snapshot`等）は`Rc::make_mut(&mut self.segments)`でラップ——直前フレームの`Rc`クローンは`list()`のクロージャごとGPUIに破棄済みのはずなので、実際には`Rc::strong_count == 1`でディープコピーは発生しない想定（COW的に安全側に倒れる）。`render()`内の`let segments = self.segments.clone();`が**Rcのクローン（参照カウントのインクリメントのみ）**になり、3000件のディープコピーが消える。`UndoSnapshot`（Undo/Redo用のスナップショット）は今まで通り`Vec<TranscriptSegment>`のままでよい（`capture_snapshot`で`(*self.segments).clone()`、`restore_snapshot`で`self.segments = Rc::new(snapshot.segments)`——Undo自体は毎フレームではなく操作のたびにしか起きないので、ここのO(n)コピーは許容範囲、既存の「シンプルさ優先」の方針と整合）。

2. **`speaker_timeline()`（`apps/desktop/src/speaker_timeline.rs`）が受け取るセグメント情報を軽量化**——`text`を一切使っていないので、`Vec<TranscriptSegment>`ではなく新設の`#[derive(Clone, Copy)] struct SegmentBlock { speaker: i32, start_ms: u64, end_ms: u64 }`のスライス/Vecを受け取るように変更。呼び出し側（`app.rs`）の`.filter(...).cloned().collect()`を`.filter(...).map(|s| SegmentBlock{...}).collect()`に変更——`String`のクローンが完全に消え、Copyなタプル相当のコピーだけになる。

3. **話者パネルの人数カウントを1パスにまとめる**——`distinct_speakers`のBTreeSetを構築するのと同じループの中で`HashMap<i32, usize>`（話者ごとのセグメント数）も同時に集計し、パネル描画ループでは`speaker_counts.get(&speaker)`を参照するだけにする（現状の「話者の数だけ全セグメントを舐め直す」O(n×k)を、単純なO(n)の1パスに削減）。

**変更するファイル**: `apps/desktop/src/bin/app.rs`（`EditorScreen.segments`の型変更＋ミューテーション箇所の`Rc::make_mut`化、話者カウントの1パス化）、`apps/desktop/src/speaker_timeline.rs`（`SegmentBlock`新設、シグネチャ変更）、`apps/desktop/src/bin/speaker_timeline_preview.rs`（呼び出し元の追随）。

**対象外（今回は見送り）**: `current_ix`の線形探索（`.position(...)`）を二分探索に変える案——`self.segments`はstart_ms昇順という前提はありそうだが、3000件程度の線形探索自体は数マイクロ秒でありボトルネックの本命ではないと判断（調査エージェントの推奨事項にも含まれていない）。必要になれば別途対応する。

**実装状況**: 実装済み。`EditorScreen.segments`を`Rc<Vec<TranscriptSegment>>`化（`commit_focused`/`split_focused_segment`/`merge_with_previous`/`choose_row_speaker`/`assign_speaker`/`confirm_merge`/`delete_segment`の各ミューテーション箇所を`Rc::make_mut`でラップ、`capture_snapshot`/`restore_snapshot`は素の`Vec`との相互変換）、`speaker_timeline.rs`に`SegmentBlock`（`speaker`/`start_ms`/`end_ms`のみ、`Copy`）を新設しシグネチャ変更、話者パネルの人数カウントを`distinct_speakers`構築と同じ1パスの`HashMap<i32, usize>`に統合。

`cargo build --workspace`/`clippy --workspace --all-targets`、`cd apps/desktop && cargo +nightly build --all-targets`/`clippy --all-targets`とも警告・エラーゼロ。起動スモークテスト2パターン（`debug-data/pipeline_transcript_stress.json`3000件・通常の51件データ）でクラッシュ無し確認済み。

**未検証（実機での操作が必要——体感速度はこちらでは測定できない）**:

1. `debug-data/pipeline_transcript_stress.json`（3000件）を開発用ショートカットで開き、スクロールが滑らかになったか（体感で明確に改善しているか）
2. 通常の実データ（51セグメント）でも今まで通り問題なく動くか
3. セグメントの追加・削除・分割・結合・Undo/Redoが従来通り正しく動くか（`Rc::make_mut`化による退行が無いか）

---

## 再生エンジンの選択式対応（signalsmith-stretch / timestretch）—実装したが撤回済み

一度`timestretch`を第2の再生エンジンとして実装し（`PlaybackEngine`列挙型、`asr-audio`側のプル型フィードスレッド、アップロード画面での選択UI）、build/clippy/テストとも通した状態まで作ったが、**ユーザーの判断により撤回し、`signalsmith-stretch`一本に戻した**。理由: 実際に使うユーザーにとって2つのエンジンを切り替える価値が薄く、選ぶ必要性自体が無いとの判断。`crates/asr-audio`の関連コード（`PlaybackEngine`型、`EngineBackend`分岐、フィードスレッド）は削除し、`Transport::new(path: &Path)`は単一引数のシグネチャに戻した（ループ再生機能はこの変更の前から独立してあったもので、そのまま維持）。`apps/desktop`側のUploadScreen/Settingsに追加していた選択UIも全て削除済み。

`timestretch` 0.15への依存自体（`crates/asr-audio/Cargo.toml`）は残っているが、実際には未使用。

ビルド・clippy・テスト（`cargo test -p asr-audio`、`cargo build --workspace`/`clippy --workspace --all-targets`、`apps/desktop`側の`build`/`clippy --all-targets`）とも撤回後の状態で警告・エラーゼロを確認済み。起動スモークテストもクラッシュ無し確認済み。

---

## 未検証のまま残っている項目

Azure関連の項目は削除——Azure AI Speechは対応しない方針に確定したため。

ElevenLabsのプラン別STT同時実行数・日本語での埋め込みモデルの優劣・M1でのCoreML/ANEの実効果の3件は、現状不都合が無いため[FEATURE.md](../FEATURE.md)（「調査・検証タスク」セクション）へ移動済み。

**確認済み**: `signalsmith-stretch`の0.5×/2.0×での音質——ユーザーが実機（耳）で確認、問題なさそうとのこと。


（`timestretch`は再生エンジンの選択式対応として一度実装したが、ユーザー判断により撤回——上記「再生エンジンの選択式対応（signalsmith-stretch / timestretch）—実装したが撤回済み」参照。）
