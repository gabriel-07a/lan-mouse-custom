//! Copied files and folders on the clipboard.
//!
//! On this PC a [`Format::Files`] item is the list of copied paths, one per
//! line (UTF-8): what the file manager put on the clipboard (`text/uri-list`
//! / `x-special/gnome-copied-files` on Linux, `CF_HDROP` on Windows).
//! Between PCs it is a **bundle**: the files themselves, so the other PC can
//! paste them. [`FileBundling`] wraps a backend and converts at its edge:
//! reading packs the copied paths into a bundle, writing a bundle extracts
//! it under [`received_dir`] and puts the extracted paths on the clipboard.
//!
//! The bundle is deterministic (top-level order kept, folder contents sorted,
//! no timestamps), so re-packing what was just extracted yields the same
//! bytes: the sync layer recognises the clipboard echo of a write by hash.
//!
//! Bundle layout (all integers big-endian):
//!
//! ```text
//! "LMF1" | u32 entries | entries...
//! entry: u8 kind (0 file, 1 folder) | u16 path length | path (UTF-8,
//!        '/'-separated, relative) | for files: u64 size | bytes
//! ```
//!
//! A bundle comes from the network, so [`unpack`] trusts nothing: every path
//! component is checked (no `..`, no absolute paths or drive letters, no
//! names Windows refuses), sizes and counts are bounded, and everything is
//! written under a fresh folder.

use std::{
    io,
    path::{Component, Path, PathBuf},
};

use sha2::{Digest, Sha256};

use crate::{ClipboardSnapshot, Error, Format};

const MAGIC: &[u8; 4] = b"LMF1";
const KIND_FILE: u8 = 0;
const KIND_DIR: u8 = 1;
/// Most entries (files plus folders) one bundle may hold.
pub const MAX_ENTRIES: usize = 10_000;
/// Longest relative path accepted, in bytes.
const MAX_PATH: usize = 1024;
/// Deepest folder nesting accepted.
const MAX_DEPTH: usize = 32;

/// One entry of a bundle: a folder, or a file with its contents.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// relative, '/'-separated
    pub path: String,
    /// `None` for a folder
    pub data: Option<Vec<u8>>,
}

/// The copied paths of a local [`Format::Files`] item.
pub fn local_paths(item: &[u8]) -> Vec<PathBuf> {
    String::from_utf8_lossy(item)
        .lines()
        .map(str::trim_end)
        .filter(|line| !line.is_empty())
        .map(PathBuf::from)
        .collect()
}

/// A local [`Format::Files`] item for these paths.
pub fn local_item(paths: &[PathBuf]) -> Vec<u8> {
    let mut out = String::new();
    for path in paths {
        out.push_str(&path.to_string_lossy());
        out.push('\n');
    }
    out.into_bytes()
}

/// Paths of a `text/uri-list` (RFC 2483): `file://` URIs only, comments and
/// other schemes skipped.
pub fn paths_from_uri_list(bytes: &[u8]) -> Vec<PathBuf> {
    String::from_utf8_lossy(bytes)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .filter_map(path_from_file_uri)
        .collect()
}

/// Paths of GNOME's `x-special/gnome-copied-files` (`copy` or `cut`, then
/// one URI per line).
pub fn paths_from_gnome_copied(bytes: &[u8]) -> Vec<PathBuf> {
    let text = String::from_utf8_lossy(bytes);
    let mut lines = text.lines();
    match lines.next().map(str::trim) {
        Some("copy" | "cut") => {}
        _ => return Vec::new(),
    }
    lines
        .filter_map(|line| path_from_file_uri(line.trim()))
        .collect()
}

/// `text/uri-list` for these paths.
pub fn uri_list(paths: &[PathBuf]) -> Vec<u8> {
    let mut out = String::new();
    for path in paths {
        out.push_str(&file_uri(path));
        out.push_str("\r\n");
    }
    out.into_bytes()
}

/// `x-special/gnome-copied-files` for these paths (a copy, never a cut: the
/// files are ours, the user only pastes them).
pub fn gnome_copied(paths: &[PathBuf]) -> Vec<u8> {
    let mut out = String::from("copy");
    for path in paths {
        out.push('\n');
        out.push_str(&file_uri(path));
    }
    out.into_bytes()
}

