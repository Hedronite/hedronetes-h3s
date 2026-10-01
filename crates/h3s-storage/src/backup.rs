//! Offline copy of one h3s registry.
//!
//! The copy is `server/db/h3s.db` and, when they exist, the `h3s.db-wal` and
//! `h3s.db-shm` sidecars that make the database consistent. Nothing else: the
//! PKI, the CA, the node token and the runtime state a node needs to run a
//! container all belong to the commands that call this, per the SPEC's G2. The
//! registry lock refusal is the caller's decision too, because the caller is
//! what holds the lock.
//!
//! A destination that already exists is refused, so two backups can never be
//! merged by accident.
use std::{
    fs,
    path::{Path, PathBuf},
};

use crate::{Error, Result};

/// The registry file and its sidecars, relative to a data directory.
const REGISTRY: &str = "server/db/h3s.db";
const SIDECARS: [&str; 2] = ["server/db/h3s.db-wal", "server/db/h3s.db-shm"];

/// What a copy moved, in the order it was written.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Copied {
    /// Paths relative to the data directory, all of them files.
    pub members: Vec<String>,
    /// Total bytes copied, sidecars included.
    pub bytes: u64,
}

fn invalid(message: impl Into<String>) -> Error {
    Error::Invalid(message.into())
}

/// The registry files a copy carries: the database, and whichever sidecars
/// exist beside it. A missing registry is not a registry directory.
fn members(data_dir: &Path) -> Result<Vec<String>> {
    let mut members = Vec::new();
    for candidate in std::iter::once(REGISTRY.to_owned()).chain(SIDECARS.map(str::to_owned)) {
        match fs::symlink_metadata(data_dir.join(&candidate)) {
            Ok(metadata) if metadata.is_file() => members.push(candidate),
            Ok(_) => return Err(invalid(format!("{candidate} is not a regular file"))),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if candidate == REGISTRY {
                    return Err(invalid(format!(
                        "{} is not an h3s data directory: {REGISTRY} is missing",
                        data_dir.display()
                    )));
                }
            }
            Err(error) => return Err(Error::Worker(format!("{candidate}: {error}"))),
        }
    }
    Ok(members)
}

fn copy_file(from: &Path, to: &Path, copied: &mut Copied, relative: &str) -> Result<()> {
    if let Some(parent) = to.parent() {
        fs::create_dir_all(parent).map_err(|e| Error::Worker(format!("{relative}: {e}")))?;
    }
    let bytes = fs::copy(from, to).map_err(|e| Error::Worker(format!("{relative}: {e}")))?;
    copied.bytes = copied.bytes.saturating_add(bytes);
    Ok(())
}

