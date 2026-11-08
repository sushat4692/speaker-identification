# UIの刷新（後で着手する）

`docs/plan.md`から分離した、承認済みだが未着手のUI刷新プランの置き場（`FEATURE.md`と同じ「後で戻ってくる置き場」の位置づけ）。

**進捗**: Settings・Projects・ProjectDetail・Uploadの4画面は`docs/plan.md`側で実装済み（テーマ/`Button`/`GroupBox`/`Alert`/`Icon`への移行、`screen_header`/`back_nav_button`/`list_row`の共有ヘルパー導入、ProjectDetail・Uploadの2カラム化）。**残るのは`Editor`画面のみ**——このアプリで最も複雑・最も行数が多い画面（`speaker_timeline`のcanvas描画、`gpui::list`の仮想化、ドラッグ操作、`player_button`/`zoom_button`/`loop_button`等の既存ローカルクロージャ、話者バッジの`style_speaker_pill`等）で、他画面より一段階リスクが高いため意図的に後回しにしている。

---

## UIの刷新（デザインシステム基盤 ＋ Settings画面での検証）

### Context

基本機能（話者分離・文字起こし・編集・書き出し・ループ再生・探索モード等）が一通り揃ったところで、ユーザーから「UIがとんでもなく微妙なので、ゼロから整理し直してほしい」との要望。調査の結果、**現状のUIは`apps/desktop/src/bin/app.rs`全体（5画面・約4300行）で、色・ボタン・余白がすべて画面ごとに手書きのハードコード**であることが判明した（詳細は下記「調査で判明した現状」）。

一方、依存している`gpui-component`（`gpui-kit`経由）には**shadcn風の本格的なデザイントークン／テーマシステムと、Button・GroupBox・Alert・Tag・Separator等の完成品コンポーネント一式がすでに同梱・起動済み**（`gpui_kit::init(cx)`が`theme::init(cx)`を呼んでおり、`main()`はすでに`Theme::change(ThemeMode::Dark, ...)`まで呼んでいる）にもかかわらず、**アプリ側は一切使わずraw hexで全部書き直している**ことも判明した。つまり「ゼロから整理し直す」の最も筋の良いやり方は、独自の見た目を新しく考案することではなく、**すでに起動済みのデザインシステムに乗り換える**こと。

ユーザー確認済みの方針:
1. テーマ／部品ライブラリを本格採用（ハードコードした色・自前ボタンをやめ、`cx.theme()`のトークンと`Button`/`GroupBox`/`Alert`等の完成品コンポーネントに置き換える）
2. アイコン（lucideアイコンセット、`gpui-kit-assets`に内蔵済み・追加DL不要）を導入する
3. 対象範囲は「まず基盤を作り、1画面で検証してから段階的に展開」——**今回はSettings画面のみ**を本刷新の検証対象とし、残り4画面（Projects/ProjectDetail/Upload/Editor）は次回以降に見送る

### 調査で判明した現状（Explore結果の要約）

- **色**: `bin/app.rs`だけで19種類のhexリテラルが95箇所に散在、名前付き定数は無し。特に「ミュートテキスト」の役割だけで`0x9a9ba0`/`0x888888`/`0x666666`/`0x555555`の4種類が使い分けの理由なく混在、エラー赤も`0xff8888`/`0xff8080`の2種類。アクセント青`0x5b9cf6`だけは一貫（9箇所）。
- **ボタン**: `.cursor_pointer()`+`.on_click()`の手書き`div()`が33箇所。役割ごとに分類すると「プライマリCTA」7個で3種類のpadding、「アウトライン」9個で4種類の見た目、が微妙に食い違う。`hover`/`active`スタイルは**アプリ全体でゼロ**（ボタンがマウスホバーしても何も反応しない）。
- **既存の共通化**: 唯一の本物のヘルパーは`style_speaker_pill`（話者バッジ、2箇所で使用）。`player_button`/`zoom_button`/`loop_button`はコピペのローカルクロージャ（`loop_button`は`player_button`と完全に同じ内容とコメントで自認済み）。
- **アイコン**: ゼロ。play/pause等は`⏸`/`▶`等のUnicode文字で代用、`+`と`＋`（全角）が混在するなど細部もブレている。
- **gpui-componentが実際に持っているもの**（`gpui_kit::component::*`経由、全て導入済みの依存関係内・追加コスト無し）:
  - テーマ: `ActiveTheme`/`cx.theme()`——18個のセマンティックトークン（`background`/`foreground`/`primary`/`muted`/`destructive`/`border`等）＋135個の詳細トークン（`button_primary*`等）、ライト/ダーク両対応、`radius`/`spacing`/`typography`スケールも同梱。カスタマイズは`Theme::global_mut(cx)`での直接変更（要`Theme::sync_base(cx)`呼び出し）か`Theme::apply_semantic_config_str(json)`。**注意点**: `primary`だけ書き換えても`Button::primary()`の見た目は変わらない（`button_primary*`系トークンも別途上書きが必要、`gpui-component-0.6.0/src/theme/mod.rs`のテストで確認済みの既知の落とし穴）——正確なJSONスキーマ/上書きキーは実装時に`theme/schema.rs`を直接読んで確定させる。
  - 部品: `Button`（`.primary()/.secondary()/.danger()/.ghost()/.outline()`等の10バリアント×4サイズ、`.icon()`/`.loading()`/`.disabled()`/`.tooltip()`込み）、`GroupBox`（タイトル付きパネル）、`Alert`（エラー表示）、`Tag`/`Badge`（バッジ・チップ）、`Separator`、`Checkbox`/`Slider`/`Progress`（導入済み）、`Icon`/`IconName`（lucideアイコン約98種、`play`/`pause`/`undo`/`redo`/`settings`/`user`/`chevron-*`等この用途に合うものが揃っている）。
