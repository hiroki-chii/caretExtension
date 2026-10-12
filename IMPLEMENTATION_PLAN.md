# キャレット上部の入力モード表示アプリ：実現可能性と実装計画

実装後の仕様更新（2026-10-12）：Chromeで「Aだけ表示される」との実機報告を受け、変換状態を取得できない欄では、取得済みのモード・座標と入力停止後700msを使う互換表示を標準にした。古いActive範囲が残る欄は同じUIA要素の非変換状態・Enter／Escを補助情報として扱う。新規入力や要素変更で補助情報を破棄する。未確定文字があっても休止時に表示される場合がある。以下の厳密な変換中判定の計画は、トレイの「変換状態を取得できない欄では非表示」または `--strict-composition` を選んだ場合の動作として残す。

位置取得の更新：正確な矩形がない場合、UIAの隣接文字の端、または編集欄の左上付近を近似位置として採用する。エクスプローラーの上部アドレス・検索欄でユーザーが表示を確認した。近似位置は正確なキャレット追従を保証しない。変換中の待機は500ms間隔で再照会し、欠けた確定・取消通知からの復帰を補う。

Chromeページ内の断続的な非表示への更新：フォーカス切替途中で読み取り専用・非入力要素が返っても、250ms間隔・最大6回の再取得を行う。編集可能属性の確認は維持する。追加のフォーカス通知なしで表示が復帰する回帰テストを通過。実サイトでの改善は確認待ち。

追加調査で対象はYouTube上部検索欄と判明。日本語IMEのDBE／OEM切替キーとAlt＋バッククォートを通常文字入力と区別し、切替だけで非変換状態の補助情報が消えないようにした。通知種別とコントロール型の診断を追加。回帰テスト14件を通過し、実サイトの確認を継続する。

候補へのマウス移動時は、UIAフォーカスがListItemへ移ることを確認。直前のComboBoxと候補の親子／ControllerFor関係を限定的に調べ、検索欄内のキャレットを再取得できた場合はその検索欄を表示基準として保持する。同じネイティブ入力先の論理フォーカス変更では最終編集の待機時間を保持する。修正版でYouTubeの検索候補にマウスを移動しても表示が維持されることをユーザーが確認し、上記の確認待ちを更新した。その他のサイトへの検証は継続対象とする。

調査・更新日：2026-10-12。Rust構成、Chrome／Slackの優先検証、入力待機中のみの表示を反映。Rustの最小アプリを実装し、自アプリの描画・起動・終了、Chrome／エクスプローラーの一部入力欄を確認済み。最新の検証状況は `VALIDATION.md` を参照。

## 結論と前提

Windows向けの常駐アプリとして、実現できる見込みがある。ただし、外部アプリのキャレット位置とIME状態を取得できるかは、そのアプリ・入力欄・IMEに依存する。全アプリへの完全対応を前提にせず、技術検証で対応範囲を確定する。

- 開発環境からWindowsを想定し、初期対象はWindows 11 x64、日本語Microsoft IMEとする。
- ユーザーの目的は、現在フォーカスがある入力欄のキャレット上部に入力モードを表示すること。特定エディターの拡張機能ではなく、複数アプリを対象にする独立したデスクトップアプリを想定する。
- 作業開始時のフォルダーは空で、既存実装・Gitリポジトリはない。現在はRustソース、ビルド・配布用スクリプト、関連文書を作成済み。`rustc 1.95.0`、`cargo 1.95.0`、Windows SDK／MSVCリンカーを利用してビルドできることを確認済み。
- macOS/Linux、管理者権限で動くアプリ、セキュアデスクトップ、リモート先の入力欄は初期対応の対象外とする。

## 実現可能性

| 要素 | 判断 | 手段・制約 |
| --- | --- | --- |
| キャレット上部への描画 | 実現可能性が高い | フォーカスを奪わない、クリックを透過する最前面の小型ウィンドウ |
| 標準的なWindowsキャレットの位置取得 | 実現可能性が高い | `GetGUIThreadInfo` の `hwndCaret` / `rcCaret` を画面座標に変換 |
| ブラウザーや独自描画エディターの位置取得 | アプリごとに検証が必要 | UI Automationの `TextPattern2.GetCaretRange`、必要ならMSAAのキャレット情報 |
| 別プロセスの日本語IME状態取得 | 最大の検証課題 | 対象入力欄の既定IMEウィンドウへの状態照会などを試し、実際の入力結果と照合 |
| 入力中の非表示と待機後の再表示 | 実現可能性が高い | 入力イベントと単発タイマーで制御。IME変換中の判定は入力欄ごとの対応確認が必要 |
| 全アプリ・全IMEへの対応 | 保証できない | 状態や座標を公開しないアプリでは、専用連携など別方式が必要 |

