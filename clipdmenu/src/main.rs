//! clipdmenu - dmenu front-end for clipdmenu.
//!
//! Every argument given to this binary is forwarded verbatim to the
//! launcher (dmenu by default; override with `CM_LAUNCHER`, e.g. `rofi -dmenu`).
//! The picked entry's id is sent to `clipdmenud` over a Unix socket, which
//! becomes the CLIPBOARD owner and serves the content back.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::process::{Command, Stdio};

use clipdmenu_common as cm;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

fn main() {
    if let Err(e) = run() {
        eprintln!("clipdmenu: {e}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let dmenu_args: Vec<String> = std::env::args().skip(1).collect();
    let launcher = std::env::var("CM_LAUNCHER").unwrap_or_else(|_| "dmenu".to_string());

    let mut lines = Vec::new();
    if let Some(preview) = cm::last_image_preview() {
        lines.push(format!("{}\t{}", cm::LAST_IMAGE_ID, preview));
    }
    for entry in cm::read_index() {
        lines.push(format!("{}\t{}", entry.hash, entry.preview));
    }

    if lines.is_empty() {
        eprintln!("clipdmenu: no clipboard history yet (is clipdmenud running?)");
        return Ok(());
    }

    let mut child = Command::new(&launcher)
        .args(&dmenu_args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|e| format!("failed to launch '{launcher}': {e}"))?;

    child
        .stdin
        .as_mut()
        .ok_or("failed to open launcher stdin")?
        .write_all(lines.join("\n").as_bytes())?;

    let output = child.wait_with_output()?;
    let selection = String::from_utf8_lossy(&output.stdout);
    let selection = selection.trim_end_matches('\n');
    if selection.is_empty() {
        return Ok(());
    }

    let id = selection.split('\t').next().unwrap_or("");
    if id.is_empty() {
        return Ok(());
    }

    send_select(id)
}

fn send_select(id: &str) -> Result<()> {
    let path = cm::socket_path();
    let mut stream = UnixStream::connect(&path)
        .map_err(|e| format!("cannot reach clipdmenud at {}: {e} (is it running?)", path.display()))?;

    writeln!(stream, "SELECT {id}")?;
    stream.flush()?;

    let mut reader = BufReader::new(stream);
    let mut resp = String::new();
    reader.read_line(&mut resp)?;
    let resp = resp.trim();

    if let Some(msg) = resp.strip_prefix("ERR") {
        return Err(format!("clipdmenud reported an error:{msg}").into());
    }
    Ok(())
}
