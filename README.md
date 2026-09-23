# StillLaunch

A lightweight native Windows launcher for reusable application and workflow profiles.

StillLaunch groups applications, folders, files, URLs, commands, and waits into ordered profiles. It includes a native profile editor, optional resident and tray modes, Windows shortcuts, and startup integration.

Windows 10/11 x64 · Rust and Win32 APIs

複数のアプリ・フォルダ・ファイル・URL・コマンドを、プロファイル単位でまとめて起動する Windows 10/11 向けランチャー。

最重要の設計思想は **Zero Idle Architecture** — ユーザーが何もしていないなら Project Launcher も何もしない。

優先順位: 軽量性 > 信頼性 > 起動速度 > 使いやすさ > 見た目 > 機能数

## 現在のステータス: Phase 7 (Performance Hardening)

Phase 1 (Core Engine) 実装済み:

- 最小限の CLI 解析
- Config モデル / 読み込み / 検証(`%LOCALAPPDATA%\ProjectLauncher\config.json`)
- GUID によるプロファイル検索(大文字小文字・波括弧の有無を区別しない)
- 6 種類の Action: `application` / `folder` / `file` / `url` / `command` / `wait`
- Profile Executor: 設定順に実行、disabled をスキップ、失敗後も継続、結果を集約して部分失敗を報告

Phase 2 (Resident Core) 実装済み:

- Named Mutex (`Local\ProjectLauncher.Resident`) による単一起動。セッション単位なのでユーザーごとに1つ常駐できる
- Message-only Window (`HWND_MESSAGE`、クラス名 `ProjectLauncher.ResidentMessageWindow`)
- `GetMessage` によるイベント待機ループ。ポーリング・周期タイマー・ファイル監視は一切行わない
- 最小 Resident State: 起動時に一度だけ Config を読む。周期的な再読み込みはしない
- Worker Thread 実行インフラ: プロファイルを一時スレッドで実行し、Wait Action が Message Loop を塞がない

Phase 3 (Integration) 実装済み:

- WM_COPYDATA IPC: `dwData` = マジック+プロトコルバージョン (`0x504C0001`)、`lpData` = NUL 終端 UTF-16 JSON(`{"type":"run_profile","profile":"<GUID>"}`)。受信側は magic・サイズ上限(8KB)・UTF-16・JSON・GUID を検証してから受理する
- `--profile <GUID>` の Resident への転送: 常駐が起動中なら要求を渡して即 exit 0。常駐がいなければ従来どおり自分で実行する
- System Tray: 通知領域アイコン + 右クリックで「終了」メニュー。Explorer 再起動時は `TaskbarCreated` ブロードキャストでアイコンを再登録する
- 同時実行ポリシー: 受理した要求ごとに独立した Worker Thread を立てる(可変の共有状態なし)。上限は `MAX_CONCURRENT_WORKERS = 16` で、超過分は送信者に拒否として返る。spawn 失敗も panic ではなく拒否になる(wndproc 内の panic は FFI 境界で abort するため)
- トレイアイコン登録の失敗は起動を妨げない(ログオン時は taskbar 未起動があり得る)。Explorer 起動後の `TaskbarCreated` で登録される
- `--profile` の転送は `SendMessageTimeoutW`(3秒, SMTO_ABORTIFHUNG)で行い、Resident が詰まっていても送信側はブロックされない

Global Hotkey はこのバージョンでは実装していない。

Phase 4 (Windows Shortcuts) 実装済み:

- `--shortcut <GUID> [desktop|startmenu|both]` でプロファイルの .lnk を作成(既定は desktop)
- 生成先: Desktop は `FOLDERID_Desktop`、Start Menu は `FOLDERID_Programs`(`Start Menu\Programs`)。`SHGetKnownFolderPath` で解決するため OneDrive リダイレクトやロケールに追従する
- .lnk の内容: Target=自身の exe、Arguments=`--profile <GUID>`、作業ディレクトリ=exe のあるフォルダ、アイコン=exe の 0 番。**exe の絶対パスが埋め込まれる**ため、exe を移動したら `--shortcut` を再実行して作り直すこと(上書きで修復される)
- ファイル名は `<プロファイル名>-<GUID先頭8hex>.lnk`。プロファイル名由来部分は Windows の禁止文字を `_` に置換し、空・予約名(CON 等)なら GUID にフォールバック。GUID suffix により、サニタイズ後に同名になる別プロファイルの .lnk を上書きしない
- 既存 .lnk は上書き(再作成=修復)。削除機能はなし
- AppUserModelID は設定しない(採用しなかった判断)。タスクバーへピン留めすると全ショートカットが同一アプリとしてグループ化される

