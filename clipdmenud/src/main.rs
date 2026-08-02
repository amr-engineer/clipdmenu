//! clipdmenud - X11 clipboard-history daemon.
//!
//! Watches the CLIPBOARD selection (and optionally PRIMARY) via the XFIXES
//! extension. On every change it fetches the content, and:
//!   - if a text target is offered, stores it (deduped by hash) in the
//!     rolling history (`~/.cache/clipdmenu/index.tsv` + `entries/`)
//!   - otherwise, if an image/* target is offered, overwrites the single
//!     "last image" cache slot
//!
//! It also listens on a Unix socket for "SELECT <id>" commands (sent by the
//! `clipdmenu` client after a dmenu pick), becomes the CLIPBOARD owner, and
//! serves the stored bytes back to whichever application pastes next -
//! including large images, via the ICCCM INCR protocol.

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use x11rb::connection::Connection;
use x11rb::protocol::xfixes::ConnectionExt as _;
use x11rb::protocol::xproto::{
    AtomEnum, ChangeWindowAttributesAux, ConnectionExt as _, CreateWindowAux, EventMask, PropMode, Property,
    SelectionNotifyEvent, SelectionRequestEvent, WindowClass, SELECTION_NOTIFY_EVENT,
};
use x11rb::protocol::{xfixes, Event};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt;

type Atom = u32;
type Window = u32;
type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

const CURRENT_TIME: u32 = 0;
/// Threshold (and per-chunk size) for switching a served selection to the
/// ICCCM INCR protocol. Kept well under typical X server request limits.
const INCR_CHUNK: usize = 200_000;

struct Config {
    max_items: usize,
    watch_primary: bool,
}

struct Atoms {
    clipboard: Atom,
    primary: Atom,
    targets: Atom,
    incr: Atom,
    utf8_string: Atom,
    string: Atom,
    text: Atom,
    transfer_prop: Atom,
}

struct OwnedSelection {
    mime_atom: Atom,
    data: Vec<u8>,
    is_text: bool,
}

struct IncrSend {
    requestor: Window,
    property: Atom,
    target: Atom,
    data: Vec<u8>,
    offset: usize,
}

fn main() {
    if let Err(e) = run() {
        eprintln!("clipdmenud: fatal: {e}");
        std::process::exit(1);
    }
}

fn parse_args() -> Config {
    let mut cfg = Config {
        max_items: clipdmenu_common::DEFAULT_MAX_ITEMS,
        watch_primary: false,
    };
    for arg in std::env::args().skip(1) {
        if let Some(v) = arg.strip_prefix("--max-items=") {
            if let Ok(n) = v.parse::<usize>() {
                cfg.max_items = n;
            }
        } else if arg == "--watch-primary" {
            cfg.watch_primary = true;
        } else if arg == "-h" || arg == "--help" {
            println!(
                "clipdmenud [--max-items=N] [--watch-primary]\n\n\
                 --max-items=N     keep at most N text entries (default {})\n\
                 --watch-primary   also capture the PRIMARY (mouse-select) selection",
                clipdmenu_common::DEFAULT_MAX_ITEMS
            );
            std::process::exit(0);
        }
    }
    cfg
}

