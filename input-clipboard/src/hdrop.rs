//! `CF_HDROP`: how Windows Explorer puts copied files on the clipboard.
//!
//! The global memory holds a `DROPFILES` header followed by the paths, each
//! NUL-terminated, the list ending with one more NUL:
//!
//! ```text
//! u32 pFiles (offset of the list, 20) | i32 pt.x | i32 pt.y | i32 fNC | i32 fWide
//! path\0path\0\0            (UTF-16LE when fWide, else the ANSI code page)
//! ```
//!
//! Pure byte handling, so it is tested on every platform; the Windows backend
//! reads and writes the bytes.

use std::path::PathBuf;

const HEADER: usize = 20;

/// The paths of a `CF_HDROP` block. ANSI lists (fWide = 0, from very old
/// programs) are read as Latin-1, which is right for ASCII paths.
pub fn parse(bytes: &[u8]) -> Vec<PathBuf> {
    let Some(header) = bytes.get(..HEADER) else {
        return Vec::new();
    };
    let offset = u32::from_le_bytes(header[0..4].try_into().expect("4")) as usize;
    let wide = i32::from_le_bytes(header[16..20].try_into().expect("4")) != 0;
    let Some(list) = bytes.get(offset..) else {
        return Vec::new();
    };
    let mut paths = Vec::new();
    if wide {
        let units: Vec<u16> = list
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| u16::from_le_bytes(*pair))
            .collect();
        for path in units.split(|&unit| unit == 0) {
            if path.is_empty() {
                break;
            }
            paths.push(PathBuf::from(String::from_utf16_lossy(path)));
        }
    } else {
        for path in list.split(|&byte| byte == 0) {
            if path.is_empty() {
                break;
            }
            paths.push(PathBuf::from(
                path.iter().map(|&b| b as char).collect::<String>(),
            ));
        }
    }
    paths
}

/// A `CF_HDROP` block (wide) for these paths.
pub fn build(paths: &[PathBuf]) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEADER + 64 * paths.len());
    out.extend_from_slice(&(HEADER as u32).to_le_bytes());
    out.extend_from_slice(&0i32.to_le_bytes()); // pt.x
    out.extend_from_slice(&0i32.to_le_bytes()); // pt.y
    out.extend_from_slice(&0i32.to_le_bytes()); // fNC
    out.extend_from_slice(&1i32.to_le_bytes()); // fWide
    for path in paths {
        for unit in path.to_string_lossy().encode_utf16() {
            out.extend_from_slice(&unit.to_le_bytes());
        }
        out.extend_from_slice(&0u16.to_le_bytes());
    }
    out.extend_from_slice(&0u16.to_le_bytes());
    out
}

/// "Preferred DropEffect" value telling Explorer to copy (not move) on paste.
pub const DROPEFFECT_COPY: u32 = 1;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wide_lists_round_trip() {
        let paths = vec![
            PathBuf::from(r"C:\Users\Ana\Área de Trabalho\relatório.pdf"),
            PathBuf::from(r"D:\fotos"),
        ];
        assert_eq!(parse(&build(&paths)), paths);
    }

    #[test]
    fn an_ansi_list_from_an_old_program_is_read() {
        let mut block = Vec::new();
        block.extend_from_slice(&20u32.to_le_bytes());
        block.extend_from_slice(&[0; 12]);
        block.extend_from_slice(&0i32.to_le_bytes()); // not wide
        block.extend_from_slice(b"C:\\a.txt\0C:\\b\0\0");
        assert_eq!(
            parse(&block),
            vec![PathBuf::from(r"C:\a.txt"), PathBuf::from(r"C:\b")]
        );
    }

    #[test]
    fn garbage_yields_no_paths() {
        assert!(parse(&[]).is_empty());
        assert!(parse(&[1, 2, 3]).is_empty());
        let mut bad_offset = build(&[PathBuf::from("x")]);
        bad_offset[0] = 200;
        assert!(parse(&bad_offset).is_empty());
    }
}