Phase 5 (Native GUI) 実装済み:

- 引数なし / `--settings` で同一のプロファイル管理ウィンドウを開く(Win32 標準コントロールのみ、GUI フレームワーク不使用)
- プロファイル一覧・実行・新規・編集・削除(削除は確認ダイアログ付き)
- プロファイルエディタ: 名前編集・アクション一覧・追加/編集/削除・上下移動・テスト実行
- アクションエディタ: 6 種類の type に応じたフィールド切り替え・ID 重複チェック・無効フラグ・種類別の検証(`validate_action` と同じルール)
- ショートカット作成: 選択プロファイルのポップアップメニューから desktop / startmenu / 両方
- 保存は検証 → 一時ファイル → リネームのアトミック書き込み。不正な入力は保存せずエラー表示
- テスト実行・プロファイル実行は別スレッドで行い、完了を `PostMessage` で画面へ通知(UI スレッドをブロックしない)
- Resident は要求到着時に config の更新時刻を比較し、変わっていれば再読込する(監視・ポーリングなし)。読み込み失敗時は直前の正常 config を保持する
- マニフェスト / Common Controls v6 は導入しない(クラシック表示は許容、依存を増やさない判断)

Phase 6 (Windows Integration) 実装済み(採用分のみ):

- **Resident の自動開始**: メインウィンドウの「Windows 起動時に常駐する」チェックで `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` に `"<exe>" --resident` を登録/解除。管理者権限不要・ユーザー単位。**exe の絶対パスが登録される**ため、exe を移動したらチェックを入れ直すこと(.lnk と同じ注意)
- **File / Folder picker**: アクションエディタの「参照...」ボタン。`application`/`file` は `GetOpenFileNameW`、`folder` は `SHBrowseForFolderW`(`BIF_NEWDIALOGSTYLE`)。`OFN_NOCHANGEDIR` でプロセスのカレントディレクトリを変えない
- 採用しなかった項目: Error notification(stderr ログと GUI 表示で十分)、Config Reload 通知(mtime 要求時再読込で正しさ上不要)、Installed App 探索

Phase 7 (Performance Hardening) 実装済み。Release profile はバイナリサイズを抑える設定になっている。具体的なCPU・メモリ・起動時間の測定値は、測定条件を含めて再現可能な形で別途確認する。

## 使い方

バイナリは Windows サブシステム(GUI)でビルドされるため、ダブルクリックしてもコンソールウィンドウは開かない。cmd/PowerShell から実行した場合は親の標準ハンドルが継承されるので `eprintln!`/`println!` の出力は従来どおり表示される。

```text
ProjectLauncher.exe                     プロファイル管理ウィンドウを開く
ProjectLauncher.exe --settings          同上(同じウィンドウ)
ProjectLauncher.exe --profile <GUID>    Resident が起動中なら転送、いなければ直接実行
ProjectLauncher.exe --resident          常駐を開始し、トレイの「終了」または WM_CLOSE まで待機
ProjectLauncher.exe --shortcut <GUID> [desktop|startmenu|both]
                                        プロファイル起動用の .lnk を作成(既定 desktop)
```

終了コード:

| code | 意味 |
|------|------|
| 0 | 全 Action 成功 / Resident への転送成功 / 常駐が正常終了 / 常駐が既に起動中 / ショートカット作成成功 |
| 1 | 1 つ以上の Action が失敗(部分失敗を含む、直接実行時のみ) |
| 2 | 引数エラー / config エラー / Resident が要求を拒否(不明な GUID・UIPI ブロック) / ショートカット作成失敗 / 未実装モード |

