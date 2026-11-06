use std::fs;
use std::io::{self, BufWriter, Read, Write};
use std::path::Path;
use std::{fs::File, io::BufReader};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

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

/// load certificate from file
pub fn load_certificate(path: &Path) -> Result<Certificate, Error> {
    let f = File::open(path)?;

    let mut reader = BufReader::new(f);
    let mut pem = String::new();
    reader.read_to_string(&mut pem)?;
    Ok(Certificate::from_pem(pem.as_str())?)
}

/// Best-effort ACL hardening for the private key on Windows.
///
/// `std` cannot set ACLs, so this shells out to `icacls` (present on every
/// supported Windows release): inheritance is removed and the current user
/// gets read-only access, mirroring the `0400` mode used on Unix. A failure
/// is logged but never prevents the service from starting.
#[cfg(windows)]
fn restrict_windows_key_acl(path: &Path) {
    use std::os::windows::process::CommandExt;
    let Ok(user) = std::env::var("USERNAME") else {
        log::warn!("could not determine the current user; key ACL not restricted");
        return;
    };
    // Capture stdout/stderr: icacls prints "processed file: ..." on success,
    // which would otherwise pollute the JSON output of `doctor --json` and
    // the release smoke test's ConvertFrom-Json.
    let result = std::process::Command::new("icacls")
        .arg(path)
        .arg("/inheritance:r")
        .arg("/grant:r")
        .arg(format!("{user}:R"))
        .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
        .output();
    match result {
        Ok(status) if status.success() => {
            log::debug!("restricted private key ACL to {user}:R");
        }
        Ok(status) => log::warn!("icacls failed for {path:?} ({status})"),
        Err(e) => log::warn!("could not run icacls for {path:?}: {e}"),
    }
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
    let parent = path.parent().expect("is a path");
    fs::create_dir_all(parent)?;
    let f = File::create(path)?;
    #[cfg(unix)]
    {
        let mut perm = f.metadata()?.permissions();
        perm.set_mode(0o400); /* r-- --- --- */
        f.set_permissions(perm)?;
    }
    // On Windows the handle is reopened after creation so the ACL can be
    // applied before the PEM bytes are written; on Unix the create handle is
    // used directly.
    #[cfg(windows)]
    let f = {
        drop(f);
        File::options().append(true).open(path)?
    };

    let mut writer = BufWriter::new(f);
    writer.write_all(serialized.as_bytes())?;
    drop(writer);

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
}
