use std::fs;
use std::io::{self, BufWriter, Read, Write};
use std::path::Path;
use std::{fs::File, io::BufReader};

use sha2::{Digest, Sha256};
use thiserror::Error;
use webrtc_dtls::crypto::Certificate;

#[derive(Debug, Error)]
pub enum Error {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Dtls(#[from] webrtc_dtls::Error),
}

pub fn generate_fingerprint(cert: &[u8]) -> String {
    let mut hash = Sha256::new();
    hash.update(cert);
    let bytes = hash
        .finalize()
        .iter()
        .map(|x| format!("{x:02x}"))
        .collect::<Vec<_>>();
    bytes.join(":").to_lowercase()
}

pub fn certificate_fingerprint(cert: &Certificate) -> String {
    let certificate = cert.certificate.first().expect("certificate missing");
    generate_fingerprint(certificate)
}

/// Short authentication string for a pair of devices.
///
/// Six decimal digits derived from both fingerprints in a stable,
/// order-independent way, so two machines that know each other's
/// fingerprint display the same code and a substituted certificate produces a
/// different one. It is a comparison aid, not a replacement for an
/// authenticated exchange.
pub fn sas_code(local_fingerprint: &str, peer_fingerprint: &str) -> String {
    let mut pair = [local_fingerprint, peer_fingerprint];
    pair.sort_unstable();
    let mut hash = Sha256::new();
    hash.update(b"lan-mouse-sas-v1");
    hash.update(pair[0].as_bytes());
    hash.update(b"\0");
    hash.update(pair[1].as_bytes());
    let digest = hash.finalize();
    let value = u32::from_be_bytes([digest[0], digest[1], digest[2], digest[3]]) % 1_000_000;
    format!("{value:06}")
}

/// Canonicalise a user-supplied SHA-256 fingerprint.
///
/// Accepts the same fingerprint with or without separators, in upper or lower
/// case, and returns the canonical lowercase colon-separated form used
/// everywhere else. Returns `None` when the input is not a SHA-256
/// fingerprint, so a typo cannot silently pin an unusable key.
pub fn normalize_fingerprint(input: &str) -> Option<String> {
    let hex: String = input
        .chars()
        .filter(|c| *c != ':' && *c != '-' && !c.is_whitespace())
        .collect();
    if hex.len() != 64 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    Some(
        hex.as_bytes()
            .chunks(2)
            .map(|pair| std::str::from_utf8(pair).expect("hex").to_ascii_lowercase())
            .collect::<Vec<_>>()
            .join(":"),
    )
}

/// Largest key/certificate PEM file we are willing to read.
///
/// A self-signed PEM is ~2 KiB; the cap only exists so a corrupted or
/// replaced file cannot make the service allocate unbounded memory at start.
const MAX_PEM_LEN: u64 = 1024 * 1024;

/// load certificate from file
pub fn load_certificate(path: &Path) -> Result<Certificate, Error> {
    let f = File::open(path)?;
    if f.metadata()?.len() > MAX_PEM_LEN {
        return Err(Error::Io(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("certificate file {path:?} is unexpectedly large"),
        )));
    }

    let mut reader = BufReader::new(f);
    let mut pem = String::new();
    reader.read_to_string(&mut pem)?;
    Ok(Certificate::from_pem(pem.as_str())?)
}

