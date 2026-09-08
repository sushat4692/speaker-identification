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
| ASR 既定 / 2番手 | ElevenLabs Scribe v2 (batch) / Azure AI Speech Fast Transcription | |
| UI フレームワーク | `gpui-kit` 0.6（Apache-2.0） | 実体は`gpui-pre`。crates.io公式`gpui`とは別物 |
| 仮想スクロール | `gpui::list` + `ListState` | 可変行高対応 |
| 音声出力・タイムストレッチ | `cpal` + `signalsmith-stretch` 0.1.3 | |

**バージョン注意**: GPUIはpre-1.0で頻繁に破壊的変更——バージョン厳密固定必須。`ort`と`voice_activity_detector`は同居不可。

---

## ASR プロバイダ

**既定 ElevenLabs Scribe v2 (batch)**（`crates/asr-core/src/asr/elevenlabs.rs`）——`keyterms`1,000語×50文字、$0.22/hr。**2番手 Azure AI Speech Fast Transcription**（日本リージョンあり、未実装）。

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
projects(id, name, created_at)
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

**ユーザー辞書 = `terms`テーブルそのもの**（登録すればkeyterms送出とlindera読み取得の両方に効く）。`correction_threshold`は0.7（実データでは既定0.85だと発火しないケースがあった）。

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

> `Textarea`自身のキーマップと衝突する組み合わせ（`Cmd+←/→`等）は避けている——自前バインドが常に勝つため、衝突するとTextarea側の機能を黙って奪う。新しいキーバインドを足す際はこの衝突に注意。GPUIのキーストローク表記は`alt`のみ有効（`option`ではない）。

**行編集パターン**: `gpui::list`+`ListState`。非フォーカス行は静的テキスト、フォーカス行だけ実`Textarea`（N個の`TextareaState`を同時に作らない設計）。

---

## 評価基盤

ラベリングUI（`tools/diarization-viewer.html`）、`asr-cli eval`でDER算出。**残タスク**: 埋め込みモデルの選定（campplus vs WeSpeaker vs TitaNet、未検証）。

---

## 実装フェーズ

### Phase 2 — ASR と用語辞書
**未着手**: Azure実装、リクエスト並列化。

### Phase 3 — 編集 UI（GPUI、進行中）

完了: セグメントリスト・タイムライン・話者パネル・再生エンジン統合・行単位話者ピッカー・アップロード画面+実パイプライン統合・APIキー設定（Keychain、`Projects`/`Upload`どちらからも開ける独立の`AppScreen::Settings`）。詳細は削除済み——`run_full_pipeline`のAPIは`crates/asr-core/src/pipeline/mod.rs`、APIキーは`apps/desktop`の`load_api_key`/`save_api_key`参照。

**まだ手を付けていないスコープ**（将来）: 話者統合のUndo、話者の新規追加（既存セグメントに出現する話者のみが対象）、cross-project話者enrollmentのGUI配線。

---

## 実機検証待ち（実装済み・ユーザーによる確認が未完了）

以下は既に実装済みだが、ユーザーによる実機確認がまだ済んでいない（一部は実機ログで動作を確認済み）。

### エディタ画面の実機フィードバック6件（実装済み）

(1c)/(1d)/(1e)を一通り実装した後にユーザーから届いた6件のフィードバック。全て対応済み:

