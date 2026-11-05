# WinShot X

A small screenshot and screen recording tool for Windows that runs in the system tray. Written in Rust using the Win32 API directly.

Zero dependencies aside from [windows-rs](https://github.com/microsoft/windows-rs) for the Win32 bindings, plus [embedinator](https://github.com/sidit77/embedinator) at build time to embed the logo as the exe icon.

Inspired by [CleanShot X](https://cleanshot.com), easily the best macOS screenshot app out there. I am a daily user of it. If you're on a Mac, buy it. This is my attempt to get some of that on Windows.

## Usage

| Hotkey        | Action                                         |
| ------------- | ---------------------------------------------- |
| `Alt+Shift+4` | Capture a region                               |
| `Alt+Shift+5` | Open the capture menu (or stop recording)      |

Right-click the tray icon to toggle **Start with Windows** or quit.

## Building

```sh
make build   # release build -> target/release/winshotx.exe
make run     # debug build with a console (Ctrl+C to quit)
```

Or use `cargo build --release` directly.

## Status

This project is provided as-is. Issues and pull requests are not accepted.

## License

[MIT](LICENSE)