/// Restrict the private key file to the current user on Windows.
///
/// `std` cannot set ACLs, so this shells out to `icacls` (present on every
/// supported Windows release): inheritance is removed and the current user
/// gets read-only access, mirroring the `0400` mode used on Unix.
///
/// The ACL change is verified by reopening the file. `/inheritance:r` removes
/// the inherited ACEs *before* the explicit grant is added, so a failed
/// `/grant` (renamed/domain account, unusual user name, ...) would otherwise
/// leave the file unreadable by its own owner and the next start would fail in
/// [`load_or_generate_key_and_cert`]. On any failure the inherited ACL is
/// restored (best effort) so a missing key never bricks the service; the
/// problem is logged loudly for the `doctor`/manual review.
#[cfg(windows)]
fn restrict_windows_key_acl(path: &Path) {
    use std::ffi::OsStr;
    use std::os::windows::process::CommandExt;

    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    fn run_icacls(args: &[&OsStr]) -> std::io::Result<std::process::Output> {
        std::process::Command::new("icacls")
            .args(args)
            .creation_flags(CREATE_NO_WINDOW)
            .output()
    }

    let restore_inheritance = |why: &str| {
        log::warn!("{why}; restoring inherited permissions for {path:?}");
        let args: [&OsStr; 2] = [path.as_os_str(), OsStr::new("/inheritance:e")];
        match run_icacls(&args) {
            Ok(output) if output.status.success() => {
                log::warn!(
                    "restored inherited ACL on {path:?}; move the certificate to a private \
                     directory or fix the ACL manually"
                );
            }
            Ok(output) => {
                log::warn!(
                    "could not restore the inherited ACL on {path:?} ({})",
                    output.status
                )
            }
            Err(e) => log::warn!("could not run icacls to restore {path:?}: {e}"),
        }
    };

    let Ok(user) = std::env::var("USERNAME") else {
        log::warn!("could not determine the current user; key ACL not restricted");
        return;
    };
    let grant = format!("{user}:R");
    let restrict_args: [&OsStr; 4] = [
        path.as_os_str(),
        OsStr::new("/inheritance:r"),
        OsStr::new("/grant:r"),
        OsStr::new(grant.as_str()),
    ];
    match run_icacls(&restrict_args) {
        Ok(output) if output.status.success() => {
            // Post-condition: the owner must still be able to read the key.
            if std::fs::File::open(path).is_ok() {
                log::debug!("restricted private key ACL to {user}:R");
            } else {
                restore_inheritance(
                    "the current user cannot read the private key after the ACL change",
                );
            }
        }
        Ok(output) => {
            restore_inheritance(&format!("icacls failed for {path:?} ({})", output.status))
        }
        Err(e) => restore_inheritance(&format!("could not run icacls for {path:?}: {e}")),
    }
}

/// Create (or replace) the private key file without ever exposing the PEM
/// bytes with a wider mode than intended.
fn write_private_file(path: &Path, contents: &[u8]) -> Result<(), Error> {
    #[cfg(unix)]
    let file = {
        use std::os::unix::fs::OpenOptionsExt;
        // The mode is applied atomically at creation: no window in which the
        // private key exists with the process umask (e.g. 0644).
        fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o400)
            .open(path)?
    };
    #[cfg(windows)]
    let file = File::create(path)?;

    let mut writer = BufWriter::new(file);
    writer.write_all(contents)?;
    writer.flush()?;
    drop(writer);

    // Defensive re-check: an existing file keeps its old mode when opened
    // with `create(true)`, and some FUSE/network filesystems ignore the
    // creation mode.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(path)?.permissions().mode() & 0o777;
        if mode != 0o400 {
            fs::set_permissions(path, fs::Permissions::from_mode(0o400))?;
        }
    }
    Ok(())
}

pub(crate) fn load_or_generate_key_and_cert(path: &Path) -> Result<Certificate, Error> {
    if path.exists() && path.is_file() {
        Ok(load_certificate(path)?)
    } else {
        generate_key_and_cert(path)
    }
}