Win32のキャレット座標はキャレット所有ウィンドウのクライアント座標であり、画面座標への変換が必要。[GUITHREADINFO](https://learn.microsoft.com/en-us/windows/win32/api/winuser/ns-winuser-guithreadinfo)

UI Automationはキャレットに対応するゼロ長範囲を提供するが、範囲の矩形取得では空配列になる場合がある。`GetCaretRange` が成功しただけで正確な座標を取得できたとは判断しない。[GetCaretRange](https://learn.microsoft.com/en-us/windows/win32/api/uiautomationclient/nf-uiautomationclient-iuiautomationtextpattern2-getcaretrange)、[GetBoundingRectangles](https://learn.microsoft.com/en-us/windows/win32/api/uiautomationclient/nf-uiautomationclient-iuiautomationtextrange-getboundingrectangles)

IMEの既定ウィンドウには `WM_IME_CONTROL` を送信できる。一方、現在のデスクトップ向け公開ドキュメントでは `IMC_GETOPENSTATUS` / `IMC_GETCONVERSIONMODE` の扱いが十分に記載されていないため、互換性に依存する検証候補として扱う。メッセージ送信が成功したことだけで、戻り値が信頼できる状態を表すとは判断しない。[ImmGetDefaultIMEWnd](https://learn.microsoft.com/en-us/windows/win32/api/imm/nf-imm-immgetdefaultimewnd)、[WM_IME_CONTROL](https://learn.microsoft.com/en-us/windows/win32/intl/wm-ime-control)

`ImmGetOpenStatus` / `ImmGetConversionStatus` は有効な入力コンテキストが前提で、別プロセスの任意の入力欄にそのまま使えると仮定しない。また、TSFのIME開閉・変換モードのコンパートメントはスレッドマネージャー単位であり、自アプリのTSF状態を読むだけでは前面アプリの状態を保証できない。[ImmGetConversionStatus](https://learn.microsoft.com/en-us/windows/win32/api/imm/nf-imm-immgetconversionstatus)、[Predefined Compartments](https://learn.microsoft.com/en-us/windows/win32/tsf/predefined-compartments)

## Chrome／Slackの対応方針

| 対象 | 見込み | 最初に検証する入力欄 |
| --- | --- | --- |
| Chrome | 対応できる見込み。入力欄ごとの確認が必要 | `input`、`textarea`、`contenteditable`、空欄、複数行、ページ拡大時 |
| Chrome上のSlack | Chromeの方式を適用する候補 | メッセージ入力欄、スレッド返信欄、編集画面 |
| Slackデスクトップ | Chromium系アクセシビリティ連携を適用する候補 | メッセージ入力欄、スレッド返信欄、検索欄 |

ChromiumはWindowsのUI Automationを実装し、現在のソースにはゼロ長範囲をキャレット用の幅1の矩形として扱う処理もある。Chromeへの対応見込みを支える根拠になるが、出荷版やすべてのWebエディターでの動作を保証するものではない。[ChromiumのUI Automation資料](https://chromium.googlesource.com/chromium/src/+/d4afc97b7/docs/accessibility/browser/uiautomation.md)、[キャレット矩形の実装](https://chromium.googlesource.com/chromium/src/+/refs/heads/main/ui/accessibility/ax_range.h)

Slackの公式技術資料にはElectronを使うデスクトップ構成が説明されている。Electronは支援技術の存在に応じてアクセシビリティ機能を有効にする。これらからSlackも同方式の検証対象にできると推論する。ただし、現行のインストール版でツリーが公開されるか、必要なイベント・パターンが提供されるかは未確認。[Slackの技術資料](https://slack.engineering/rebuilding-slack-on-the-desktop/)、[Electronのアクセシビリティ資料](https://www.electronjs.org/docs/latest/tutorial/accessibility)

通常起動のChrome／Slackを最初に検証する。ブラウザー起動オプションの強制変更や、Slack内部の `app.setAccessibilitySupportEnabled` を外部から呼べることは前提にしない。アクセシビリティを有効化した場合のChrome／Slack側のCPU・メモリ増加も測定する。対応判定は「座標・モード・変換中判定」の3項目を別々に記録する。

## 初期仕様

- **入力待機中のみ表示し、入力中は非表示にする。** 待機の初期定義は「入力欄にフォーカスがあり、最後の編集操作から700ms以上経過し、IME変換中ではない状態」。700msは暫定値で、設定から調整できるようにする。
- 入力欄へフォーカスが移った直後は、キャレット座標・モード・変換していないことを確認できれば表示する。文字入力、削除、貼り付けなどが始まったら即座に非表示にし、待機タイマーを更新する。
- IMEの未確定文字列や変換候補を操作している間は、キー入力が止まっても再表示しない。確定／キャンセル後、最後の編集操作から700ms経過し、変換していないことを再確認して表示する。
- 待機中のIMEモード切り替えはラベルを速やかに更新する。矢印キー／クリックによるキャレット移動は位置更新として扱い、移動中は古い位置の表示を消して有効な座標で再表示する。
- 最小試作は「あ」「A」の切り替えを検証する。初期リリースでは取得できたモードに応じて以下を表示する。

| 表示 | 意味 |
| --- | --- |
| あ | ひらがな |
| A | 直接入力または半角英数字 |
| 全A | 全角英数字（半角のAと区別するため明記） |
| カ | 全角カタカナ |
| ｶ | 半角カタカナ |

日本語IMEの開閉状態と `NATIVE` / `KATAKANA` / `FULLSHAPE` などの変換フラグを組み合わせる。「A」は英数字モードのラベルであり、大文字入力を保証する表示ではない。ローマ字入力／かな入力とCaps Lockは初期仕様では別表示しない。特殊な変換モードは、意味を確定できるまで非表示とする。[IME Conversion Mode Values](https://learn.microsoft.com/en-us/windows/win32/intl/ime-conversion-mode-values)

- 大きさは24～28 DIP、キャレット上端から4～8 DIPの間隔を初期値として、実機で調整する。
- 上部に表示余地がない場合は下部へ移し、モニターの作業領域内に収める。
- 最前面・非アクティブ化・クリック透過を実装し、文字入力、クリック、選択、IME変換を妨げないことを確認する。
- キャレット点滅だけを理由に表示を点滅させない。フォーカス消失、非入力欄、取得失敗、古い結果では表示を消す。
- 取得失敗を「A」に置き換えない。以前の入力欄の位置やモードを、新しい入力欄に流用しない。
- パスワード欄は識別できる場合に非表示。診断には入力本文・変換文字列を記録しない。
- トレイから一時停止／再開、終了ができる。色・サイズ・距離・アプリ除外設定は基本動作が成立してから追加する。

入力状態の制御：

| 状態 | 表示 | 復帰条件 |
| --- | --- | --- |
| 待機中 | 表示 | 次の編集操作で非表示 |
| 編集中／700ms待機中 | 非表示 | 最後の編集操作から700ms経過し、変換中ではないと確認 |
| IME変換中 | 非表示 | 確定／キャンセルが確認され、待機条件を満たす |
| 入力欄なし／位置・モード・変換状態を取得不能 | 非表示 | 必要な情報を再取得できる |

変換中の判定はUI Automationの `TextEdit` パターンにある `GetActiveComposition` とComposition／CompositionFinalized通知を候補とする。入力文字列の `GetText` は呼ばず、通知に含まれる文字列も利用・保存しない。対応パターンの有無と通知の信頼性を実機で確認する。[GetActiveComposition](https://learn.microsoft.com/en-us/windows/win32/api/uiautomationclient/nf-uiautomationclient-iuiautomationtexteditpattern-getactivecomposition)、[TextEditの通知](https://learn.microsoft.com/en-us/windows/win32/winauto/textedit-control-pattern)

変換状態を取得できない入力欄では、700ms無打鍵だったことだけで「変換終了」と推測しない。初期の厳密な動作では非表示とし、タイマーだけの近似動作は有効にしない。このため、キャレット表示には対応できても「変換中は常に非表示」という条件を満たせない入力欄があり得る。

## 言語とアーキテクチャの比較

以下は本用途に対する設計上の評価であり、実測ベンチマークではない。

| 候補 | 本用途での利点 | 主な負担 | 適する優先事項 |
| --- | --- | --- | --- |
| Rust + Win32 / COM | 小さなネイティブ常駐アプリを構成でき、OS連携の境界以外は所有権・型で誤りを抑えられる | Win32／COMには `unsafe` とスレッド・寿命管理が必要 | 軽さと長期保守のバランス |
| C++ + Win32 / COM | Windows SDKの宣言・公式サンプルを直接使いやすく、Rust同様に軽量な構成を取れる | RAIIによるリソース管理、ポインターやコールバックの寿命管理が必要 | Windows APIとの連携を最も直接的に実装 |
| C# + WinForms + Win32 / COM | トレイ・設定画面・通常のアプリ処理を比較的少ないコードで構成できる | 通常はCLRの常駐・配布方法を考慮する。OS連携の複雑さ自体は残る | 試作速度と設定UIの作りやすさ |

UI AutomationのネイティブAPIはWindows SDKのC/C++ヘッダーで定義されているため、Rust以外の第一候補はC++とする。ただし、C++へ変更すればChrome／Slackの対応範囲が広がるわけではない。同じOS APIを使う場合、制約は対象アプリが公開する情報に左右される。[UI Automation Overview](https://learn.microsoft.com/en-us/windows/win32/winauto/uiauto-uiautomationoverview)

C#のNative AOTには組み込みCOM連携の制約があり、通常のWinForms＋COM構成をそのままAOT化できるとは扱わない。ソース生成COMなどの追加設計とUIフレームワークの互換性確認が必要なので、軽量化の初期方針としては採用しない。[Native AOTの制約](https://learn.microsoft.com/en-us/dotnet/core/deploying/native-aot/)、[ソース生成COM](https://learn.microsoft.com/en-us/dotnet/standard/native-interop/comwrappers-source-generation)

今回の推奨は引き続きRustとする。小型表示とトレイが中心で大きな設定UIを必要とせず、Rust／Cargoも既に利用可能なため、C++へ変更する明確な利益は現時点では確認できていない。技術検証でRust側のAPI連携が開発上の障害になる場合に、C++を再評価する。

推奨アーキテクチャは、**ユーザーセッション内の1プロセス、Win32の表示・イベントスレッド、UI Automation用のMTAワーカー、明示的な表示状態機械**とする。

- 表示・イベント側：トレイ、小型ウィンドウ、軽い入力通知、待機タイマーを扱う。
- MTAワーカー：現在の入力欄だけを対象に、キャレット・IME・変換状態を取得し、世代番号付きの結果を返す。
- 状態機械：待機／編集中／変換中／取得不能を区別し、表示条件を一か所で決める。
- 重複する通知はまとめ、必要なUIAプロパティをキャッシュ要求で一括取得する。フォーカス変更・UI変更時に更新し、変化した位置やモードを古いキャッシュで代用しない。
- まず1プロセスで測定する。UIA呼び出しの長時間停止が確認された場合は、取得部分だけを再起動可能な補助プロセスへ分離し、追加メモリ・IPCの負担と比較する。

UIAのプロパティを個別に取得するとプロセス間呼び出しが増えるため、キャッシュによる一括取得が負荷削減に有効。軽量化では言語だけでなく、照会範囲・頻度・まとめ方・描画回数を優先する。[UIAキャッシュ](https://learn.microsoft.com/en-us/windows/win32/winauto/uiauto-cachingforclients)、[UIAのスレッド要件](https://learn.microsoft.com/en-us/windows/win32/winauto/uiauto-threading)

## 推奨構成

実装候補は **Rust + Microsoftの `windows` crate + Win32 API / UI Automation COM**。トレイと小型ウィンドウをWin32で構成し、文字や背景をGDI等で描画する。`windows-rs` はRustからWin32・COM・WinRTを利用するためのバインディングを提供しており、本用途をRustで実装できる。[Microsoft windows-rs](https://github.com/microsoft/windows-rs)

依存関係の機能を必要なAPIだけに限定し、試作で動作確認したバージョンを `Cargo.lock` に固定する。常駐部分はOSのメッセージループ、MTAワーカー、イベント、単発タイマーで構成する。小型ウィンドウと設定UIはネイティブで実装し、WebViewや非同期ランタイムを常駐させる構成は初期版に採用しない。Win32／COMの `unsafe` とリソース解放は専用モジュールに閉じ込める。

Rustはランタイムの追加導入を避けた配布と、少ない常駐メモリでの実装に適する。一方、CPU負荷はOSへの照会頻度とUI Automationの使い方にも左右され、言語を変えただけで軽量化が保証されるわけではない。

| コンポーネント | 責務 |
| --- | --- |
| FocusTracker | 前面ウィンドウ、フォーカス先、プロセス／スレッドの追跡 |
| CaretLocator | Win32 → UI Automation → 必要ならMSAAの順に位置を取得 |
| InputModeReader | フォーカス先のIME状態を読み、既知／取得不能を区別 |
| InputActivityMonitor | 編集開始と最終編集時刻を追跡。文字列やキー履歴は保持しない |
| CompositionTracker | IME変換中／非変換中／取得不能を区別 |
| IndicatorController | 同じフォーカス先の新しい結果と待機条件を使い、表示・非表示を決定 |
| OverlayWindow | DPI対応、位置調整、描画、非アクティブ化、クリック透過 |
| TrayApplication | 常駐ライフサイクル、設定、一時停止、終了 |

更新処理の方針：

1. `SetWinEventHook(WINEVENT_OUTOFCONTEXT)` で前面／フォーカス／キャレット位置変更を監視し、UI Automationの選択変更通知を補助的に利用する。コールバックは短時間で終了し、重い取得処理をワーカーへ渡す。[SetWinEventHook](https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-setwineventhook)
2. 表示／非表示は編集開始通知と700msの再設定可能な単発タイマーで制御する。UIAの文字変更通知に加え、最初の打鍵から速やかに隠すため、短時間の `WH_KEYBOARD_LL` コールバックを検証候補にする。コールバックは操作種別・時刻だけを伝え、入力を遮断せず `CallNextHookEx` で渡す。コールバック内ではCOM照会、文字変換、ファイル入出力をしない。貼り付け、音声入力などキー入力以外の変更はUIA通知で補完する。[LowLevelKeyboardProc](https://learn.microsoft.com/en-us/windows/win32/winmsg/lowlevelkeyboardproc)
3. UI Automationは専用MTAスレッドで実行する。呼び出し中のフォーカス変更・対象終了を考慮し、古い結果を破棄する。単純な待機タイムアウトでCOM呼び出し自体が停止したと仮定せず、多重呼び出しを防ぐ。必要なら取得処理を再起動可能な補助プロセスに分離する。[UI Automationのスレッド要件](https://learn.microsoft.com/en-us/windows/win32/winauto/uiauto-threading)
4. 外部ウィンドウへのメッセージは `SendMessageTimeout` を使い、送信の成否とメッセージの結果値を区別する。[SendMessageTimeout](https://learn.microsoft.com/ja-jp/windows/win32/api/winuser/nf-winuser-sendmessagetimeoutw)
5. Per-Monitor DPI Awareness V2を使用し、OSから取得する画面座標と描画寸法のDIPを明示的に変換する。負の画面座標、混在DPIも扱う。

軽量化の方針：

- 入力中はオーバーレイ描画と高頻度なキャレット取得を停止する。復帰直前に位置とモードを再取得する。フォーカス・編集・変換状態の必要な通知は引き続き受け取る。
- UIAの購読と検索は現在の入力欄と必要な祖先に限定する。全デスクトップの再帰探索を繰り返さない。
- 座標・ラベルが変化した場合だけ更新し、アニメーションや常時描画ループを設けない。
- 前面変更・入力欄変更時にキャッシュと購読を更新する。フォーカス先IDと世代番号で古い結果を破棄する。
- 待機表示中のIMEモード照会はイベントを優先する。通知を取得できない対象に限り、200～300ms程度の補助照会を検証し、反応速度と負荷を測定して調整する。
- 入力欄がない場合は定期的な座標／モード照会を止め、フォーカス通知で再開する。

ゼロ長のUIA範囲で矩形が得られない場合、複製した範囲を隣接文字へ拡張して推定する方式は追加候補とする。ただし、空欄・改行・折り返し・双方向テキスト・選択範囲では誤差が出るため、正確なキャレット座標と区別し、初期版では未検証の推定表示を有効にしない。

## 実装段階と完了条件

### 1. 開発環境と技術検証

- 既存のRust／Cargoを使い、Windows SDK／MSVCリンカーを確認して、診断用の最小Cargoプロジェクトを作る。
- 座標取得元、座標、フォーカス先ID、IME状態、取得時間、失敗理由を表示する。入力本文は取得しない。
- メモ帳、Chromeの `input`・`textarea`・`contenteditable`、Chrome上のSlack、Slackデスクトップのメッセージ／スレッド返信欄を優先し、Microsoft IMEで座標・モード・変換中判定を確認する。Edge／VS Codeも追加候補にする。
- IME ON/OFF、アプリ間の切り替え、ウィンドウごとの入力状態、空欄、クリック移動、矢印移動、改行、スクロールを検証する。
- APIの結果と、手動で入力した際の実際のモード／画面上のキャレット位置を照合する。
- 完了条件：メモ帳、Chromeの標準入力欄、Slackのメッセージ入力欄で、座標と「あ／A」を安定して取得し、変換途中での長い休止を待機と誤判定しない。未達の対象は制約と代替案を記録し、対応済みとは扱わない。アプリ・バージョン・入力欄・IMEごとに対応可否を記録する。
- 取得が成立しない主要対象がある場合は、対応範囲を限定するか、専用連携／TSFテキストサービスの別設計を評価する。DLLの注入やキーボード操作からの状態推測は初期方式に含めない。

### 2. 最小アプリ

- トレイ常駐、待機中のキャレット表示、編集開始時の非表示、700ms待機タイマー、変換中の非表示、取得不能時の非表示を実装する。
- 画面端、100%／150%／200% DPI、複数モニター、入力先終了に対応する。
- 完了条件：段階1で対応が確認できた対象で、文字入力・IME変換・クリックを妨げず、待機中だけ表示できる。

### 3. 対応範囲と実用性の改善

- 全角英数字、カタカナ、半角カタカナを追加し、誤判定を検証する。
- Word、Windows Terminal、Google日本語入力を追加の検証対象にする。対応済みとは事前に扱わない。
- 候補ウィンドウやツールチップとの重なりを確認し、取得できる情報の範囲で位置調整または非表示を行う。候補位置を全IMEで取得できるとは仮定しない。
- 色・サイズ・距離・除外アプリの設定を追加する。
- 完了条件：対応表と制約が明確で、未対応対象で誤ったラベルや位置が残らない。

### 4. 配布と最終検証

- まずWindows x64向けのRust製EXEを配布する。`cargo build --release` でビルドし、MSVCランタイムへの依存と静的リンクの可否を確認する。追加ランタイムなしの配布が成立することを別環境で検証する。
- 二重起動防止、終了時のフック解除、設定保存、長時間稼働を確認する。
- 手動導入・終了・制約をREADMEに記載する。自動起動登録はユーザーが設定で選択できる形とする。
- 完了条件：別のWindows環境で起動・入力追従・設定・終了を確認できる。

## 検証方針

- 自動テスト：モード変換、取得不能の扱い、画面端配置、DPI変換、フォーカス変更による古い結果の破棄。時間を制御できるテストで、連続入力時のタイマー更新、700ms前後、変換中の長い休止、確定／取消、取得不能時の非表示を確認する。
- 実機検証：Chrome／Slackで最初の打鍵、連続入力、削除、貼り付け、入力終了、IME切り替え、変換候補表示、確定前に数秒休止、空欄・文末・折り返し、選択・スクロール、アプリ切り替え、複数モニター、対象アプリの終了／無応答。
- 初期性能目標：入力開始で速やかに隠れ、最終編集から約700ms後に再表示する。待機中のモード変更は150ms以内を目指す。補助照会が必要な対象では未達になる可能性があるため、実測分布と使用環境を記録し、照会間隔とのバランスを調整する。現時点では未測定。
- アイドル時／入力中のCPU・メモリ使用量と長時間稼働での増加を測定する。イベントごとの全デスクトップ検索や、無制限の取得タスク生成を避ける。

## 現時点の成果と未確認事項

Rustによる最小アプリ、Windows向けビルド、トレイ・入力通知・キャレット／IME取得処理、700msの待機制御、互換／厳密な変換判定、診断機能を実装した。単体テストと自アプリの描画・起動・終了に加え、Chrome上部欄とエクスプローラーの一部入力欄で表示を確認した。ChatGPTのプロンプト欄でAからひらがなへ切り替えるだけで「あ」が出ることもユーザーが確認した。変換候補表示中の長い休止、Slack、長時間負荷、別環境での配布検証は残っている。実施した確認と未検証項目は `VALIDATION.md` に記録する。