- **前提の穴（要対応）**: アイコンを使うには`main()`の`gpui_kit::application()`に`.with_assets(gpui_kit::assets::Assets)`を追加する必要がある（現状未設定のため、今`Icon`を置いても空白になる）。また調査時点で`~/.cargo/registry/`自体が丸ごと存在しなかった（`cargo fetch`ないし`cargo build`で復元される想定、実装開始時に最初に確認する）。

### 設計

**Phase A: 基盤（全画面に効く土台、今回必ずやる）**

1. `apps/desktop/src/bin/main`（`main()`）: `gpui_kit::application()` に `.with_assets(gpui_kit::assets::Assets)` を追加——これが無いとアイコンが一切表示されない。
2. テーマのアクセントカラーを現行の青（`0x5b9cf6`）に合わせて上書き——`main()`の`gpui_kit::init(cx)`直後に1回。具体的な上書き方法（`Theme::global_mut`直接編集＋`sync_base` vs `apply_semantic_config_str`）と対象トークン（`primary`/`accent`/`ring`/`button_primary*`一式）は、`cargo fetch`後に`theme/schema.rs`・`theme/mod.rs`を直接読んで確定する（見切り発車で書かない——実行時パニックや意図しない配色になるリスクがあるため）。ダークモード固定（`ThemeMode::Dark`）は現状維持、ライト/システム追従には今回は変更しない。
3. `apps/desktop/src/speaker_timeline.rs`の`speaker_color()`——8色固定パレットを、テーマの`chart_1..chart_5`ないし`ColorName`ベースに置き換えるかは**保留**（話者色は全画面のタイムライン・バッジ・スウォッチに波及する変更で、Settings画面という今回の検証対象には影響しないため、Phase Aでは触らない——次にEditor画面へ展開する時に判断）。

**Phase B: Settings画面をデザインシステムに全面移行（今回の検証対象）**

対象: `apps/desktop/src/bin/app.rs`の`AppScreen::Settings`分岐（現状 `3623-3704`付近、要再確認——直近の編集で行番号がズレている可能性があるので実装時に`grep`し直す）。

- 外枠のカード（現状: 手書き`div()`に`rgb(0x1e1e1e)`背景＋`items_center justify_center`）→ `GroupBox`（タイトル付きパネル）または`ThemeStyled::popover_style(cx)`を使った`div()`のどちらが見た目に合うか実装時に両方試して選ぶ。
- タイトル「APIキー設定」・ステータス行（`ElevenLabs APIキー: {status}`）→ 生の`px(18.0)`/`px(13.0)`をやめ、`cx.theme().typography_tokens()`のスケール（`lg`/`sm`相当）か、GPUI組み込みの`.text_lg()`/`.text_sm()`に置き換え。色も`cx.theme().foreground`/`cx.theme().muted_foreground`に。
- APIキー入力欄——`Input`自体は既存のまま維持しつつ、囲みのdivのボーダー・背景をテーマトークン（`cx.theme().input`/`cx.theme().popover`等）に置き換え。
- エラー表示（現状: `rgb(0xff8888)`のプレーンテキスト）→ `Alert::error(...)`コンポーネントに置き換え。
- 「戻る」「保存」ボタン（現状: 手書き`div()`×2、微妙に違うpadding）→ `Button::new("settings-cancel").outline().label("戻る")` / `Button::new("settings-save").primary().label("保存").icon(IconName::Check)`に置き換え——クリックハンドラのロジック（`cancel_settings`/`save_settings`呼び出し）自体は変更なし。
- アイコン: タイトル横に`IconName::Settings`、「← APIキー設定」的な戻り導線があれば`IconName::ChevronLeft`（現状のSettings画面自体には戻るリンクは無く、`Upload`/`Projects`側にあるので、Settings画面内はタイトルアイコンのみで十分）。

**対象外（今回は見送り）**:
- Projects / ProjectDetail / Upload / Editor の4画面——Settings画面での検証結果を見てから、同じパターン（Button/GroupBox/Alert/Icon置き換え）を横展開する。特にEditorはこのアプリで最も複雑・最も行数が多い画面（`speaker_timeline`のcanvas描画、`gpui::list`の仮想化、ドラッグ操作等）なので最後に回す。
- `player_button`/`zoom_button`/`loop_button`の統合、`style_speaker_pill`の話者パネルスウォッチへの適用——Editor画面の刷新時にまとめて対応。
- ホバー/アクティブ状態のスタイル追加——`Button`コンポーネントへの置き換えで自動的に得られる部分はそれで済ませ、まだ手書き`div()`のまま残る箇所（他画面）への追加は見送り。
- ライト/ダーク自動追従（`Theme::sync_system_appearance`）——今回はダーク固定を維持。

### 検証方法

1. `cargo fetch`（または`cargo build`）で`~/.cargo/registry`を復元できることをまず確認。
2. `cd apps/desktop && cargo +nightly build --bin app` / `clippy --bin app --all-targets`——警告・エラーゼロ。
3. 起動スモークテスト（引数無し→プロジェクト一覧→「設定」リンクからSettings画面を開く）でクラッシュ無し確認。
4. **実機での見た目確認（ユーザー、必須）**——(1) 全体の配色・余白がテーマに乗って統一感が出ているか (2) 戻る/保存ボタンがBoxonコンポーネントとして自然に見えるか、クリック・ホバー時の反応があるか (3) エラー時の`Alert`表示が分かりやすいか (4) アクセントカラーが従来の青の印象を保っているか (5) この画面の方向性で残り4画面へ展開してよいか、それとも別の調整が要るか。
