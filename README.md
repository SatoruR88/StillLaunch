# StillLaunch

[English](#english) · [日本語](#日本語)

## English

StillLaunch is a Windows launcher for running groups of apps and other actions from saved profiles.

### Features

- Create, edit, and run profiles in the built-in window.
- Launch applications, open folders and files, visit URLs, run `cmd` or PowerShell commands, and add wait steps.
- Create profile shortcuts on the Desktop or in the Start menu.
- Run in resident mode with a tray icon, or start the resident process when Windows starts.

### Build

Requires Windows 10/11 (x64) and a Rust toolchain that supports edition 2024.

```powershell
cargo build --release
cargo test
```

The executable is written to `target\release\ProjectLauncher.exe`.

### Run

| Command | Description |
| --- | --- |
| `ProjectLauncher.exe` | Open the profile manager. |
| `ProjectLauncher.exe --settings` | Open the profile manager. |
| `ProjectLauncher.exe --profile <GUID>` | Run a profile. If resident mode is running, the request is sent to it. |
| `ProjectLauncher.exe --resident` | Start the resident process and tray icon. |
| `ProjectLauncher.exe --shortcut <GUID> [desktop|startmenu|both]` | Create a shortcut. The default location is the Desktop. |

### Configuration

The configuration file is `%LOCALAPPDATA%\ProjectLauncher\config.json`. Edit profiles in the profile manager, or see [`config.example.json`](config.example.json) for the JSON format.

Actions run in order. Starting an application does not wait for it to close; use a wait action when a fixed delay is needed. Commands run as written under your Windows account, so only run profiles you trust.

Shortcuts and the Windows startup entry point to the current executable path. Recreate them if you move the executable.

## 日本語

StillLaunch は、保存したプロファイルから複数のアプリや操作をまとめて実行する Windows 向けランチャーです。

### 主な機能

- プロファイルの作成・編集・実行を管理画面から行えます。
- アプリ、フォルダ、ファイル、URLを開き、`cmd` や PowerShell のコマンドを実行できます。操作の間に待ち時間を入れることもできます。
- プロファイルごとにデスクトップまたはスタートメニューへショートカットを作成できます。
- 常駐モードとタスクトレイに対応しています。Windows 起動時に常駐させる設定もあります。

### ビルド

Windows 10/11（x64）と、Rust 2024 Edition に対応したツールチェーンが必要です。

```powershell
cargo build --release
cargo test
```

実行ファイルは `target\release\ProjectLauncher.exe` に作成されます。

### 起動方法

| コマンド | 動作 |
| --- | --- |
| `ProjectLauncher.exe` | プロファイル管理画面を開きます。 |
| `ProjectLauncher.exe --settings` | プロファイル管理画面を開きます。 |
| `ProjectLauncher.exe --profile <GUID>` | 指定したプロファイルを実行します。常駐中は常駐プロセスへ要求を送ります。 |
| `ProjectLauncher.exe --resident` | 常駐プロセスとタスクトレイアイコンを起動します。 |
| `ProjectLauncher.exe --shortcut <GUID> [desktop|startmenu|both]` | ショートカットを作成します。省略時はデスクトップに作成します。 |

### 設定ファイル

設定ファイルは `%LOCALAPPDATA%\ProjectLauncher\config.json` です。プロファイル管理画面から編集できます。JSON形式は [`config.example.json`](config.example.json) を参照してください。

アクションは記載順に実行されます。アプリの起動後、そのアプリが終了するまで待つことはありません。一定時間待つ場合は wait アクションを使ってください。コマンドは Windows のユーザー権限で設定どおりに実行されるため、信頼できるプロファイルだけを実行してください。

ショートカットと Windows 起動時の登録には、実行ファイルの現在のパスが使われます。実行ファイルを移動した場合は作り直してください。

## License / ライセンス

Released under the MIT License. MITライセンスで公開しています。