fn run() -> Result<()> {
    let cfg = parse_args();
    clipdmenu_common::ensure_dirs()?;

    let (conn, screen_num) = x11rb::connect(None)?;
    let screen = conn.setup().roots[screen_num].clone();

    let window = conn.generate_id()?;
    conn.create_window(
        x11rb::COPY_DEPTH_FROM_PARENT,
        window,
        screen.root,
        0,
        0,
        1,
        1,
        0,
        WindowClass::INPUT_OUTPUT,
        screen.root_visual,
        &CreateWindowAux::new().event_mask(EventMask::PROPERTY_CHANGE),
    )?
    .check()?;

    conn.xfixes_query_version(5, 0)?.reply()?;

    let atoms = Atoms {
        clipboard: intern(&conn, "CLIPBOARD")?,
        primary: AtomEnum::PRIMARY.into(),
        targets: intern(&conn, "TARGETS")?,
        incr: intern(&conn, "INCR")?,
        utf8_string: intern(&conn, "UTF8_STRING")?,
        string: AtomEnum::STRING.into(),
        text: intern(&conn, "TEXT")?,
        transfer_prop: intern(&conn, "CLIPDMENU_TRANSFER")?,
    };

    let mask = xfixes::SelectionEventMask::SET_SELECTION_OWNER
        | xfixes::SelectionEventMask::SELECTION_WINDOW_DESTROY
        | xfixes::SelectionEventMask::SELECTION_CLIENT_CLOSE;
    conn.xfixes_select_selection_input(window, atoms.clipboard, mask)?;
    if cfg.watch_primary {
        conn.xfixes_select_selection_input(window, atoms.primary, mask)?;
    }
    conn.flush()?;

    eprintln!(
        "clipdmenud: watching {}(cache: {})",
        if cfg.watch_primary {
            "CLIPBOARD + PRIMARY "
        } else {
            "CLIPBOARD "
        },
        clipdmenu_common::cache_dir().display()
    );

    let (ipc_tx, ipc_rx) = mpsc::channel::<(String, mpsc::Sender<String>)>();
    spawn_ipc_thread(ipc_tx)?;

    let mut owned: Option<OwnedSelection> = None;
    let mut pending: Vec<IncrSend> = Vec::new();

    loop {
        while let Some(event) = conn.poll_for_event()? {
            match event {
                Event::XfixesSelectionNotify(ev) => {
                    if ev.owner != 0
                        && ev.owner != window
                        && (ev.selection == atoms.clipboard || (cfg.watch_primary && ev.selection == atoms.primary))
                    {
                        if let Err(e) = capture_selection(&conn, window, ev.selection, &atoms, cfg.max_items) {
                            eprintln!("clipdmenud: capture failed: {e}");
                        }
                    }
                }
                Event::SelectionRequest(ev) => {
                    if let Err(e) = handle_selection_request(&conn, &ev, &owned, &atoms, &mut pending) {
                        eprintln!("clipdmenud: selection request failed: {e}");
                    }
                }
                Event::SelectionClear(ev) => {
                    if ev.selection == atoms.clipboard {
                        owned = None;
                    }
                }
                Event::PropertyNotify(pev) if pev.state == Property::DELETE => {
                    advance_incr_send(&conn, pev.window, pev.atom, &mut pending)?;
                }
                _ => {}
            }
        }

        if let Ok((cmd, resp_tx)) = ipc_rx.try_recv() {
            let resp = if let Some(id) = cmd.strip_prefix("SELECT ") {
                match handle_select(&conn, window, &atoms, id.trim()) {
                    Ok(sel) => {
                        owned = Some(sel);
                        "OK".to_string()
                    }
                    Err(e) => format!("ERR {e}"),
                }
            } else if cmd == "PING" {
                "PONG".to_string()
            } else {
                "ERR unknown command".to_string()
            };
            let _ = resp_tx.send(resp);
        }

        conn.flush()?;
        thread::sleep(Duration::from_millis(25));
    }
}

fn spawn_ipc_thread(tx: mpsc::Sender<(String, mpsc::Sender<String>)>) -> Result<()> {
    let path = clipdmenu_common::socket_path();
    let _ = fs::remove_file(&path);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let listener = UnixListener::bind(&path)?;

    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let tx = tx.clone();
            thread::spawn(move || {
                let mut reader = BufReader::new(match stream.try_clone() {
                    Ok(s) => s,
                    Err(_) => return,
                });
                let mut line = String::new();
                if reader.read_line(&mut line).is_ok() && !line.trim().is_empty() {
                    let (resp_tx, resp_rx) = mpsc::channel();
                    if tx.send((line.trim().to_string(), resp_tx)).is_ok() {
                        if let Ok(resp) = resp_rx.recv_timeout(Duration::from_secs(2)) {
                            let mut stream = stream;
                            let _ = writeln!(stream, "{resp}");
                        }
                    }
                }
            });
        }
    });

    Ok(())
}

fn handle_select(conn: &RustConnection, window: Window, atoms: &Atoms, id: &str) -> Result<OwnedSelection> {
    let (mime, data) = if id == clipdmenu_common::LAST_IMAGE_ID {
        clipdmenu_common::read_last_image().ok_or("no image cached yet")?
    } else {
        let data = clipdmenu_common::read_entry_data(id).map_err(|_| "history entry not found")?;
        let mime = clipdmenu_common::entry_mime(id).unwrap_or_else(|| "text/plain".to_string());
        (mime, data)
    };

    let mime_atom = intern(conn, &mime)?;
    conn.set_selection_owner(window, atoms.clipboard, CURRENT_TIME)?;
    conn.flush()?;

    Ok(OwnedSelection {
        mime_atom,
        is_text: mime.starts_with("text/"),
        data,
    })
}