/// Copy the registry of `data_dir` into `destination`, which must not exist.
pub fn backup(data_dir: &Path, destination: &Path) -> Result<Copied> {
    let data_dir = data_dir.to_owned();
    let destination = destination.to_owned();
    let members = members(&data_dir)?;
    match destination.symlink_metadata() {
        Ok(_) => {
            return Err(invalid(format!(
                "backup destination {} already exists",
                destination.display()
            )))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(Error::Worker(error.to_string())),
    }
    if destination.starts_with(&data_dir) {
        return Err(invalid("backup destination is inside the data directory"));
    }
    fs::create_dir_all(&destination).map_err(|e| Error::Worker(e.to_string()))?;
    let mut copied = Copied::default();
    for member in &members {
        copy_file(
            &data_dir.join(member),
            &destination.join(member),
            &mut copied,
            member,
        )?;
        copied.members.push(member.clone());
    }
    Ok(copied)
}

/// Put a copy written by [`backup`] back under `data_dir`, replacing it.
pub fn restore(data_dir: &Path, source: &Path) -> Result<Copied> {
    let data_dir = data_dir.to_owned();
    let source = source.to_owned();
    let members = members(&source).map_err(|error| match error {
        Error::Invalid(_) => invalid(format!("{} is not a backup", source.display())),
        other => other,
    })?;
    fs::create_dir_all(&data_dir).map_err(|e| Error::Worker(e.to_string()))?;
    let mut copied = Copied::default();
    for member in &members {
        copy_file(
            &source.join(member),
            &data_dir.join(member),
            &mut copied,
            member,
        )?;
        copied.members.push(member.clone());
    }
    Ok(copied)
}

/// The absolute path a registry file has inside a data directory.
pub fn member_path(data_dir: &Path, member: &str) -> PathBuf {
    data_dir.join(member)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// A data directory with the registry, both sidecars, and the custody and
    /// runtime files this step must never copy.
    fn data_dir(root: &Path) -> PathBuf {
        let dir = root.join("runtime");
        for (path, bytes) in [
            (REGISTRY, b"registry-bytes".to_vec()),
            (SIDECARS[0], b"wal-bytes".to_vec()),
            (SIDECARS[1], b"shm-bytes".to_vec()),
            ("server/ca.crt", b"ca-bytes".to_vec()),
            ("server/node-token", b"token-bytes".to_vec()),
            ("server/tls/cluster-pki.json", b"pki-bytes".to_vec()),
            ("containerd/state", b"junk".to_vec()),
            ("agent/pods/uid/volumes/token", b"junk".to_vec()),
        ] {
            let path = dir.join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::File::create(&path).unwrap().write_all(&bytes).unwrap();
        }
        dir
    }
    fn read(path: &Path) -> Vec<u8> {
        fs::read(path).unwrap()
    }

    /// The copy the brief names: the registry and its sidecars, and nothing
    /// else from the directory.
    #[test]
    fn a_backup_copies_the_registry_and_its_sidecars_and_nothing_else() {
        let root = tempfile::tempdir().unwrap();
        let dir = data_dir(root.path());
        let destination = root.path().join("backup");
        let copied = backup(&dir, &destination).unwrap();
        assert_eq!(
            copied.members,
            vec![
                REGISTRY.to_owned(),
                SIDECARS[0].to_owned(),
                SIDECARS[1].to_owned()
            ]
        );
        assert_eq!(
            read(&destination.join(REGISTRY)),
            b"registry-bytes".to_vec()
        );
        assert_eq!(read(&destination.join(SIDECARS[0])), b"wal-bytes".to_vec());
        assert_eq!(read(&destination.join(SIDECARS[1])), b"shm-bytes".to_vec());
        assert_eq!(
            copied.bytes,
            14 + 9 + 9,
            "every copied byte is accounted for"
        );
        for absent in [
            "server/tls",
            "server/ca.crt",
            "server/node-token",
            "containerd",
            "agent",
        ] {
            assert!(
                !destination.join(absent).exists(),
                "{absent} must not be in this copy"
            );
        }
    }

    /// The sidecars are conditional; the registry is not.
    #[test]
    fn sidecars_are_copied_when_they_exist_and_the_registry_is_required() {
        let root = tempfile::tempdir().unwrap();
        let dir = data_dir(root.path());
        fs::remove_file(dir.join(SIDECARS[0])).unwrap();
        fs::remove_file(dir.join(SIDECARS[1])).unwrap();
        let destination = root.path().join("backup");
        let copied = backup(&dir, &destination).unwrap();
        assert_eq!(copied.members, vec![REGISTRY.to_owned()]);
        assert!(!destination.join(SIDECARS[0]).exists());
        assert!(!destination.join(SIDECARS[1]).exists());
        // Without a registry there is nothing to copy, and that is not a
        // registry directory.
        let empty = root.path().join("empty");
        fs::create_dir_all(&empty).unwrap();
        assert!(backup(&empty, &root.path().join("nowhere")).is_err());
    }

    /// A destination that already exists is refused and left untouched.
    #[test]
    fn an_existing_destination_is_refused_and_nothing_is_written() {
        let root = tempfile::tempdir().unwrap();
        let dir = data_dir(root.path());
        let destination = root.path().join("backup");
        backup(&dir, &destination).unwrap();
        let before = read(&destination.join(REGISTRY));
        let failure = backup(&dir, &destination).unwrap_err();
        assert!(failure.to_string().contains("already exists"), "{failure}");
        assert_eq!(read(&destination.join(REGISTRY)), before);
        // A plain file is refused as well, and a destination inside the data
        // directory cannot swallow its own source.
        let file = root.path().join("file");
        fs::File::create(&file).unwrap();
        assert!(backup(&dir, &file).is_err());
        assert!(backup(&dir, &dir.join("inside")).is_err());
    }

    /// Restore puts the same registry files back and replaces what is there.
    #[test]
    fn restore_returns_the_same_files_and_replaces_older_ones() {
        let root = tempfile::tempdir().unwrap();
        let dir = data_dir(root.path());
        let destination = root.path().join("backup");
        let copied = backup(&dir, &destination).unwrap();
        let restored = root.path().join("restored");
        let back = restore(&restored, &destination).unwrap();
        assert_eq!(back, copied);
        assert_eq!(read(&restored.join(REGISTRY)), b"registry-bytes".to_vec());
        assert_eq!(read(&restored.join(SIDECARS[0])), b"wal-bytes".to_vec());
        assert_eq!(read(&restored.join(SIDECARS[1])), b"shm-bytes".to_vec());
        assert!(!restored.join("server/tls").exists());
        assert!(!restored.join("containerd").exists());
        fs::remove_file(restored.join(REGISTRY)).unwrap();
        assert_eq!(restore(&restored, &destination).unwrap(), copied);
        assert_eq!(read(&restored.join(REGISTRY)), b"registry-bytes".to_vec());
        // A source that is not a copy of a registry is refused.
        let empty = root.path().join("empty");
        fs::create_dir_all(&empty).unwrap();
        assert!(restore(&root.path().join("other"), &empty).is_err());
    }

    /// Member paths are the ones the SPEC spells.
    #[test]
    fn member_paths_are_the_spec_paths() {
        let dir = Path::new("/data");
        assert_eq!(
            member_path(dir, REGISTRY),
            Path::new("/data/server/db/h3s.db")
        );
        assert_eq!(
            member_path(dir, SIDECARS[0]),
            Path::new("/data/server/db/h3s.db-wal")
        );
        assert_eq!(
            member_path(dir, SIDECARS[1]),
            Path::new("/data/server/db/h3s.db-shm")
        );
    }
}