1. **タイムラインの中央追従が再生中に効かない** → **原因確定・修正済み・実機ログで動作確認済み**。`#timeline-scroll`自体がflexコンテナではなく、その直下の「左右パディング＋本編」の行が明示的な幅を持たないため、GPUIが行の幅を「子要素の合計」ではなく「ビューポート幅にストレッチ」して測定し、スクロール可能な超過幅をゼロと誤認していた（`max_offset.x == 0`→どんなオフセットを設定してもGPUI自身のクランプで即座に0に戻る）。行に明示的な合計幅（`half_viewport*2 + content_width`）を与えて解決。実機ログで`max_offset`が`0px`→`8340px`等の正しい値になり、実際の再生中に毎フレームのオフセットが連続的に変化する（0にクランプされ戻らない）ことを確認済み。
2. **一時停止中にドラッグ/スクロールバーでスクロールしたい** → `gpui_component::scroll::Scrollbar`を試したが、その内部実装（`gpui-base-0.6.0/src/scrollbar.rs`）はマウスダウンされた領域全体（100%/100%に自己サイズする）を「クリックでジャンプ」として扱い`cx.stop_propagation()`するため、シーククリックと共存できず、さらに専用の行に分離しようとすると今度は完全に非表示になる（コンポーネント自身のサイズ解決の癖に起因、深追いせず撤退）。**最終的に自前のドラッグ実装に切替**——`#timeline-scroll`に`on_mouse_down`/`on_mouse_move`/`on_mouse_up`を実装し、移動量が4px未満なら「クリック＝シーク」、それ以上なら「ドラッグ＝パン」として扱う（`begin_timeline_drag`/`continue_timeline_drag`/`end_timeline_drag`、`EditorLayoutDemo::timeline_drag`フィールド）。実機ログで、一時停止中に大きくオフセットが変化した後にシークが行われる、という実際のドラッグ操作に整合するパターンを確認済み。
3. **キーマップがTextareaフォーカス時しか反応しない** → **原因確定・修正済み**。GPUIの`Window::dispatch_action`はフォーカスされた要素からバブリング（上方向のみ）でディスパッチするため、`FocusHandle`を持つ要素がTextarea以外に一つも無いこのアプリでは、何もフォーカスされていない時にキーマップ用`.on_action`群へ絶対に届かなかった。`EditorLayoutDemo`に`focus_handle: FocusHandle`を追加し、外側のdivに`.track_focus(&self.focus_handle)`。`render()`の先頭で「何もフォーカスされていなければ自分のハンドルにフォーカスする」（`window.focused(cx).is_none()`のガード付きなので、Textareaフォーカス中は奪わない）。
4. **プロジェクト一覧に戻るボタンが無い（エディタ画面）** → `AppRoot::render`の`AppScreen::Editor`分岐に、`EditorLayoutDemo`自体には手を入れずヘッダーを追加（`flex_col`の兄弟として配置、`editor.clone()`は`flex_1().min_h(px(0.0))`でラップするだけで正しく縮む）。離脱前に`editor.update(cx, |view, cx| view.persist(cx))`で強制保存してから(1e)の`back_to_projects`を呼ぶ。
5. **再生速度をボタンでも変更したい** → プレイヤー欄のレート表示の両脇に「−」「＋」ボタンを追加、既存の`rate_down`/`rate_up`をそのまま呼ぶ（新規ロジック無し）。
6. **-30秒/-5秒/+5秒/+30秒のシークボタン** → `seek_back`/`seek_forward`を`ms: u64`引数付きに一般化（既存のキーマップは`Self::SEEK_STEP_MS`=5秒を渡すよう据え置き）、プレイヤー欄に4ボタン追加。

**ついでに対応**: セグメント行のタイムスタンプ表示をクリックするとその時刻へシークするようにした（`cx.stop_propagation()`で行自体の`focus_row`とは独立）。

**変更したファイル**: `apps/desktop/src/bin/editor_layout_demo.rs`のみ。ビルド・clippyとも警告ゼロ、診断用の一時的な`eprintln!`は全て削除済み。

**未検証（実機での操作が必要）**: (1)(2)は実機ログで強く裏付けが取れているが、ユーザー自身の目視確認はまだ。(3)〜(6)とタイムスタンプクリックは未検証——実機で個別に確認してもらう必要がある。

### (1e) 設定画面をUpload/Projects共通のトップレベル画面に昇格

**Context**: 「設定 (APIキー)」ボタンが`UploadScreen`にしか無く（`UploadMode::Settings`という`Upload`専用のサブモード）、プロジェクト一覧からは「新規作成」を経由しないと辿り着けなかった。ユーザーから「設定は新規にあるのは良くない」と指摘があり修正。