fn path_from_file_uri(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    // `file:///home/x` (empty host) or `file://localhost/home/x`
    let rest = rest.strip_prefix("localhost").unwrap_or(rest);
    if !rest.starts_with('/') {
        return None;
    }
    let decoded = percent_decode(rest)?;
    Some(PathBuf::from(decoded))
}

fn file_uri(path: &Path) -> String {
    let mut out = String::from("file://");
    for byte in path.to_string_lossy().bytes() {
        if byte.is_ascii_alphanumeric() || b"/-._~".contains(&byte) {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

fn percent_decode(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = text.get(i + 1..i + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// Read the copied paths (files and whole folders) into a bundle of at most
/// `max_bytes` of file contents. Symbolic links are left out (they would
/// point at something on this PC); a copy over the limit is refused with
/// [`Error::TooLarge`].
pub fn pack(paths: &[PathBuf], max_bytes: usize) -> Result<Vec<u8>, Error> {
    let mut entries = Vec::new();
    let mut total = 0usize;
    let mut top_names = std::collections::HashSet::new();
    for path in paths {
        let Some(name) = path.file_name().map(|n| n.to_string_lossy().into_owned()) else {
            continue;
        };
        // two copied items with the same name from different folders
        let mut unique = name.clone();
        let mut n = 2;
        while !top_names.insert(unique.to_lowercase()) {
            unique = numbered(&name, n);
            n += 1;
        }
        collect(path, &unique, 0, &mut entries, &mut total, max_bytes)?;
    }
    if entries.is_empty() {
        return Err(Error::Backend("the copied files could not be read".into()));
    }
    Ok(encode(&entries))
}

/// "name (2).ext"
fn numbered(name: &str, n: usize) -> String {
    match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => format!("{stem} ({n}).{ext}"),
        _ => format!("{name} ({n})"),
    }
}

fn collect(
    path: &Path,
    relative: &str,
    depth: usize,
    entries: &mut Vec<Entry>,
    total: &mut usize,
    max_bytes: usize,
) -> Result<(), Error> {
    if depth > MAX_DEPTH {
        return Err(Error::Backend(
            "the copied folder is nested too deeply".into(),
        ));
    }
    let meta = std::fs::symlink_metadata(path)
        .map_err(|e| Error::Backend(format!("cannot read {}: {e}", path.display())))?;
    if meta.file_type().is_symlink() {
        log::info!("clipboard: leaving out the link {}", path.display());
        return Ok(());
    }
    if entries.len() >= MAX_ENTRIES {
        return Err(Error::TooLarge);
    }
    if meta.is_dir() {
        entries.push(Entry {
            path: relative.to_string(),
            data: None,
        });
        let mut children: Vec<_> = std::fs::read_dir(path)
            .map_err(|e| Error::Backend(format!("cannot list {}: {e}", path.display())))?
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        children.sort();
        for child in children {
            collect(
                &path.join(&child),
                &format!("{relative}/{child}"),
                depth + 1,
                entries,
                total,
                max_bytes,
            )?;
        }
    } else if meta.is_file() {
        *total = total.saturating_add(meta.len() as usize);
        if *total > max_bytes {
            return Err(Error::TooLarge);
        }
        let data = std::fs::read(path)
            .map_err(|e| Error::Backend(format!("cannot read {}: {e}", path.display())))?;
        entries.push(Entry {
            path: relative.to_string(),
            data: Some(data),
        });
    }
    Ok(())
}

pub fn encode(entries: &[Entry]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&(entries.len() as u32).to_be_bytes());
    for entry in entries {
        out.push(if entry.data.is_some() {
            KIND_FILE
        } else {
            KIND_DIR
        });
        out.extend_from_slice(&(entry.path.len() as u16).to_be_bytes());
        out.extend_from_slice(entry.path.as_bytes());
        if let Some(data) = &entry.data {
            out.extend_from_slice(&(data.len() as u64).to_be_bytes());
            out.extend_from_slice(data);
        }
    }
    out
}

/// Parse a bundle from a peer, checking every path (see the module docs).
pub fn decode(bundle: &[u8]) -> Result<Vec<Entry>, Error> {
    let bad = |what: &str| Error::Transfer(format!("bad file bundle: {what}"));
    let mut rest = bundle
        .strip_prefix(MAGIC.as_slice())
        .ok_or_else(|| bad("not a file bundle"))?;
    let count = u32::from_be_bytes(
        take(&mut rest, 4)
            .ok_or_else(|| bad("truncated"))?
            .try_into()
            .expect("4"),
    ) as usize;
    if count == 0 || count > MAX_ENTRIES {
        return Err(bad("entry count"));
    }
    let mut entries = Vec::with_capacity(count.min(1024));
    let mut seen = std::collections::HashSet::new();
    for _ in 0..count {
        let kind = *take(&mut rest, 1)
            .ok_or_else(|| bad("truncated"))?
            .first()
            .expect("1");
        let len = u16::from_be_bytes(
            take(&mut rest, 2)
                .ok_or_else(|| bad("truncated"))?
                .try_into()
                .expect("2"),
        ) as usize;
        let path = std::str::from_utf8(take(&mut rest, len).ok_or_else(|| bad("truncated"))?)
            .map_err(|_| bad("path is not UTF-8"))?;
        check_relative(path).map_err(|why| bad(&format!("unsafe path {path:?}: {why}")))?;
        // case-insensitive file systems would merge these
        if !seen.insert(path.to_lowercase()) {
            return Err(bad("duplicate path"));
        }
        let data = match kind {
            KIND_DIR => None,
            KIND_FILE => {
                let size = u64::from_be_bytes(
                    take(&mut rest, 8)
                        .ok_or_else(|| bad("truncated"))?
                        .try_into()
                        .expect("8"),
                );
                let size = usize::try_from(size).map_err(|_| bad("size"))?;
                Some(
                    take(&mut rest, size)
                        .ok_or_else(|| bad("truncated"))?
                        .to_vec(),
                )
            }
            _ => return Err(bad("entry kind")),
        };
        entries.push(Entry {
            path: path.to_string(),
            data,
        });
    }
    if !rest.is_empty() {
        return Err(bad("trailing bytes"));
    }
    // every parent folder of an entry must come before it
    let dirs: std::collections::HashSet<String> = entries
        .iter()
        .filter(|e| e.data.is_none())
        .map(|e| e.path.to_lowercase())
        .collect();
    for entry in &entries {
        if let Some((parent, _)) = entry.path.rsplit_once('/') {
            if !dirs.contains(&parent.to_lowercase()) {
                return Err(bad("an entry outside its folders"));
            }
        }
    }
    Ok(entries)
}

fn take<'a>(rest: &mut &'a [u8], n: usize) -> Option<&'a [u8]> {
    if rest.len() < n {
        return None;
    }
    let (head, tail) = rest.split_at(n);
    *rest = tail;
    Some(head)
}

/// Whether `path` is a safe relative path on Linux and Windows alike.
fn check_relative(path: &str) -> Result<(), &'static str> {
    if path.is_empty() || path.len() > MAX_PATH {
        return Err("length");
    }
    let parts: Vec<&str> = path.split('/').collect();
    if parts.len() > MAX_DEPTH + 1 {
        return Err("too deep");
    }
    for part in parts {
        if part.is_empty() || part == "." || part == ".." {
            return Err("empty, . or .. component");
        }
        if part
            .chars()
            .any(|c| c.is_control() || "\\:*?\"<>|".contains(c))
        {
            return Err("a character Windows or the path syntax refuses");
        }
        if part.ends_with('.') || part.ends_with(' ') {
            return Err("ends with a dot or space");
        }
        let stem = part.split('.').next().unwrap_or(part).to_ascii_uppercase();
        let reserved = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
            || (stem.len() == 4
                && (stem.starts_with("COM") || stem.starts_with("LPT"))
                && stem.as_bytes()[3].is_ascii_digit());
        if reserved {
            return Err("a reserved Windows name");
        }
    }
    // belt and braces: the platform's own parser sees only normal parts
    if Path::new(path)
        .components()
        .any(|c| !matches!(c, Component::Normal(_)))
    {
        return Err("not a plain relative path");
    }
    Ok(())
}

