//! Geode custody for Secret payloads. h3s stores no cipher; sealing and
//! opening delegate to the `geode` CLI (`geode seal` / `geode cat`), suite
//! 0x01. The database keeps only an envelope that names the vault entry.

use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicU64, Ordering},
        LazyLock, Mutex,
    },
    time::{SystemTime, UNIX_EPOCH},
};

use crate::{Error, Result, SecretsSealer};

/// Stored values starting with this marker are Geode envelopes.
const ENVELOPE: &[u8] = b"H3SGEO1:";

static COUNTER: LazyLock<AtomicU64> = LazyLock::new(|| {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(0x5eed);
    AtomicU64::new(nanos << 20 | 1)
});

pub struct GeodeSealer {
    binary: PathBuf,
    key: PathBuf,
    vault: PathBuf,
    staging: PathBuf,
    seal_lock: Mutex<()>,
}

impl GeodeSealer {
    /// Create custody under `dir`: a Geode identity key and vault, initialized
    /// on first use. Both stay inside `dir` (the server data directory).
    pub fn create(binary: impl AsRef<Path>, dir: impl AsRef<Path>) -> Result<Self> {
        let binary = binary.as_ref().to_owned();
        let dir = dir.as_ref().to_owned();
        let sealer = Self {
            binary,
            key: dir.join("secrets.gkey"),
            vault: dir.join("secrets.vault"),
            staging: dir.join("secrets.staging"),
            seal_lock: Mutex::new(()),
        };
        fs::create_dir_all(&dir).map_err(|e| Error::Worker(e.to_string()))?;
        fs::create_dir_all(&sealer.staging).map_err(|e| Error::Worker(e.to_string()))?;
        if !sealer.key.exists() {
            sealer.geode(&["keygen".into(), sealer.key.display().to_string()])?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&sealer.key, fs::Permissions::from_mode(0o600))
                    .map_err(|e| Error::Worker(e.to_string()))?;
            }
        }
        if !sealer.vault.exists() {
            sealer.geode(&[
                "vault".into(),
                "init".into(),
                sealer.vault.display().to_string(),
            ])?;
        }
        Ok(sealer)
    }

    fn geode(&self, args: &[String]) -> Result<Vec<u8>> {
        let mut argv: Vec<String> = vec!["--key".into(), self.key.display().to_string()];
        argv.extend(args.iter().cloned());
        let output = Command::new(&self.binary)
            .args(&argv)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| Error::Worker(format!("geode spawn: {e}")))?
            .wait_with_output()
            .map_err(|e| Error::Worker(format!("geode wait: {e}")))?;
        if !output.status.success() {
            return Err(Error::Worker(format!(
                "geode failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        Ok(output.stdout)
    }
}

impl SecretsSealer for GeodeSealer {
    fn seal(&self, plaintext: &[u8]) -> Result<Vec<u8>> {
        let entry = format!("s{:x}", COUNTER.fetch_add(1, Ordering::SeqCst));
        let source = self.staging.join(&entry);
        let _guard = self
            .seal_lock
            .lock()
            .map_err(|e| Error::Worker(e.to_string()))?;
        fs::write(&source, plaintext).map_err(|e| Error::Worker(e.to_string()))?;
        let sealed = self.geode(&[
            "seal".into(),
            source.display().to_string(),
            self.vault.display().to_string(),
        ]);
        let _ = fs::remove_file(&source);
        sealed?;
        let mut out = ENVELOPE.to_vec();
        out.extend_from_slice(entry.as_bytes());
        Ok(out)
    }

    fn open(&self, sealed: &[u8]) -> Result<Vec<u8>> {
        let Some(entry) = sealed.strip_prefix(ENVELOPE) else {
            return Err(Error::Invalid("not a geode envelope".into()));
        };
        let entry = std::str::from_utf8(entry)
            .map_err(|_| Error::Invalid("geode envelope name is not utf8".into()))?;
        self.geode(&["cat".into(), self.vault.display().to_string(), entry.into()])
    }
}
