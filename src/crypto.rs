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

/// load certificate from file
pub fn load_certificate(path: &Path) -> Result<Certificate, Error> {
    let f = File::open(path)?;

    let mut reader = BufReader::new(f);
    let mut pem = String::new();
    reader.read_to_string(&mut pem)?;
    Ok(Certificate::from_pem(pem.as_str())?)
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
    /* FIXME windows permissions */
    let mut writer = BufWriter::new(f);
    writer.write_all(serialized.as_bytes())?;
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