/// Write a bundle under `root`, in a folder named after its hash (the same
/// bundle received twice lands in the same place), and return the top-level
/// paths in bundle order.
pub fn unpack(bundle: &[u8], root: &Path) -> Result<Vec<PathBuf>, Error> {
    let entries = decode(bundle)?;
    let hash = Sha256::digest(bundle);
    let dir = root.join(crate::hex(&hash[..8]));
    let io = |e: io::Error| Error::Backend(format!("cannot write the received files: {e}"));
    let complete = dir.join(".complete");
    if !complete.exists() {
        if dir.exists() {
            // a half-written earlier attempt
            std::fs::remove_dir_all(&dir).map_err(io)?;
        }
        std::fs::create_dir_all(&dir).map_err(io)?;
        for entry in &entries {
            let target = dir.join(entry.path.replace('/', std::path::MAIN_SEPARATOR_STR));
            match &entry.data {
                None => std::fs::create_dir_all(&target).map_err(io)?,
                Some(data) => std::fs::write(&target, data).map_err(io)?,
            }
        }
        std::fs::write(&complete, b"").map_err(io)?;
    }
    Ok(entries
        .iter()
        .filter(|entry| !entry.path.contains('/'))
        .map(|entry| dir.join(&entry.path))
        .collect())
}

