# X11 & Wayland Clipboard Bridge

[简体中文](README.zh_CN.md)

`clip-bridge` synchronizes clipboard contents between X11 and Wayland
environments. It is designed for mixed sessions where native Wayland clients
and X11/XWayland clients need to share clipboard data reliably.

## Features

- **Bidirectional clipboard sync** between X11 and Wayland clipboard selections.
- **Modern UTF-8 text support** using `text/plain;charset=utf-8`, `text/plain`,
  and `UTF8_STRING`.
- **Image clipboard support** for `image/png`, `image/jpeg`, and `image/jpg`.
- **Primary selection support** for X11 Primary to Wayland Primary mirroring.
- **Content deduplication** for text and binary data. Image data is compared by
  hash instead of repeatedly comparing full byte buffers.
- **Event-driven runtime loops** for X11 and Wayland. The main event loops block
  on protocol file descriptors and internal wake pipes instead of polling on a
  fixed timer.

## Scope and Limitations

- The regular Clipboard selection is synchronized in both directions.
- X11 Primary selection changes can be mirrored to Wayland Primary selection.
  Wayland Primary selection input is currently not read back into X11.
- Legacy X11 text targets such as `STRING` and `TEXT` are intentionally not
  advertised. The bridge focuses on modern UTF-8 text targets.
- Image data is transferred as-is. The bridge does not transcode image formats.
  For example, `image/jpeg` remains `image/jpeg`.
- X11 `INCR` transfers are handled for large clipboard payloads.

## Build and Run

### Prerequisites

- Rust 1.88.0 or higher
- Development libraries for X11 and Wayland
- `xclip` for X11-side manual testing
- `wl-clipboard` for Wayland-side manual testing

### Build

```bash
cargo build --release
```

### Run

```bash
cargo run
```

The default log level is `info`. Use `RUST_LOG` to change it:

```bash
RUST_LOG=debug cargo run
RUST_LOG=error cargo run
```

## Manual Testing

Start the bridge:

```bash
cargo run
```

### Text

Set text from X11:

```bash
echo "X11 text $(date)" | xclip -selection clipboard
```

Read it from Wayland:

```bash
wl-paste
```

Set text from Wayland:

```bash
echo "Wayland text $(date)" | wl-copy
```

Read it from X11:

```bash
xclip -selection clipboard -o
```

### Images

Set a PNG image from X11:

```bash
xclip -selection clipboard -t image/png -i image.png
```

Read it from Wayland:

```bash
wl-paste -t image/png > out.png
```

Set a PNG image from Wayland:

```bash
wl-copy -t image/png < image.png
```

Read it from X11:

```bash
xclip -selection clipboard -t image/png -o > out.png
```

### Primary Selection

Set X11 Primary text:

```bash
echo "Primary selection $(date)" | xclip -selection primary
```

The bridge can mirror this to Wayland Primary selection when the compositor
supports the data-control primary-selection path.

## How It Works

### X11 Side

- Creates a hidden X11 window to own and serve selections.
- Uses XFixes selection notifications to detect owner changes for Clipboard and
  Primary selections.
- Requests the selection owner's `TARGETS`, then chooses the best supported MIME
  or X11 target by preference.
- Supports modern UTF-8 text and image targets.
- Uses `poll()` on the X11 connection fd plus an internal wake pipe, so the main
  X11 loop sleeps when there is no work.

### Wayland Side

- Uses the `zwlr_data_control_v1` protocol to monitor and set clipboard content.
- Records offered MIME types and requests the best supported type.
- Offers only the MIME types represented by the current clipboard content.
- Uses `EventQueue::prepare_read()` with `poll()` on the Wayland fd plus an
  internal wake pipe, so the main Wayland loop sleeps when there is no work.

### Synchronization Logic

- Text is stored as compact UTF-8 strings.
- Binary data is stored as shared byte buffers with an `xxh3` hash for cheaper
  deduplication.
- Sync events are forwarded through a central task to avoid immediate feedback
  loops.
- Clipboard-setting requests wake the relevant protocol thread through a pipe,
  avoiding timer-based polling.

## Supported Formats

### Text

- `text/plain;charset=utf-8`
- `text/plain`
- `UTF8_STRING`

### Images

- `image/png`
- `image/jpeg`
- `image/jpg`

## Troubleshooting

### Build Failures

Ensure the X11 and Wayland development libraries are installed. On Arch-based
systems, the runtime package metadata expects:

```text
wayland
wayland-protocols
libx11
libxkbcommon
libxkbcommon-x11
```

### No Clipboard Sync

- Confirm both X11 and Wayland connections are available in the current session.
- Run with `RUST_LOG=debug` and check whether XFixes or Wayland data-control
  events are received.
- Verify clipboard tools directly with `xclip`, `wl-copy`, and `wl-paste`.

### Images Do Not Paste

- Check the offered types:

  ```bash
  wl-paste --list-types
  xclip -selection clipboard -t TARGETS -o
  ```

- Ensure the copied image is offered as `image/png`, `image/jpeg`, or
  `image/jpg`.
- The bridge does not convert between image formats.

### Unexpected CPU Usage

The main X11 and Wayland loops are event-driven. If CPU usage is still high,
run with `RUST_LOG=info` or `RUST_LOG=error` first, then inspect debug logs for a
source application repeatedly changing clipboard ownership.

## Technical Details

### Dependencies

- `x11rb`: X11 bindings
- `wayland-client`: Wayland client library
- `wayland-protocols`: Wayland protocol definitions
- `wayland-protocols-wlr`: `zwlr_data_control_v1` protocol bindings
- `tokio`: asynchronous runtime
- `tracing`: logging
- `compact_str`: compact UTF-8 text storage
- `xxhash-rust`: binary content hashing

### Protocol Support

- X11 Clipboard selection
- X11 Primary selection monitoring
- X11 `TARGETS`, `MULTIPLE`, and `INCR`
- Wayland `zwlr_data_control_v1`
- Modern UTF-8 text targets
- PNG and JPEG image MIME types

## License

This project is licensed under the MIT License.

## Contribution

Contributions are welcome. Please open issues or submit pull requests.
