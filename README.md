# clipdmenu
> A clipboard manager for X11, built in Rust.

- **`clipdmenud`** - Daemon to cache copied image and `CLIPBOARD` selection via XFIXES. (supports large payloads via the ICCCM `INCR` protocol).
- **`clipdmenu`** - Lists cached history using `CM_LAUNCHER`, defaults to `dmenu`.

## Build & Run

```sh
cargo build --release
```

No required runtime dependencies except [`x11rb`](https://github.com/psychon/x11rb).

Run the daemon persistently in the background by adding it to WM/session autostart.

```sh
clipdmenud &
```

we suggest to bind `clipdmenu` to a hotkey in your WM or using [`sxhkd`](https://github.com/baskerville/sxhkd).

Any `dmenu` (or `CM_LAUNCHER`) flags work with `clipdmenu` since they're forwarded as-is.

## Configuration

### `clipdmenu` ENV vars
| Variable              | Description                                                  |
|-----------------------|--------------------------------------------------------------|
| `CM_LAUNCHER`         | Menu command (default `dmenu`), e.g. `rofi -dmenu` for rofi  |
| `CLIPDMENU_CACHE_DIR` | Cache dir, default `[$XDG_CACHE_HOME or ~/.cache]/clipdmenu` |
| `XDG_RUNTIME_DIR`     | IPC socket dir, for daemon <=> client communication          |

### `clipdmenud` (daemon) flags
| Option Flag       | Description                                         |
|-------------------|-----------------------------------------------------|
| `max-items=<num>` | Max number of enteries to store, defualt 200        |
| `watch-primary`   | Capture mouse-selection / `PRIMARY`, off by default |

## Limitations

- X11 only (no Wayland)
- One image slot (by design)
- No encryption, clipboard history is plain files under `~/.cache`, same
  as other clipboard menus. Treat it accordingly if you copy secrets.