/// Where received files are extracted: `~/.cache/lan-mouse/received` on
/// Linux, `%LOCALAPPDATA%\lan-mouse\received` on Windows.
pub fn received_dir() -> PathBuf {
    #[cfg(windows)]
    let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from);
    #[cfg(not(windows))]
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")));
    base.unwrap_or_else(std::env::temp_dir)
        .join("lan-mouse")
        .join("received")
}

/// Keep only the `keep` most recently written received copies.
pub fn prune_received(root: &Path, keep: usize) {
    let Ok(read) = std::fs::read_dir(root) else {
        return;
    };
    let mut dirs: Vec<_> = read
        .filter_map(Result::ok)
        .filter(|e| e.path().is_dir())
        .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
        .collect();
    dirs.sort_by_key(|entry| std::cmp::Reverse(entry.0));
    for (_, dir) in dirs.into_iter().skip(keep) {
        let _ = std::fs::remove_dir_all(dir);
    }
}

/// The snapshot as it travels: a copy with files is the files alone (the text
/// a file manager adds is this PC's paths, meaningless elsewhere).
pub fn bundle_snapshot(
    snapshot: &ClipboardSnapshot,
    max_bytes: usize,
) -> Result<ClipboardSnapshot, Error> {
    let Some(item) = snapshot.get(Format::Files) else {
        return Ok(snapshot.clone());
    };
    let bundle = pack(&local_paths(item), max_bytes)?;
    Ok(ClipboardSnapshot::from_items([(Format::Files, bundle)]))
}

/// The snapshot to put on this clipboard: a received bundle extracted, its
/// paths as the file list and as plain text.
pub fn local_snapshot(
    snapshot: &ClipboardSnapshot,
    root: &Path,
) -> Result<ClipboardSnapshot, Error> {
    let Some(bundle) = snapshot.get(Format::Files) else {
        return Ok(snapshot.clone());
    };
    std::fs::create_dir_all(root)
        .map_err(|e| Error::Backend(format!("cannot create {}: {e}", root.display())))?;
    let paths = unpack(bundle, root)?;
    prune_received(root, KEEP_RECEIVED);
    let mut text = String::new();
    for path in &paths {
        text.push_str(&path.to_string_lossy());
        text.push('\n');
    }
    Ok(ClipboardSnapshot::from_items([
        (Format::Files, local_item(&paths)),
        (Format::Text, text.trim_end().as_bytes().to_vec()),
    ]))
}