**設計**: `Settings`を`Upload`専用のサブモードから`AppScreen`のトップレベルの一variantに昇格——`Projects`・`Upload`どちらからも開けて元の画面に戻れるようにした:
```rust
enum AppScreen {
    Projects(ProjectsScreen),
    Upload(UploadScreen), // mode: UploadMode フィールドは削除
    Editor(Entity<EditorLayoutDemo>),
    Settings { api_key_input: Entity<InputState>, error: Option<String>, return_to: Box<AppScreen> },
}
```
`return_to`に開いた時点の画面（`projects`/`error`や`media_path`/`status`を保持したまま）を丸ごと退避し、「戻る」「保存」で復元（`std::mem::replace`で取り出し／書き戻し）。保存失敗時は`Settings`の`error`フィールドにその場で表示するよう修正——旧実装は`upload.status`にエラーを書いていたが、Settingsサブビューの早期returnに阻まれて**一切表示されない潜在バグ**だったため、ついでに直した。当初`ProjectsScreen`のヘッダーにも同じ「設定 (APIキー)」リンクを追加したが、その後ユーザーから「新規画面から設定ボタン削除してください」との指示があり、**`Upload`側のリンクは削除**——設定へのアクセスは`Projects`画面のみ（`Settings`自体は引き続き`return_to`で任意の画面から開けるトップレベルvariantのまま、Upload側の導線を無くしただけ）。

**変更したファイル**: `apps/desktop/src/bin/editor_layout_demo.rs`のみ。ビルド・clippyとも警告ゼロ、起動スモークテスト（クラッシュ無し）確認済み。

**未検証（実機での操作が必要）**: (1) プロジェクト一覧から直接「設定」を開き、保存→一覧に戻れるか（一覧の内容が保持されているか） (2) アップロード画面からも従来通り開けるか（`media_path`/進行状況が保持されているか） (3) 保存に失敗した場合その場にエラーが表示されるか。

---

### (1c) UI調整: プレイヘッドの中央追従＋話者バッジの色分け

**Context**: (1)/(1b)が実機で通ったあと、ユーザーから実際に使ってみての調整要望が2点。(a)「タイムラインの現在バーを常に中央に表示してほしい」——現状は`tools/diarization-viewer.html`から移植した端10%マージンのエッジトリガ追従なので、再生中プレイヘッドが左右に行ったり来たりして視線が安定しない。(b)「文字起こしの各行のスピーカー名に各スピーカーの色をつけて区別しやすくしてほしい。ポップオーバーの選択肢の様な表示がいいのかも」——現状のバッジは話者に関係なく一律`rgb(0x88aaff)`のプレーンテキストで、タイムラインや話者パネルの色分けと繋がっていない。

**ユーザーへの確認結果**: (1) 追従は**プレイヘッドが動いた時だけ**（一時停止中は手動横スクロールを許す。セグメントリスト側で踏んだ「自動追従が手動スクロールを引き戻す」バグと同じ轍を踏まないため） (2) 先頭・末尾でも**空白を足して完全に常に中央**（クランプではなく） (3) バッジは**枠線ピル**（`speaker_option_row`のピルと同じ見た目、枠と文字が話者色）。

---

**(a) プレイヘッドの中央追従**

現行の`follow_timeline_playhead`（`editor_layout_demo.rs:463-481`、10%マージンのエッジトリガ）を中央追従に置き換える。

1. **タイムラインのスクロール内容に左右の余白を追加**（`editor_layout_demo.rs`の`#timeline-scroll`の子、:1135-1151）。現状は`w(timeline_content_width(...))`の縦積み1枚だけが子になっているので、これを横並びの3要素`[余白][既存の縦積み][余白]`に変える。余白幅は`viewport_width / 2`（`self.timeline_scroll.bounds().size.width`から取る——前フレームの値だが、後述の通りオフセット計算と同じ値を使うので自己整合する）。

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

3. **呼ぶタイミング**——`last_highlighted_ix`と同じメモ化パターンで「プレイヘッドが動いた時だけ」にする。`EditorLayoutDemo`に`last_centered_playhead_ms: Option<u64>`を追加し、`render()`の現行`if self.transport.is_playing() { self.follow_timeline_playhead(...) }`（:560-562）を置き換える:
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

**変更するファイル**: `apps/desktop/src/bin/editor_layout_demo.rs`のみ（余白付きタイムライン構造、`follow_timeline_playhead`→`center_timeline_playhead`、`last_centered_playhead_ms`フィールド、`zoom_in`/`zoom_out`でのメモ無効化、`style_speaker_pill`ヘルパーとバッジ／`speaker_option_row`への適用）。`speaker_timeline.rs`は変更不要（余白はスクロールコンテナ側の兄弟要素として足すため、canvasの座標系は今のまま）。