fn capture_selection(
    conn: &RustConnection,
    window: Window,
    selection: Atom,
    atoms: &Atoms,
    max_items: usize,
) -> Result<()> {
    // Give the new owner a moment to settle before we start requesting targets.
    thread::sleep(Duration::from_millis(50));

    let targets = get_targets(conn, window, selection, atoms)?;
    if targets.is_empty() {
        return Ok(());
    }

    let cookies: std::result::Result<Vec<_>, _> = targets.iter().map(|&a| conn.get_atom_name(a)).collect();
    let cookies = cookies?;

    let mut text_atom: Option<Atom> = None;
    let mut text_priority = u8::MAX;
    let mut image_candidates: Vec<(Atom, String)> = Vec::new();

    for (atom, cookie) in targets.iter().zip(cookies) {
        let reply = cookie.reply()?;
        let name = String::from_utf8_lossy(&reply.name).to_string();
        let priority = match name.as_str() {
            "UTF8_STRING" => 0,
            "STRING" => 1,
            "TEXT" => 2,
            _ => u8::MAX,
        };
        if priority < text_priority {
            text_priority = priority;
            text_atom = Some(*atom);
        }
        if name.starts_with("image/") {
            image_candidates.push((*atom, name));
        }
    }

    if let Some(text_atom) = text_atom {
        if let Some((data, _)) = fetch_selection(conn, window, selection, text_atom, atoms.transfer_prop, atoms.incr)? {
            if let Ok(text) = String::from_utf8(data) {
                if !text.trim().is_empty() {
                    let hash = clipdmenu_common::hash_bytes(text.as_bytes());
                    let preview = clipdmenu_common::make_text_preview(&text);
                    clipdmenu_common::upsert_entry(&hash, "text/plain", &preview, text.as_bytes(), max_items)?;
                }
            }
        }
        return Ok(());
    }

    let chosen_image = image_candidates
        .iter()
        .find(|(_, name)| name == "image/png")
        .or_else(|| image_candidates.first());

    if let Some((image_atom, mime)) = chosen_image {
        if let Some((data, _)) = fetch_selection(conn, window, selection, *image_atom, atoms.transfer_prop, atoms.incr)?
        {
            if !data.is_empty() {
                clipdmenu_common::save_last_image(mime, &data)?;
            }
        }
    }

    Ok(())
}

fn get_targets(conn: &RustConnection, window: Window, selection: Atom, atoms: &Atoms) -> Result<Vec<Atom>> {
    match fetch_selection(conn, window, selection, atoms.targets, atoms.transfer_prop, atoms.incr)? {
        Some((data, _)) => Ok(data
            .chunks_exact(4)
            .map(|c| u32::from_ne_bytes([c[0], c[1], c[2], c[3]]))
            .collect()),
        Option::None => Ok(Vec::new()),
    }
}

/// Requests `target` from `selection`, waits for the reply, and reassembles
/// it (transparently handling the ICCCM INCR protocol for large payloads).
/// Returns `(bytes, actual_type_atom)`, or `None` if the owner refused / timed out.
fn fetch_selection(
    conn: &RustConnection,
    window: Window,
    selection: Atom,
    target: Atom,
    property: Atom,
    incr_atom: Atom,
) -> Result<Option<(Vec<u8>, Atom)>> {
    conn.delete_property(window, property)?;
    conn.convert_selection(window, selection, target, property, CURRENT_TIME)?;
    conn.flush()?;

    let notify = match wait_for_selection_notify(conn, window, selection, target, Duration::from_secs(2))? {
        Some(ev) => ev,
        Option::None => return Ok(None),
    };
    if notify.property == 0 {
        return Ok(None); // owner refused this target
    }

    let first = conn
        .get_property(false, window, property, AtomEnum::ANY, 0, 0x1FFF_FFFF)?
        .reply()?;

    if first.type_ == incr_atom {
        let mut data = Vec::new();
        let mut actual_type = target;
        conn.delete_property(window, property)?;
        conn.flush()?;
        loop {
            if wait_for_property_new_value(conn, window, property, Duration::from_secs(5))?.is_none() {
                return Ok(None);
            }
            let chunk = conn
                .get_property(true, window, property, AtomEnum::ANY, 0, 0x1FFF_FFFF)?
                .reply()?;
            if chunk.value.is_empty() {
                break;
            }
            actual_type = chunk.type_;
            data.extend_from_slice(&chunk.value);
        }
        Ok(Some((data, actual_type)))
    } else {
        let mut data = first.value.clone();
        let mut bytes_after = first.bytes_after;
        while bytes_after > 0 {
            let more = conn
                .get_property(
                    false,
                    window,
                    property,
                    AtomEnum::ANY,
                    (data.len() / 4) as u32,
                    0x1FFF_FFFF,
                )?
                .reply()?;
            data.extend_from_slice(&more.value);
            bytes_after = more.bytes_after;
        }
        conn.delete_property(window, property)?;
        Ok(Some((data, first.type_)))
    }
}