GUI での編集は OK 時に検証され、不正な内容はエラーメッセージを表示して保存しない。保存に成功した場合のみファイルが置き換わる。

### IPC の注意点

- 転送は「要求を Resident が受理した」までを保証する。実行結果は Resident 側の標準エラーに出る
- UIPI の制約により、異なる Integrity Level のプロセス間では `WM_COPYDATA` がブロックされる(例: 管理者権限で起動した Resident には非特権プロセスから送れない)

### 常駐の停止

- トレイアイコン右クリック →「終了」
- または message-only window への `WM_CLOSE` 送信

いずれも `DestroyWindow` → `PostQuitMessage` でループを抜け、トレイアイコン・Mutex・ウィンドウ・クラスを解放して終了する。

## 設定ファイル

`%LOCALAPPDATA%\ProjectLauncher\config.json` に配置する。例は `config.example.json` を参照。

- `version` は必須。現在 `1` のみ受理
- Profile は名前ではなく安定 GUID で参照する
- 未知のフィールド・未知の Action type はエラー(タイポの黙殺を防ぐ)
- `command` アクションのコマンド文字列と `application` の `arguments` は**そのまま**実行される。引用符の付け直しは行わず、クォートはユーザーの責任範囲
- `wait` の上限は 24 時間 (`86400000` ms)
- UTF-8 BOM 付きで保存されていても読み込める(Notepad の「UTF-8 (BOM付き)」保存を許容)。ファイルサイズ上限は 1MB
- `working_directory` は絶対パス必須。`application` の `path` に `"` や末尾の `\`/`/` は不可(起動時に必ず失敗するため保存時点で拒否)
- プロファイル名は空白のみでも不可
- GUI 表示中にファイルが外部変更されると、保存時に「上書きしますか?」と確認される(サイレントな変更破棄を防ぐ)
- `url` の値は RFC 3986 のスキームを必須とする。ただし Windows のドライブ文字 (`C:` 等) と区別するため、1 文字スキームは拒否する。`C:\work` のようなパスは `file` / `folder` アクションを使う
- `command` の `cmd` / `powershell` は PATH ではなく Windows システムディレクトリから絶対パスで解決される。カレントディレクトリに置かれた偽の `cmd.exe` は実行されない

## Action の JSON 形式

```jsonc
{ "id": "a1", "type": "application", "path": "C:\\Tools\\app.exe",
  "arguments": "-x 1", "working_directory": "C:\\Tools" }
{ "id": "a2", "type": "folder", "path": "C:\\Example\\Project" }
{ "id": "a3", "type": "file", "path": "C:\\Example\\notes.txt" }
{ "id": "a4", "type": "url", "url": "https://example.com" }
{ "id": "a5", "type": "command", "shell": "cmd", "command": "echo hi" }
{ "id": "a6", "type": "command", "shell": "powershell", "command": "Get-Date" }
{ "id": "a7", "type": "wait", "milliseconds": 500 }
{ "id": "a8", "type": "wait", "milliseconds": 500, "disabled": true }
```

`command` は選択したシェルのコマンドラインへ `cmd.exe /C ` / `powershell.exe -NoProfile -Command ` の後ろにそのまま連結される。
`powershell` の `-Command` は複数トークンに分割されるため、スペースを含むコマンドは自分で引用符で囲む:

```jsonc
// NG: -Format 以降が別の引数として解釈される
{ "id": "ps", "type": "command", "shell": "powershell",
  "command": "Get-Date -Format yyyy" }

// OK: 全体を 1 トークンにする
{ "id": "ps", "type": "command", "shell": "powershell",
  "command": "\"Get-Date -Format yyyy\"" }
```

「起動できた」ことと「アプリが正常終了した」ことは区別する。ランチャーが保証するのは「Windows へ起動リクエストを渡せた」ことまでで、子プロセスの終了は待たない。

## ビルド / テスト

```powershell
cargo build --release   # target\release\ProjectLauncher.exe
cargo test
```

依存 crate は `windows-sys` / `serde` / `serde_json` の 3 つのみ。