/// Received copies kept in [`received_dir`]; older ones are deleted.
pub const KEEP_RECEIVED: usize = 8;

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(label: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "lan-mouse-files-{label}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn entry(path: &str, data: Option<&[u8]>) -> Entry {
        Entry {
            path: path.into(),
            data: data.map(<[u8]>::to_vec),
        }
    }

    #[test]
    fn a_folder_and_a_file_survive_the_round_trip_and_repack_identically() {
        let src = temp("src");
        std::fs::create_dir_all(src.join("photos/2026")).unwrap();
        std::fs::write(src.join("photos/a.png"), b"png-a").unwrap();
        std::fs::write(src.join("photos/2026/b.jpg"), b"jpg-b").unwrap();
        std::fs::write(src.join("notes.txt"), "olá".as_bytes()).unwrap();
        let bundle = pack(&[src.join("photos"), src.join("notes.txt")], 1 << 20).unwrap();

        let dest = temp("dest");
        let paths = unpack(&bundle, &dest).unwrap();
        assert_eq!(paths.len(), 2);
        assert!(paths[0].ends_with("photos") && paths[1].ends_with("notes.txt"));
        assert_eq!(
            std::fs::read(paths[0].join("2026/b.jpg")).unwrap(),
            b"jpg-b"
        );
        assert_eq!(std::fs::read(&paths[1]).unwrap(), "olá".as_bytes());
        // the echo of writing it is the same bundle
        assert_eq!(pack(&paths, 1 << 20).unwrap(), bundle);
        // receiving it again reuses the folder
        assert_eq!(unpack(&bundle, &dest).unwrap(), paths);
    }

    #[test]
    fn a_copy_over_the_limit_is_refused() {
        let src = temp("big");
        std::fs::write(src.join("big.bin"), vec![0u8; 2048]).unwrap();
        assert!(matches!(
            pack(&[src.join("big.bin")], 1024),
            Err(Error::TooLarge)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn links_are_left_out() {
        let src = temp("link");
        std::fs::create_dir_all(src.join("dir")).unwrap();
        std::fs::write(src.join("dir/real.txt"), b"x").unwrap();
        std::os::unix::fs::symlink("/etc/passwd", src.join("dir/passwd")).unwrap();
        let entries = decode(&pack(&[src.join("dir")], 1 << 20).unwrap()).unwrap();
        let names: Vec<_> = entries.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(names, vec!["dir", "dir/real.txt"]);
    }

    #[test]
    fn same_names_from_different_folders_get_numbered() {
        let src = temp("dup");
        for d in ["x", "y"] {
            std::fs::create_dir_all(src.join(d)).unwrap();
            std::fs::write(src.join(d).join("report.pdf"), d.as_bytes()).unwrap();
        }
        let entries = decode(
            &pack(
                &[src.join("x/report.pdf"), src.join("y/report.pdf")],
                1 << 20,
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(entries[1].path, "report (2).pdf");
    }

    #[test]
    fn hostile_paths_are_rejected() {
        for path in [
            "../evil",
            "a/../../evil",
            "/etc/passwd",
            "C:/Windows/evil",
            "a\\..\\evil",
            "CON",
            "nul.txt",
            "com1",
            "trailing.",
            "a//b",
            "",
            "a\u{0}b",
        ] {
            let bundle = encode(&[entry(path, Some(b"x"))]);
            assert!(decode(&bundle).is_err(), "accepted {path:?}");
        }
        // a file in a folder the bundle never created
        assert!(decode(&encode(&[entry("dir/file", Some(b"x"))])).is_err());
        // duplicates (also by case)
        assert!(decode(&encode(&[entry("A", Some(b"1")), entry("a", Some(b"2"))])).is_err());
        // ordinary names with spaces, accents and dots pass
        let fine = encode(&[
            entry("Minhas Fotos", None),
            entry("Minhas Fotos/férias.2026.jpg", Some(b"x")),
            entry("COMPANY.txt", Some(b"y")),
        ]);
        assert_eq!(decode(&fine).unwrap().len(), 3);
    }

    #[test]
    fn a_truncated_or_padded_bundle_is_rejected() {
        let bundle = encode(&[entry("a.txt", Some(b"hello"))]);
        assert!(decode(&bundle[..bundle.len() - 1]).is_err());
        let mut padded = bundle.clone();
        padded.push(0);
        assert!(decode(&padded).is_err());
        assert!(decode(b"LMF1").is_err());
        assert!(decode(b"nope").is_err());
    }

    #[test]
    fn uri_lists_round_trip_with_spaces_and_accents() {
        let paths = vec![
            PathBuf::from("/home/ana/Área de Trabalho/a b.txt"),
            PathBuf::from("/tmp/x%y"),
        ];
        assert_eq!(paths_from_uri_list(&uri_list(&paths)), paths);
        assert_eq!(paths_from_gnome_copied(&gnome_copied(&paths)), paths);
        assert_eq!(
            paths_from_uri_list(b"# comment\r\nhttps://example.com\r\nfile:///etc/hosts\r\n"),
            vec![PathBuf::from("/etc/hosts")]
        );
        assert!(
            paths_from_gnome_copied(b"file:///etc/hosts").is_empty(),
            "needs copy/cut"
        );
        assert_eq!(
            paths_from_gnome_copied(b"cut\nfile://localhost/etc/hosts"),
            vec![PathBuf::from("/etc/hosts")]
        );
    }

    #[test]
    fn local_items_round_trip() {
        let paths = vec![PathBuf::from("/a/b"), PathBuf::from("/c d/e")];
        assert_eq!(local_paths(&local_item(&paths)), paths);
    }
}