**検証方法**: `cd apps/desktop && cargo +nightly build --bin editor-layout-demo` / `clippy --all-targets` の後、実機（ユーザー操作）で——(1) 再生中、プレイヘッドが常に画面中央に留まるか (2) 先頭・末尾でも中央のままか（左右に空白が出る） (3) 一時停止中にタイムラインを手で横スクロールしても引き戻されないか (4) ズームしても中央が保たれるか (5) タイムラインのクリックシークが従来通り正しい位置に飛ぶか (6) 各行のバッジが話者ごとの色のピルになり、タイムライン／話者パネルの色と一致しているか (7) **バッジをクリックした時のピッカーの表示位置がズレていないか（ズレていれば`-70.0`を再調整）**。

**実装状況**: 実装済み。`style_speaker_pill<E: Styled>`ヘルパーを追加し`speaker_option_row`のピルとセグメント行バッジの両方に適用（バッジは`speaker_color(seg.speaker)`で色分け）。`follow_timeline_playhead`を`center_timeline_playhead`（`offset.x = -playhead_px`、`viewport/2`の左右パディングにより自己クランプ）に置き換え、`last_centered_playhead_ms`によるメモ化で「プレイヘッドが動いた時だけ」再センタリング、`zoom_in`/`zoom_out`でメモをリセットして強制再センタリング。ビルド・clippyとも警告ゼロ、起動スモークテスト（`pipeline_transcript.json`+`clip_230s.mp4`、クラッシュ無し）確認済み。

**未検証（実機での目視・操作確認が必要）**: 上記「検証方法」の(1)〜(7)全て——特に(7)のピッカー位置ズレ（バッジの高さが`py(2px)+border 1px`分増えたため）は実機でないと分からない。

---

### (1d) プロジェクト履歴（過去の文字起こし結果の一覧・再オープン・編集の自動保存）

