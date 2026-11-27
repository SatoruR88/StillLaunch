# StillLaunch

[English](#english) · [日本語](#日本語)

> **Note:** this branch is a from-scratch rewrite (raw Win32, no serde) that builds without warnings (cross-compiled for `x86_64-pc-windows-gnu`, unit tests pass) but has **not been run on Windows yet**.
> **注意:** このブランチは作り直したコード（Win32 直接・serde なし）で、ビルドは警告なしで通り単体テストも成功していますが（Windows 向けにクロスコンパイル）、**まだ Windows で動作確認していません**。

## English

StillLaunch runs a saved list of steps — start apps, open folders, files and URLs, run `cmd`/PowerShell commands, wait — with one click, a shortcut or the tray.

- One-window editor: profiles on the left, the selected profile's steps and a step form on the right.
- Desktop / Start-menu shortcut per profile.
- Optional tray resident (right-click: run a profile, edit, exit), optionally started with Windows.
- Single dependency (`windows-sys`); the resident sleeps in `GetMessageW` and does nothing while idle.

### Build

Windows 10/11 and Rust with edition 2024: `cargo build --release` → `target\release\ProjectLauncher.exe` (`cargo test` runs the parser and CLI tests).

### Command line

| Command | Action |
| --- | --- |
| `ProjectLauncher.exe` / `--settings` | Open the editor. |
| `--profile <GUID>` | Run a profile (sent to the resident if one is running). Exit code 1 if a step failed. |
| `--resident` | Start the tray resident. |
| `--shortcut <GUID> [desktop\|startmenu\|both]` | Create a shortcut (default: desktop). |

### Configuration

`%LOCALAPPDATA%\ProjectLauncher\config.json` — edited by the editor, or by hand (see [`config.example.json`](config.example.json)). Steps run in order; a failed step does not stop the rest; launched apps are not waited for. Commands run as written under your account, so only run profiles you trust. Shortcuts and the autostart entry point at the current exe path; recreate them after moving it.

Files from the previous version load unchanged. The per-step `"id"` field is still accepted but no longer written, so a file saved by this version cannot be read by the old one.

## 日本語

StillLaunch は、保存した手順（アプリ起動、フォルダ・ファイル・URL を開く、`cmd`/PowerShell コマンド、待機）をワンクリック・ショートカット・タスクトレイからまとめて実行するランチャーです。

- 1 画面の編集ウィンドウ：左にプロファイル一覧、右に選択中プロファイルのステップ一覧と編集欄。
- プロファイルごとにデスクトップ／スタートメニューへショートカットを作成。
- タスクトレイ常駐（右クリックでプロファイル実行・編集・終了）。Windows 起動時の常駐も設定可能。
- 依存は `windows-sys` のみ。常駐中は `GetMessageW` で待機し、何もしていない間は CPU を使いません。

### ビルド

Windows 10/11 と Rust（edition 2024）で `cargo build --release` → `target\release\ProjectLauncher.exe`（`cargo test` で設定ファイル読み込みと引数解析をテスト）。

### コマンドライン

| コマンド | 動作 |
| --- | --- |
| `ProjectLauncher.exe` / `--settings` | 編集ウィンドウを開きます。 |
| `--profile <GUID>` | プロファイルを実行します（常駐中なら常駐プロセスに渡します）。失敗したステップがあれば終了コード 1。 |
| `--resident` | タスクトレイに常駐します。 |
| `--shortcut <GUID> [desktop\|startmenu\|both]` | ショートカットを作成します（省略時はデスクトップ）。 |

### 設定ファイル

`%LOCALAPPDATA%\ProjectLauncher\config.json`。編集ウィンドウから、または直接編集できます（[`config.example.json`](config.example.json) 参照）。ステップは上から順に実行し、失敗しても残りは続行、起動したアプリの終了は待ちません。コマンドは書かれたとおりにユーザー権限で実行されるため、信頼できるプロファイルだけを実行してください。ショートカットと自動起動は現在の exe の場所を指すので、移動したら作り直してください。

旧バージョンの設定ファイルはそのまま読めます。各ステップの `"id"` は読み込み時に受け付けますが保存時には書かないため、このバージョンで保存したファイルは旧バージョンでは読めません。

## License / ライセンス

MIT License.