pub(crate) fn generate_key_and_cert(path: &Path) -> Result<Certificate, Error> {
    let cert = Certificate::generate_self_signed(["ignored".to_owned()])?;
    let serialized = cert.serialize_pem();
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }
    write_private_file(path, serialized.as_bytes())?;

    // Windows: harden the ACL once the PEM bytes are on disk, then verify
    // that the owner can still read the key (see restrict_windows_key_acl).
    #[cfg(windows)]
    restrict_windows_key_acl(path);

    Ok(cert)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn unique_path(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join("lan-mouse-crypto-tests");
        std::fs::create_dir_all(&dir).expect("test dir");
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        dir.join(format!("{}-{id}-{name}.pem", std::process::id()))
    }

    #[test]
    fn fingerprint_has_the_sha256_shape() {
        let cert = generate_key_and_cert(&unique_path("shape")).expect("cert");
        let fingerprint = certificate_fingerprint(&cert);
        let parts = fingerprint.split(':').collect::<Vec<_>>();
        assert_eq!(parts.len(), 32, "sha256 is 32 bytes");
        for part in parts {
            assert_eq!(part.len(), 2);
            assert!(
                part.chars()
                    .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
                "expected lowercase hex, got {part:?}"
            );
        }
    }

    #[test]
    fn generate_then_load_returns_the_same_certificate() {
        let path = unique_path("roundtrip");
        let generated = load_or_generate_key_and_cert(&path).expect("generate");
        assert!(path.exists());
        let loaded = load_or_generate_key_and_cert(&path).expect("load");
        assert_eq!(
            certificate_fingerprint(&generated),
            certificate_fingerprint(&loaded)
        );
        assert_eq!(
            generated.certificate, loaded.certificate,
            "loading must not regenerate"
        );
    }

    #[test]
    fn different_certificates_have_different_fingerprints() {
        let a = generate_key_and_cert(&unique_path("a")).expect("cert a");
        let b = generate_key_and_cert(&unique_path("b")).expect("cert b");
        assert_ne!(certificate_fingerprint(&a), certificate_fingerprint(&b));
    }

    #[test]
    fn sas_is_stable_and_order_independent() {
        let a = "aa:bb:cc";
        let b = "dd:ee:ff";
        let code = sas_code(a, b);
        assert_eq!(code, sas_code(b, a), "both sides must show the same code");
        assert_eq!(code.len(), 6);
        assert!(code.chars().all(|c| c.is_ascii_digit()), "got {code}");
        assert_ne!(
            code,
            sas_code(a, "11:22:33"),
            "a substituted certificate changes the code"
        );
    }

    #[test]
    fn normalize_fingerprint_accepts_common_spellings() {
        let canonical = "aa:bb:cc:dd:ee:ff:00:11:22:33:44:55:66:77:88:99:\
                         aa:bb:cc:dd:ee:ff:00:11:22:33:44:55:66:77:88:99";
        let upper = canonical.replace(':', "").to_uppercase();
        assert_eq!(normalize_fingerprint(&upper).as_deref(), Some(canonical));
        // Spaces and dashes are just visual separators.
        let dashed = canonical.replace(':', "-");
        assert_eq!(normalize_fingerprint(&dashed).as_deref(), Some(canonical));
        let generated = certificate_fingerprint(
            &generate_key_and_cert(&unique_path("normalize")).expect("cert"),
        );
        assert_eq!(
            normalize_fingerprint(&generated).as_deref(),
            Some(generated.as_str())
        );
    }

    #[test]
    fn normalize_fingerprint_rejects_typos() {
        assert_eq!(normalize_fingerprint(""), None);
        assert_eq!(normalize_fingerprint("aa:bb"), None);
        assert_eq!(
            normalize_fingerprint(&"zz".repeat(32)),
            None,
            "non-hex must not be pinned"
        );
        assert_eq!(
            normalize_fingerprint(&"aa".repeat(31)),
            None,
            "sha256 is exactly 32 bytes"
        );
    }

    #[cfg(unix)]
    #[test]
    fn key_file_is_not_world_readable() {
        use std::os::unix::fs::PermissionsExt;
        let path = unique_path("perms");
        generate_key_and_cert(&path).expect("cert");
        let mode = std::fs::metadata(&path)
            .expect("metadata")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o400);
    }

    #[cfg(unix)]
    #[test]
    fn an_existing_loose_key_file_is_tightened() {
        use std::os::unix::fs::PermissionsExt;
        let path = unique_path("loose");
        std::fs::write(&path, b"stale contents").expect("seed");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).expect("loosen");
        generate_key_and_cert(&path).expect("regenerate over a loose file");
        let mode = std::fs::metadata(&path)
            .expect("metadata")
            .permissions()
            .mode();
        assert_eq!(
            mode & 0o777,
            0o400,
            "an existing file must be re-tightened after regeneration"
        );
    }

    #[test]
    fn oversized_certificate_file_is_rejected() {
        let path = unique_path("huge");
        std::fs::write(&path, vec![b'x'; (MAX_PEM_LEN + 1) as usize]).expect("seed");
        let result = load_certificate(&path);
        assert!(
            matches!(result, Err(Error::Io(ref e)) if e.kind() == io::ErrorKind::InvalidData),
            "a huge PEM file must be rejected before reading it"
        );
    }
}