**Context**: ユーザーから「履歴機能ってまだ用意てなかったっけ」と質問があり、調査の結果**未実装**と確定した（`editor_layout_demo.rs`に`AppScreen::Upload`と`AppScreen::Editor`の2画面しか無く、過去のプロジェクト一覧・再オープン画面は存在しない。さらに調べると、パイプライン完了後の文字起こし結果は**一切ディスクに保存されておらず**、アプリを閉じると完全に消える——実際に課金・時間のかかるASR実行をした直後にウィンドウを閉じたら結果が丸ごと失われる、という見過ごせないギャップだった）。ユーザーへの確認の結果、(1) パイプライン完了後の編集もDBに保存する (2) 起動時にまずプロジェクト一覧を表示する、の両方を実装することで合意。

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
   enum AppScreen { Projects(ProjectsScreen), Upload(UploadScreen), Editor(Entity<EditorLayoutDemo>) }
   ```
   `AppRoot`に`conn: Connection`を追加（`main()`で`app_db_path()`を`create_dir_all`した上で一度だけ`asr_core::store::open`）。`render()`は現行の「Upload以外はEditorのはず」という2分岐のelse-unreachableパターンをやめ、3方向の明示的な分岐に変更（各画面の描画を`render_projects`/既存のUpload本体/`Editor`委譲に分ける）。

   一覧の各行は既存の話者パネル行と同じ見た目（ボーダー付き横並び、名前・日時・セグメント数）、末尾に「開く」ボタン。上部に常設の「新規作成」ボタン（`AppScreen::Upload(UploadScreen::default())`相当へ遷移）。「開く」は`store::projects::load_project`→`Transport::new(&media_path)`→（開けなければエラー表示、既存の「音声の読み込みに失敗しました」パスと同じ形）→`build_editor_entity`拡張版を呼んで`AppScreen::Editor`へ。

4. **`EditorLayoutDemo`に永続化フィールドを追加**: `conn: Connection`（自分専用に同じDBファイルへ再オープン——`store::open`がWALモードにしているため複数コネクションの同時アクセスは安全）、`project_id: Option<String>`、`media_id: Option<String>`、`speaker_db_ids: HashMap<i32, String>`、`dirty: bool`。開発用ショートカット経由の場合は`project_id`/`media_id`を`None`にし、自動保存は常に`project_id.is_some()`をガードして無効化する。

5. **`build_editor_entity`を拡張**——新しい保存先情報（`conn`, 既存プロジェクトなら`project_id`/`media_id`/`speaker_db_ids`、開発用ショートカットなら全部無し）を追加パラメータとして受け取るよう変更。呼び出し元3箇所（開発用ショートカット・アップロード完了後・プロジェクト再オープン後）がそれぞれ適切な値を渡す。

6. **アップロード完了時（`start_pipeline`のポーリングループの`Next::Succeeded`分岐）**——`build_editor_entity`を呼ぶ前に`store::projects::create_project`→初回の`create_speakers`→初回の`save_segments`を実行し、その`project_id`/`media_id`/`speaker_db_ids`を新しい`EditorLayoutDemo`に持たせる。この時点の`conn`は`this: &mut AppRoot`が保持するもの（`AppRoot`と`EditorLayoutDemo`は別々にDBを開くので、ここでは一時的に`AppRoot.conn`を使って初回保存し、新しい`EditorLayoutDemo`は自分用に再オープンする）。

7. **自動保存**——`build_editor_entity`の既存の200msタイマーとは別に、2秒間隔の`cx.spawn`タイマーを追加。`dirty`なら`sync_speaker_names`（統合で消えた話者は`delete_speaker`）→`save_segments`を実行し`dirty = false`、失敗したら`save_error: Option<String>`に記録してどこかに小さく表示（既存のステータス表示パターンを踏襲、大きなダイアログにはしない）。

   `dirty = true`をセットする箇所を全て列挙する（既存コードの正確な行を基準に、漏れなく当てる）:
   - `commit_focused`（本文コミット、:305）
   - `split_focused_segment`（分割、:358-360）
   - `merge_with_previous`（結合、:392-395）
   - `assign_speaker`（Cmd+1..9、:405）
   - `choose_row_speaker`（行ピッカー、:515）
   - `confirm_merge`（話者統合、:554-561——`speaker_db_ids`から`from`のエントリを取り除いておく）
   - 話者名の変更——専用のコミット関数が無く常時ライブバインドの`Textarea`なので、`render()`の「話者ごとにTextareaStateを用意する」ブロック（各speakerの`TextareaState`を`cx.new`する箇所）で`cx.observe(&state, |this, _, cx| { this.dirty = true; cx.notify(); })`を仕込む。

**変更するファイル**: `crates/asr-core/src/store/projects.rs`（新規）、`crates/asr-core/src/store/mod.rs`（`pub mod projects;`）、`apps/desktop/Cargo.toml`（`rusqlite`追加）、`apps/desktop/src/bin/editor_layout_demo.rs`（`AppScreen::Projects`、`AppRoot.conn`、`EditorLayoutDemo`の永続化フィールド、`build_editor_entity`拡張、`start_pipeline`の保存呼び出し、自動保存タイマー、`main()`の起動分岐）。

**(2) 用語パネルとの関係**: (2)は元々「`EditorLayoutDemo`にSQLite接続と`project_id`を追加する」という同じ配線を必要としていた。(1d)でこの配線が先に済むため、(2)の残りスコープは新しいstore関数群（`load_all_terms`等）とパネルUIのみに縮小される。

**検証方法**: `cd apps/desktop && cargo +nightly build --bin editor-layout-demo` / `clippy --all-targets` の後、実機（ユーザー操作）で——(1) 起動直後にプロジェクト一覧が出るか（初回は空状態＋新規作成ボタン） (2) 新規作成→パイプライン完了後、一覧に戻ってから開くと同じ内容で再オープンできるか (3) 再オープン後に本文編集・話者名変更・分割・結合・話者割当・話者統合をそれぞれ行い、アプリを再起動してから開き直しても反映されているか (4) 開発用ショートカット（`[json_path] [media_path]`）が相変わらず一覧に出現しない（DBを汚染しない）か (5) メディアファイルを移動/削除した状態で「開く」を押した場合にエラー表示になりクラッシュしないか。

**実装状況**: 実装済み。`crates/asr-core/src/store/projects.rs`（`list_projects`/`create_project`/`get_media_path`/`create_speakers`/`sync_speaker_names`/`delete_speaker`/`save_segments`/`load_project`、単体テスト`round_trips_a_project`込み）、`apps/desktop`（`app_db_path`/`open_app_db`、`AppScreen::Projects`＋一覧UI、`AppRoot.conn`、`EditorLayoutDemo`の`conn`/`project_id`/`media_id`/`speaker_db_ids`/`initial_speaker_names`/`dirty`/`save_error`フィールド、`ProjectContext`、`build_editor_entity`拡張、`persist`メソッド＋2秒間隔の自動保存タイマー、`open_upload`/`open_project`、`start_pipeline`成功時のプロジェクト作成、6箇所の`dirty=true`＋話者名`Textarea`への`cx.observe`）。

`cargo build --workspace`/`clippy --workspace --all-targets`（asr-core側の新規テスト込み）、`cd apps/desktop && cargo +nightly build --bin editor-layout-demo`/`clippy --all-targets`とも警告・エラーゼロ。起動スモークテスト2パターン——(a) 引数無し→実際に`~/Library/Application Support/speaker-identification/app.db`が生成され、`sqlite3`で全テーブルのスキーマと`projects`が空であることを確認 (b) 開発用ショートカット（`[json_path] [media_path]`）実行後も同DBの`projects`件数が0のまま（DBを汚染しないという設計通り）——ともクラッシュ無し確認済み。

**未検証（実機での操作が必要）**: 上記「検証方法」の(1)〜(5)全て——特に(2)(3)は実際にAPI課金を伴うパイプライン実行を経ないと確認できないため、ユーザー自身の操作が必要。

---

## 将来のタスク（未着手）

### (2) 用語パネル

**現状の制約（調査で判明）**: `crates/asr-core/src/terms/store.rs`の既存API（`load_terms`/`load_aliases`/`add_term`/`add_alias`）は追加専用——**削除・更新・IDでの参照が一切できない**。`load_terms`は`id`列を選択しておらず`Term`構造体自体にid フィールドが無い（`enabled=1`のフィルタも掛かっている）。`load_aliases`も同様にidを持たない（surface文字列同士の突合せのみ、`asr-cli`の一覧表示もこの制約に合わせてsurfaceマッチングしている）。パネルでの編集・削除には新しいstore関数が要る。

**設計**:

1. **`crates/asr-core/src/terms/store.rs`に追加**（既存の4関数の隣、`asr-core`がSQLiteを知る唯一の場所という規律を維持）:
   ```rust
   pub struct TermRow { pub id: String, pub term: Term }
   pub fn load_all_terms(conn: &Connection, project_id: &str) -> Result<Vec<TermRow>>; // enabledフィルタ無し
   pub fn update_term(conn: &Connection, id: &str, term: &Term) -> Result<()>;
   pub fn delete_term(conn: &Connection, id: &str) -> Result<()>; // term_aliasesはON DELETE CASCADEで自動削除
   pub fn set_enabled(conn: &Connection, id: &str, enabled: bool) -> Result<()>;
   pub struct AliasRow { pub id: String, pub term_id: String, pub alias_surface: String }
   pub fn load_all_aliases(conn: &Connection, project_id: &str) -> Result<Vec<AliasRow>>;
   pub fn delete_alias(conn: &Connection, id: &str) -> Result<()>;
   ```

2. **`EditorLayoutDemo`にSQLite接続を追加**——(1)で発行された`project_id`と、`asr_core::store::open(&db_path)`で開いた`rusqlite::Connection`を保持する。話者パネルと同じ左カラムの縦積みに3枚目のパネルとして追加するか、右カラムの下に折りたたみ式で追加するか——検証用HTMLに参考レイアウトが無いため、話者パネルと同じ構造（一覧＋インラインの追加フォーム＋各行に有効/無効トグル・編集・削除ボタン）を踏襲する。

3. **UI**: 話者パネルの`Textarea`常時編集パターンを流用——各用語行に`surface`/`reading_kana`用の2つの小さな`Textarea`、有効チェックボックス相当のトグル、削除ボタン。新規追加は下部に常設の空行フォーム（`surface`入力→Enter的な操作ではなく明示の「追加」ボタン、テキスト編集中の誤操作を避ける——このアプリ全体の「構造操作は明示的操作で」という一貫方針に合わせる）。

**変更するファイル**: `crates/asr-core/src/terms/store.rs`（新規関数群）、`apps/desktop/src/bin/editor_layout_demo.rs`（用語パネルUI、SQLite接続の保持）。

**検証方法**: 実データで用語を追加・編集・無効化・削除し、`asr-cli`側の`terms list`で同じDBを開いて整合性を確認。既存の3層用語補正（実行済みのパイプラインの結果には影響しない——補正は既にASR時点で適用済み、パネルでの変更は次回パイプライン実行から効く点をユーザーに明示）。

---

### (3) 書き出し機能

**設計**: 純粋なデータ変換なので`asr-core`に新モジュールとして実装（UI非依存、CLIからも将来使える）:
```rust
// crates/asr-core/src/export/mod.rs
pub fn to_plain_text(segments: &[TranscriptSegment], speaker_names: &HashMap<i32, String>) -> String;
pub fn to_srt(segments: &[TranscriptSegment], speaker_names: &HashMap<i32, String>) -> String;
pub fn to_vtt(segments: &[TranscriptSegment], speaker_names: &HashMap<i32, String>) -> String;
pub fn to_markdown(segments: &[TranscriptSegment], speaker_names: &HashMap<i32, String>) -> String;
```
SRT/VTTはミリ秒→`HH:MM:SS,mmm`（SRT）/`HH:MM:SS.mmm`（VTT）変換が要る（`format_ms`は`mm:ss`までしか出さないので新しいフォーマッタが要る）。

UI側: ツールバーに「書き出し」ボタン→話者パネルの`speaker_option_row`と同じ見た目のインラインピッカーで4形式から選択→`cx.prompt_for_new_path(default_dir, Some(default_filename))`で保存先を聞く（`App::prompt_for_new_path`、ネイティブ保存ダイアログ）→`std::fs::write`。話者名は`EditorLayoutDemo::speaker_name_states`から`speaker_label()`で解決してから`HashMap<i32,String>`に変換して渡す。

**変更するファイル**: `crates/asr-core/src/export/mod.rs`（新規）、`crates/asr-core/src/lib.rs`（`pub mod export;`追加）、`apps/desktop/src/bin/editor_layout_demo.rs`（書き出しボタン・保存フロー）。

**検証方法**: 実データで4形式それぞれ書き出し、SRT/VTTは実際に動画プレイヤー（QuickTime等）やブラウザで字幕として読み込んで表示崩れが無いか確認。

---

### (4) ループ再生UI

**設計**: `asr_audio::LoopRegion { start_ms, end_ms }`は既存だが`Transport`は未対応（3bスパイクの`play_signalsmith`にはループ処理があったが、`Transport`実装時は意図的にスコープ外にした）。

1. **`Transport`にループ対応を追加**（`crates/asr-audio/src/transport.rs`）: `set_loop_region(Option<LoopRegion>)`を追加し、`Shared`に`loop_region: Mutex<Option<LoopRegion>>`を持たせる。オーディオコールバック（`Transport::render`）で、ループ区間内かつ`pos`が`region.end_ms`相当のフレームに達したら`region.start_ms`相当のフレームへ巻き戻し、`stretch.reset()`を呼ぶ——3bスパイクの`play_signalsmith`で既に検証済みの継ぎ目防止パターンをそのまま踏襲。

2. **UI操作**（ドラッグ選択はタイムラインの既存クリックシークと衝突しやすいため見送り、キーマップで設定する方式を採用）:
   - `Cmd+Shift+[` — 現在の再生位置をループ開始点に設定
   - `Cmd+Shift+]` — 現在の再生位置をループ終了点に設定
   - `Cmd+Shift+L` — ループのON/OFF切り替え（開始・終了の両方が設定済みの場合のみ有効）
   タイムラインに設定済みのループ区間を半透明の帯として表示（`speaker_timeline()`のcanvasに、プレイヘッドと同様の要領で追加描画）。

**変更するファイル**: `crates/asr-audio/src/transport.rs`（ループ対応）、`apps/desktop/src/speaker_timeline.rs`（ループ区間の描画）、`apps/desktop/src/bin/editor_layout_demo.rs`（キーマップ・状態管理）。

**検証方法**: 実データでループを設定し、区間の継ぎ目でノイズ/ポップ音が無いか耳で確認（3bスパイクと同水準の「多分大丈夫」確認）。

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
