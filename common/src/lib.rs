//! Shared state for clipdmenu: cache layout, history index, and small
//! dependency-free helpers (hashing, previews, PNG dimension sniffing).
//!
//! Layout under `cache_dir()`:
//!   index.tsv        - rolling text history, most recent first: "<hash>\t<mime>\t<preview>"
//!   entries/<hash>    - raw bytes for each text history entry
//!   last_image.bin    - raw bytes of the most recently copied image (single slot)
//!   last_image.meta   - mime type of last_image.bin (e.g. "image/png")

use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;

pub const DEFAULT_MAX_ITEMS: usize = 200;
pub const PREVIEW_MAX_CHARS: usize = 120;
pub const LAST_IMAGE_ID: &str = "last_image";

pub fn cache_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("CLIPDMENU_CACHE_DIR") {
        return PathBuf::from(dir);
    }
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let home = std::env::var_os("HOME").expect("HOME is not set");
            PathBuf::from(home).join(".cache")
        });
    base.join("clipdmenu")
}

pub fn entries_dir() -> PathBuf {
    cache_dir().join("entries")
}

pub fn index_path() -> PathBuf {
    cache_dir().join("index.tsv")
}

pub fn last_image_data_path() -> PathBuf {
    cache_dir().join("last_image.bin")
}

pub fn last_image_meta_path() -> PathBuf {
    cache_dir().join("last_image.meta")
}

/// Socket used for client -> daemon IPC ("SELECT <id>").
pub fn socket_path() -> PathBuf {
    if let Ok(dir) = std::env::var("XDG_RUNTIME_DIR") {
        PathBuf::from(dir).join("clipdmenu.sock")
    } else {
        cache_dir().join("clipdmenu.sock")
    }
}

pub fn ensure_dirs() -> io::Result<()> {
    fs::create_dir_all(entries_dir())
}

#[derive(Debug, Clone)]
pub struct Entry {
    pub hash: String,
    pub mime: String,
    pub preview: String,
}

impl Entry {
    pub fn to_line(&self) -> String {
        format!("{}\t{}\t{}", self.hash, self.mime, self.preview)
    }

    pub fn from_line(line: &str) -> Option<Entry> {
        let mut parts = line.splitn(3, '\t');
        let hash = parts.next()?.to_string();
        let mime = parts.next()?.to_string();
        let preview = parts.next().unwrap_or("").to_string();
        if hash.is_empty() {
            return None;
        }
        Some(Entry { hash, mime, preview })
    }
}

pub fn read_index() -> Vec<Entry> {
    let content = match fs::read_to_string(index_path()) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    content.lines().filter_map(Entry::from_line).collect()
}

fn write_index(entries: &[Entry]) -> io::Result<()> {
    ensure_dirs()?;
    let tmp = cache_dir().join("index.tsv.tmp");
    {
        let mut f = fs::File::create(&tmp)?;
        for e in entries {
            writeln!(f, "{}", e.to_line())?;
        }
    }
    fs::rename(tmp, index_path())
}

pub fn upsert_entry(hash: &str, mime: &str, preview: &str, data: &[u8], max_items: usize) -> io::Result<()> {
    ensure_dirs()?;

    let entry_file = entries_dir().join(hash);
    if !entry_file.exists() {
        fs::write(&entry_file, data)?;
    }

    let mut entries = read_index();
    entries.retain(|e| e.hash != hash);
    entries.insert(
        0,
        Entry {
            hash: hash.to_string(),
            mime: mime.to_string(),
            preview: preview.to_string(),
        },
    );

    while entries.len() > max_items {
        if let Some(removed) = entries.pop() {
            let _ = fs::remove_file(entries_dir().join(&removed.hash));
        }
    }

    write_index(&entries)
}

pub fn read_entry_data(hash: &str) -> io::Result<Vec<u8>> {
    fs::read(entries_dir().join(hash))
}

pub fn entry_mime(hash: &str) -> Option<String> {
    read_index().into_iter().find(|e| e.hash == hash).map(|e| e.mime)
}

pub fn save_last_image(mime: &str, data: &[u8]) -> io::Result<()> {
    ensure_dirs()?;
    fs::write(last_image_data_path(), data)?;
    fs::write(last_image_meta_path(), mime)?;
    Ok(())
}

pub fn read_last_image() -> Option<(String, Vec<u8>)> {
    let mime = fs::read_to_string(last_image_meta_path()).ok()?;
    let data = fs::read(last_image_data_path()).ok()?;
    Some((mime.trim().to_string(), data))
}

pub fn last_image_preview() -> Option<String> {
    let (mime, data) = read_last_image()?;
    Some(make_image_preview(&mime, &data))
}

/// Hashing (two seeded SipHash passes -> 128 bits)
pub fn hash_bytes(data: &[u8]) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let mut h1 = DefaultHasher::new();
    data.hash(&mut h1);
    let a = h1.finish();

    let mut h2 = DefaultHasher::new();
    0x9e3779b97f4a7c15u64.hash(&mut h2);
    data.hash(&mut h2);
    let b = h2.finish();

    format!("{:016x}{:016x}", a, b)
}

pub fn make_text_preview(text: &str) -> String {
    let line_count = text.lines().count().max(1);
    let flattened: String = text
        .chars()
        .map(|c| if c == '\n' || c == '\t' || c == '\r' { ' ' } else { c })
        .collect();
    let trimmed = flattened.trim();

    let char_count = trimmed.chars().count();
    let mut preview: String = trimmed.chars().take(PREVIEW_MAX_CHARS).collect();
    if char_count > PREVIEW_MAX_CHARS {
        preview.push('\u{2026}'); // …
    }

    if line_count > 1 {
        format!("{}  [{} lines]", preview, line_count)
    } else {
        preview
    }
}

pub fn make_image_preview(mime: &str, data: &[u8]) -> String {
    let dims = png_dimensions(data)
        .map(|(w, h)| format!("{}x{}", w, h))
        .unwrap_or_else(|| "?x?".to_string());
    format!("[image]  {}  {}  {}", mime, dims, human_size(data.len()))
}

pub fn human_size(bytes: usize) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut size = bytes as f64;
    let mut unit = 0usize;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{}{}", bytes, UNITS[0])
    } else {
        format!("{:.1}{}", size, UNITS[unit])
    }
}

/// Reads width/height straight out of a PNG's IHDR chunk. Returns None for
/// non-PNG data (e.g. image/bmp, image/tiff) - the preview just omits size.
pub fn png_dimensions(data: &[u8]) -> Option<(u32, u32)> {
    const SIG: &[u8; 8] = b"\x89PNG\r\n\x1a\n";

    if data.len() < 24 || data.get(0..8)? != SIG {
        return None;
    }
    if data.get(12..16)? != b"IHDR" {
        return None;
    }

    let width = u32::from_be_bytes(data.get(16..20)?.try_into().ok()?);
    let height = u32::from_be_bytes(data.get(20..24)?.try_into().ok()?);

    Some((width, height))
}