fn wait_for_selection_notify(
    conn: &RustConnection,
    window: Window,
    selection: Atom,
    target: Atom,
    timeout: Duration,
) -> Result<Option<SelectionNotifyEvent>> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(event) = conn.poll_for_event()? {
            if let Event::SelectionNotify(ev) = event {
                if ev.requestor == window && ev.selection == selection && ev.target == target {
                    return Ok(Some(ev));
                }
            }
            // Other events (e.g. a concurrent SelectionRequest) are dropped here;
            // this synchronous fetch only runs during our own capture/serve calls.
        } else if Instant::now() > deadline {
            return Ok(None);
        } else {
            thread::sleep(Duration::from_millis(5));
        }
    }
}

fn wait_for_property_new_value(
    conn: &RustConnection,
    window: Window,
    property: Atom,
    timeout: Duration,
) -> Result<Option<()>> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(Event::PropertyNotify(pev)) = conn.poll_for_event()? {
            if pev.window == window && pev.atom == property && pev.state == Property::NEW_VALUE {
                return Ok(Some(()));
            }
        } else if Instant::now() > deadline {
            return Ok(None);
        } else {
            thread::sleep(Duration::from_millis(5));
        }
    }
}

fn handle_selection_request(
    conn: &RustConnection,
    ev: &SelectionRequestEvent,
    owned: &Option<OwnedSelection>,
    atoms: &Atoms,
    pending: &mut Vec<IncrSend>,
) -> Result<()> {
    let property = if ev.property == 0 { ev.target } else { ev.property };

    let owned = match owned {
        Some(o) => o,
        Option::None => return send_notify(conn, ev, 0),
    };

    if ev.target == atoms.targets {
        let mut list = vec![atoms.targets, owned.mime_atom];
        if owned.is_text {
            list.push(atoms.utf8_string);
            list.push(atoms.string);
            list.push(atoms.text);
        }
        conn.change_property32(PropMode::REPLACE, ev.requestor, property, AtomEnum::ATOM, &list)?;
        return send_notify(conn, ev, property);
    }

    let matches_target = ev.target == owned.mime_atom
        || (owned.is_text && (ev.target == atoms.utf8_string || ev.target == atoms.string || ev.target == atoms.text));

    if !matches_target {
        return send_notify(conn, ev, 0);
    }

    if owned.data.len() > INCR_CHUNK {
        conn.change_window_attributes(
            ev.requestor,
            &ChangeWindowAttributesAux::new().event_mask(EventMask::PROPERTY_CHANGE),
        )?;
        conn.change_property32(
            PropMode::REPLACE,
            ev.requestor,
            property,
            atoms.incr,
            &[owned.data.len() as u32],
        )?;
        pending.push(IncrSend {
            requestor: ev.requestor,
            property,
            target: ev.target,
            data: owned.data.clone(),
            offset: 0,
        });
    } else {
        conn.change_property8(PropMode::REPLACE, ev.requestor, property, ev.target, &owned.data)?;
    }

    send_notify(conn, ev, property)
}

fn advance_incr_send(
    conn: &RustConnection,
    requestor: Window,
    property: Atom,
    pending: &mut Vec<IncrSend>,
) -> Result<()> {
    let idx = match pending
        .iter()
        .position(|p| p.requestor == requestor && p.property == property)
    {
        Some(i) => i,
        Option::None => return Ok(()),
    };

    let take;
    {
        let p = &pending[idx];
        take = (p.data.len() - p.offset).min(INCR_CHUNK);
    }
    let chunk: Vec<u8> = {
        let p = &pending[idx];
        p.data[p.offset..p.offset + take].to_vec()
    };
    let target = pending[idx].target;
    conn.change_property8(PropMode::REPLACE, requestor, property, target, &chunk)?;
    pending[idx].offset += take;
    conn.flush()?;

    if take == 0 {
        pending.remove(idx);
    }
    Ok(())
}

fn send_notify(conn: &RustConnection, ev: &SelectionRequestEvent, property: Atom) -> Result<()> {
    let notify = SelectionNotifyEvent {
        response_type: SELECTION_NOTIFY_EVENT,
        sequence: 0,
        time: ev.time,
        requestor: ev.requestor,
        selection: ev.selection,
        target: ev.target,
        property,
    };
    conn.send_event(false, ev.requestor, EventMask::NO_EVENT, notify)?;
    conn.flush()?;
    Ok(())
}

fn intern(conn: &RustConnection, name: &str) -> Result<Atom> {
    Ok(conn.intern_atom(false, name.as_bytes())?.reply()?.atom)
}
