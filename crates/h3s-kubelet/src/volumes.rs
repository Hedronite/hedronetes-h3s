//! ConfigMap/Secret directory projections. Only the root-owned agent writes here;
//! containers receive read-only bind mounts. Payloads never enter errors or logs.
//! Which volumes and mounts are legal is decided by the runtime profile before
//! any of this runs; here the admitted spec is only parsed and projected.
use crate::{inputs, invalid, pod, Agent, Result};
use h3s_certs::private;
use h3s_cri::v1::Mount;
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{Read, Write},
    os::unix::fs::{symlink, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};

const LIMIT: usize = 1024 * 1024;
const GENERATION: &str = "..h3s-";
#[derive(PartialEq, Eq)]
struct File {
    bytes: Vec<u8>,
    mode: u32,
}
type Payload = BTreeMap<String, File>;
struct Source<'a> {
    name: &'a str,
    object: &'a str,
    kind: &'static str,
    spec: &'a Value,
    optional: bool,
    mode: u32,
}
fn array(v: &Value) -> Result<&[Value]> {
    if v.is_null() {
        Ok(&[])
    } else {
        v.as_array()
            .map(Vec::as_slice)
            .ok_or_else(|| invalid("volume field must be an array"))
    }
}
fn mode(v: &Value, default: u32) -> Result<u32> {
    if v.is_null() {
        Ok(default)
    } else {
        v.as_u64()
            .filter(|n| *n <= 0o777)
            .map(|n| n as u32)
            .ok_or_else(|| invalid("volume file mode must be within 0000-0777"))
    }
}
fn relative(path: &str) -> bool {
    !path.is_empty()
        && path.len() <= 4096
        && !path.contains('\0')
        && path.split('/').count() <= 32
        && path
            .split('/')
            .all(|s| !s.is_empty() && s.len() <= 255 && s != "." && s != "..")
        && !path.starts_with("..")
        && !path.starts_with('/')
}
fn paths<'a>(paths: impl Iterator<Item = &'a str>) -> Result<()> {
    let mut seen = BTreeSet::new();
    for path in paths {
        if !relative(path) || !seen.insert(path) {
            return Err(invalid("invalid or duplicate projected file path"));
        }
    }
    for path in &seen {
        let mut prefix = *path;
        while let Some((parent, _)) = prefix.rsplit_once('/') {
            if seen.contains(parent) {
                return Err(invalid("projected file conflicts with a directory"));
            }
            prefix = parent;
        }
    }
    Ok(())
}
/// Volume sources as the runtime profile already admitted them. A claim is a
/// node-local directory, not a projection, and is mounted from its own path.
fn sources(p: &Value) -> Result<Vec<Source<'_>>> {
    let mut out = vec![];
    for v in array(&p["spec"]["volumes"])? {
        if !v["persistentVolumeClaim"].is_null() || !v["projected"].is_null() {
            continue;
        }
        let (spec, kind, field) = if v["configMap"].is_null() {
            (&v["secret"], "secrets", "secretName")
        } else {
            (&v["configMap"], "configmaps", "name")
        };
        out.push(Source {
            name: pod::text(v, "name")?,
            object: pod::text(spec, field)?,
            kind,
            spec,
            optional: spec["optional"] == true,
            mode: mode(&spec["defaultMode"], 0o644)?,
        });
    }
    Ok(out)
}
fn payload(source: &Source<'_>, mut data: BTreeMap<String, Vec<u8>>) -> Result<Payload> {
    let mut out = Payload::new();
    let items = array(&source.spec["items"])?;
    if items.is_empty() {
        for (name, bytes) in data {
            out.insert(
                name,
                File {
                    bytes,
                    mode: source.mode,
                },
            );
        }
    } else {
        for item in items {
            // A key may intentionally be projected at more than one path.
            match data.get_mut(pod::text(item, "key")?) {
                Some(bytes) => {
                    out.insert(
                        pod::text(item, "path")?.into(),
                        File {
                            bytes: bytes.clone(),
                            mode: mode(&item["mode"], source.mode)?,
                        },
                    );
                }
                None if source.optional => {}
                None => return Err(invalid("required projected key is missing")),
            }
        }
    }
    paths(out.keys().map(String::as_str))?;
    if out.len() > 1024
        || out
            .values()
            .try_fold(0usize, |n, f| n.checked_add(f.bytes.len()))
            .is_none_or(|n| n > LIMIT)
    {
        return Err(invalid("projected volume exceeds size or file limit"));
    }
    Ok(out)
}
/// A projected volume whose sources are the bound token, the cluster CA and the
/// namespace the API injects for `automountServiceAccountToken`.
async fn projected_files(agent: &Agent, p: &Value, ns: &str, volume: &Value) -> Result<Payload> {
    let mut files = Payload::new();
    for source in array(&volume["projected"]["sources"])? {
        if !source["serviceAccountToken"].is_null() {
            let token = &source["serviceAccountToken"];
            let account = p["spec"]["serviceAccountName"]
                .as_str()
                .unwrap_or("default");
            let issued = inputs::token(
                agent,
                ns,
                account,
                pod::text(&p["metadata"], "name")?,
                pod::text(&p["metadata"], "uid")?,
            )
            .await?;
            files.insert(
                pod::text(token, "path")?.to_owned(),
                File {
                    bytes: issued.into_bytes(),
                    mode: 0o644,
                },
            );
        } else if !source["configMap"].is_null() {
            let config = &source["configMap"];
            let name = pod::text(config, "name")?;
            let optional = config["optional"] == true;
            let data = inputs::data(agent, ns, "configmaps", name, optional, true).await?;
            let projected = Source {
                name: "",
                object: name,
                kind: "configmaps",
                spec: config,
                optional,
                mode: mode(&config["defaultMode"], 0o644)?,
            };
            for (path, file) in payload(&projected, data)? {
                files.insert(path, file);
            }
        } else if !source["downwardAPI"].is_null() {
            files.insert(
                "namespace".into(),
                File {
                    bytes: ns.as_bytes().to_vec(),
                    mode: 0o644,
                },
            );
        } else {
            return Err(invalid("unsupported projected volume source"));
        }
    }
    Ok(files)
}
pub async fn prepare(agent: &Agent, p: &Value, pod_root: &Path) -> Result<()> {
    let sources = sources(p)?;
    let claims = claim_volumes(p)?;
    let automount = p["spec"]["automountServiceAccountToken"] == true;
    if sources.is_empty() && claims.is_empty() && !automount {
        return Ok(());
    }
    let ns = pod::text(&p["metadata"], "namespace")?;
    // Resolve all required inputs before mutating any mounted payload.
    let mut prepared = vec![];
    for source in &sources {
        let data =
            inputs::data(agent, ns, source.kind, source.object, source.optional, true).await?;
        prepared.push((source.name.to_owned(), payload(source, data)?));
    }
    for volume in array(&p["spec"]["volumes"])? {
        if volume["projected"].is_null() {
            continue;
        }
        let files = projected_files(agent, p, ns, volume).await?;
        prepared.push((pod::text(volume, "name")?.to_owned(), files));
    }
    let account = match automount {
        true => Some(
            account_files(
                agent,
                p,
                pod_root,
                ns,
                pod::text(&p["metadata"], "name")?,
                pod::text(&p["metadata"], "uid")?,
            )
            .await?,
        ),
        false => None,
    };
    // The provisioner names each claim's directory; the kubelet owns it here.
    for (_, claim, _) in &claims {
        let path = inputs::claim(agent, ns, claim).await?;
        if path != claim_path(ns, claim) {
            return Err(invalid(
                "only the local-path provisioner's own directory is mounted",
            ));
        }
        real_directory_private(Path::new(&path))?;
    }
    let root = pod_root.join("volumes");
    ram_mount(&root)?;
    for (name, files) in prepared {
        let dir = root.join(&name);
        real_directory(&dir, 0o755)?;
        project(&dir, &files)?;
    }
    if let Some(files) = account {
        let dir = root.join(SERVICE_ACCOUNT_VOLUME);
        real_directory(&dir, 0o755)?;
        project(&dir, &files)?;
    }
    Ok(())
}
/// The ServiceAccount directory every container with automount reaches.
const SERVICE_ACCOUNT_VOLUME: &str = "kube-api-access";
const SERVICE_ACCOUNT_PATH: &str = "/var/run/secrets/kubernetes.io/serviceaccount";
/// Bound tokens are reminted well before the API stops accepting them.
const TOKEN_REFRESH: std::time::Duration = std::time::Duration::from_secs(2400);
/// The directory the API's local-path provisioner hands this claim. `prepare`
/// checks the bound volume publishes exactly this path before mounting it.
fn claim_path(namespace: &str, claim: &str) -> String {
    format!("/var/lib/hedronetes/local-path/{namespace}_{claim}")
}
/// Node-local claim volumes: name, claim name, optional subPath.
fn claim_volumes(p: &Value) -> Result<Vec<(String, String, Option<String>)>> {
    let mut out = vec![];
    for v in array(&p["spec"]["volumes"])? {
        if v["persistentVolumeClaim"].is_null() {
            continue;
        }
        let source = &v["persistentVolumeClaim"];
        out.push((
            pod::text(v, "name")?.to_owned(),
            pod::text(source, "claimName")?.to_owned(),
            source["subPath"]
                .as_str()
                .filter(|s| !s.is_empty())
                .map(str::to_owned),
        ));
    }
    Ok(out)
}
fn stale(path: &Path) -> bool {
    match fs::metadata(path).and_then(|m| m.modified()) {
        Ok(modified) => modified
            .elapsed()
            .map(|age| age >= TOKEN_REFRESH)
            .unwrap_or(true),
        Err(_) => true,
    }
}
/// The projected ServiceAccount files: a bound token, the cluster CA and the
/// namespace. A live projection is reused until it needs reminting.
async fn account_files(
    agent: &Agent,
    p: &Value,
    pod_root: &Path,
    ns: &str,
    name: &str,
    uid: &str,
) -> Result<Payload> {
    let dir = pod_root.join("volumes").join(SERVICE_ACCOUNT_VOLUME);
    let mut files = Payload::new();
    let mut token = None;
    if let Some(generation) = current(&dir)? {
        let path = generation.join("token");
        if !stale(&path) {
            token = Some(fs::read(path)?);
        }
    }
    let token = match token {
        Some(bytes) => bytes,
        None => {
            let account = p["spec"]["serviceAccountName"]
                .as_str()
                .unwrap_or("default");
            inputs::token(agent, ns, account, name, uid)
                .await?
                .into_bytes()
        }
    };
    files.insert(
        "token".into(),
        File {
            bytes: token,
            mode: 0o644,
        },
    );
    files.insert(
        "ca.crt".into(),
        File {
            bytes: agent.ca_pem.clone().into_bytes(),
            mode: 0o644,
        },
    );
    files.insert(
        "namespace".into(),
        File {
            bytes: ns.as_bytes().to_vec(),
            mode: 0o644,
        },
    );
    Ok(files)
}
pub fn mounts(p: &Value, c: &Value, pod_root: &Path) -> Result<Vec<Mount>> {
    let claims = claim_volumes(p)?;
    let ns = pod::text(&p["metadata"], "namespace")?;
    let mut mounts: Vec<Mount> = array(&c["volumeMounts"])?
        .iter()
        .map(|m| {
            let name = pod::text(m, "name")?;
            let (host, readonly) = match claims.iter().find(|(volume, _, _)| volume == name) {
                Some((_, claim, sub_path)) => {
                    let mut host = PathBuf::from(claim_path(ns, claim));
                    if let Some(sub_path) = sub_path {
                        host.push(sub_path);
                    }
                    (host, m["readOnly"] == true)
                }
                None => (pod_root.join("volumes").join(name), true),
            };
            Ok(Mount {
                container_path: pod::text(m, "mountPath")?.into(),
                host_path: host
                    .to_str()
                    .ok_or_else(|| invalid("non UTF-8 volume path"))?
                    .into(),
                readonly,
                ..Default::default()
            })
        })
        .collect::<Result<_>>()?;
    // Every container reaches the bound token, CA and namespace at the
    // documented path unless it already mounts something there.
    if p["spec"]["automountServiceAccountToken"] == true
        && !mounts
            .iter()
            .any(|m| m.container_path == SERVICE_ACCOUNT_PATH)
    {
        mounts.push(Mount {
            container_path: SERVICE_ACCOUNT_PATH.into(),
            host_path: pod_root
                .join("volumes")
                .join(SERVICE_ACCOUNT_VOLUME)
                .to_str()
                .ok_or_else(|| invalid("non UTF-8 volume path"))?
                .into(),
            readonly: true,
            ..Default::default()
        });
    }
    Ok(mounts)
}
/// A claim directory is shared by every Pod on the node: 0777 like the upstream
/// local-path helper, and never a symlink this process did not just create.
fn real_directory_private(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(m) if m.is_dir() => {}
        Ok(_) => return Err(invalid("claim directory is not a real directory")),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir_all(path)?;
        }
        Err(e) => return Err(e.into()),
    }
    fs::set_permissions(path, fs::Permissions::from_mode(0o777))?;
    Ok(())
}
fn real_directory(path: &Path, permissions: u32) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(m) if m.is_dir() => {}
        Ok(_) => return Err(invalid("projection directory is not a real directory")),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir(path)?;
        }
        Err(e) => return Err(e.into()),
    }
    fs::set_permissions(path, fs::Permissions::from_mode(permissions))?;
    Ok(())
}
fn generation(name: &str) -> bool {
    name.strip_prefix(GENERATION)
        .is_some_and(|s| s.len() >= 6 && s.bytes().all(|b| b.is_ascii_alphanumeric()))
}
fn current(dir: &Path) -> Result<Option<PathBuf>> {
    match fs::read_link(dir.join("..data")) {
        Ok(target) if target.to_str().is_some_and(generation) => {
            let path = dir.join(target);
            if !fs::symlink_metadata(&path)?.is_dir() {
                return Err(invalid("invalid projected generation"));
            }
            Ok(Some(path))
        }
        Ok(_) => Err(invalid("invalid projection link")),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}
fn read_tree(root: &Path, dir: &Path, out: &mut Payload, remaining: &mut usize) -> Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        let path = entry.path();
        if kind.is_dir() {
            // Every directory consumes budget too, preventing unbounded traversal.
            *remaining = remaining
                .checked_sub(1)
                .ok_or_else(|| invalid("oversized projection tree"))?;
            read_tree(root, &path, out, remaining)?;
        } else if kind.is_file() {
            let mut file = fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW)
                .open(&path)?;
            let meta = file.metadata()?;
            if meta.len() > *remaining as u64 || out.len() >= 1024 {
                return Err(invalid("oversized projection tree"));
            }
            let mut bytes = vec![];
            (&mut file)
                .take(*remaining as u64 + 1)
                .read_to_end(&mut bytes)?;
            *remaining = remaining
                .checked_sub(bytes.len())
                .ok_or_else(|| invalid("oversized projection tree"))?;
            let name = path
                .strip_prefix(root)
                .ok()
                .and_then(Path::to_str)
                .ok_or_else(|| invalid("invalid projection tree path"))?;
            out.insert(
                name.into(),
                File {
                    bytes,
                    mode: meta.permissions().mode() & 0o777,
                },
            );
        } else {
            return Err(invalid("unexpected object in projection generation"));
        }
    }
    Ok(())
}
fn ensure_link(dir: &Path, name: &str, target: &Path) -> Result<()> {
    let path = dir.join(name);
    match fs::read_link(&path) {
        Ok(existing) if existing == target => Ok(()),
        Ok(_) => Err(invalid("unexpected projection link target")),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            symlink(target, path)?;
            Ok(())
        }
        Err(e) => Err(e.into()),
    }
}
fn project(dir: &Path, files: &Payload) -> Result<()> {
    let old = current(dir)?;
    let mut previous = Payload::new();
    if let Some(old) = &old {
        read_tree(old, old, &mut previous, &mut (LIMIT + 65536))?;
    }
    let changed = old.is_none() || previous != *files;
    if changed {
        let temp = tempfile::Builder::new()
            .prefix(GENERATION)
            .tempdir_in(dir)?;
        fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o755))?;
        for (name, value) in files {
            let path = temp.path().join(name);
            let parent = path
                .parent()
                .ok_or_else(|| invalid("missing projection parent"))?;
            let mut next = temp.path().to_path_buf();
            for part in parent
                .strip_prefix(temp.path())
                .expect("local parent")
                .components()
            {
                next.push(part);
                real_directory(&next, 0o755)?;
            }
            let mut file = fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW)
                .open(path)?;
            file.write_all(&value.bytes)?;
            file.set_permissions(fs::Permissions::from_mode(value.mode))?;
        }
        // Payload is complete before publication. A crash before/after rename
        // leaves either the previous generation or the new one usable.
        let next = temp.keep();
        let target = Path::new(next.file_name().expect("temporary generation"));
        let pending = dir.join("..data_tmp");
        if fs::symlink_metadata(&pending).is_ok() {
            if !fs::symlink_metadata(&pending)?.file_type().is_symlink() {
                return Err(invalid("invalid pending projection link"));
            }
            fs::remove_file(&pending)?;
        }
        symlink(target, &pending)?;
        fs::rename(&pending, dir.join("..data"))?;
    }
    let top: BTreeSet<_> = files
        .keys()
        .map(|p| p.split('/').next().expect("file path"))
        .collect();
    for name in &top {
        ensure_link(dir, name, &Path::new("..data").join(name))?;
    }
    let active = current(dir)?.ok_or_else(|| invalid("missing projection generation"))?;
    let pending = dir.join("..data_tmp");
    if fs::symlink_metadata(&pending).is_ok() {
        if !fs::symlink_metadata(&pending)?.file_type().is_symlink() {
            return Err(invalid("invalid pending projection link"));
        }
        fs::remove_file(pending)?;
    }
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name
            .to_str()
            .ok_or_else(|| invalid("invalid projection entry"))?;
        if generation(name) && entry.path() != active {
            if !entry.file_type()?.is_dir() {
                return Err(invalid("invalid stale generation"));
            }
            fs::remove_dir_all(entry.path())?;
        } else if !name.starts_with("..") && !top.contains(name) {
            if !entry.file_type()?.is_symlink() {
                return Err(invalid("invalid stale projection link"));
            }
            fs::remove_file(entry.path())?;
        }
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn mounted(root: &Path) -> Result<bool> {
    let target = root
        .to_str()
        .ok_or_else(|| invalid("non UTF-8 volume root"))?;
    // mountinfo escapes spaces, tabs, newlines and backslashes in mount paths.
    let escaped = target
        .replace('\\', "\\134")
        .replace(' ', "\\040")
        .replace('\t', "\\011")
        .replace('\n', "\\012");
    for line in fs::read_to_string("/proc/self/mountinfo")?.lines() {
        let Some((before, after)) = line.split_once(" - ") else {
            continue;
        };
        let fields: Vec<_> = before.split_whitespace().collect();
        if fields.get(4) == Some(&escaped.as_str()) {
            if after.split_whitespace().next() != Some("tmpfs")
                || after.split_whitespace().nth(1) != Some("h3s-projections")
                || !fields.get(5).is_some_and(|opts| {
                    ["nosuid", "nodev"]
                        .iter()
                        .all(|flag| opts.split(',').any(|s| s == *flag))
                })
                || !after
                    .split_whitespace()
                    .nth(2)
                    .is_some_and(|opts| opts.split(',').any(|s| s == "noswap"))
            {
                return Err(invalid("projection root requires a private noswap tmpfs"));
            }
            return Ok(true);
        }
    }
    Ok(false)
}
#[cfg(target_os = "linux")]
fn ram_mount(root: &Path) -> Result<()> {
    use std::{ffi::CString, os::unix::ffi::OsStrExt};
    private::directory(root)?;
    if mounted(root)? {
        return Ok(());
    }
    if fs::read_dir(root)?.next().is_some() {
        return Err(invalid("refusing to mount over existing projection files"));
    }
    let target =
        CString::new(root.as_os_str().as_bytes()).map_err(|_| invalid("invalid volume root"))?;
    // SAFETY: NUL-terminated strings remain live for mount; target is an
    // agent-owned real directory, not a Pod-controlled host path.
    let result = unsafe {
        libc::mount(
            c"h3s-projections".as_ptr(),
            target.as_ptr(),
            c"tmpfs".as_ptr(),
            libc::MS_NOSUID | libc::MS_NODEV,
            c"size=40m,nr_inodes=65536,mode=0700,noswap".as_ptr().cast(),
        )
    };
    if result != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    if !mounted(root)? {
        return Err(invalid("projection tmpfs verification failed"));
    }
    Ok(())
}
#[cfg(not(target_os = "linux"))]
fn ram_mount(_root: &Path) -> Result<()> {
    Err(invalid("runtime volume mounts require Linux"))
}
/// Called only after the complete assigned-Pod snapshot and CRI cleanup prove
/// the UID no longer has an owned sandbox. Busy mounts fail; never lazy-unmount.
pub fn cleanup(pod_root: &Path) -> Result<()> {
    let root = pod_root.join("volumes");
    if !root.try_exists()? {
        return Ok(());
    }
    private::directory(&root)?;
    #[cfg(target_os = "linux")]
    if mounted(&root)? {
        use std::{ffi::CString, os::unix::ffi::OsStrExt};
        let target = CString::new(root.as_os_str().as_bytes())
            .map_err(|_| invalid("invalid volume root"))?;
        // SAFETY: verified agent-owned tmpfs, with a live NUL-terminated path.
        if unsafe { libc::umount2(target.as_ptr(), 0) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn fixture() -> Value {
        json!({"metadata":{"name":"web","namespace":"team-a","uid":"3e17c2c0-49e2-4f2b-a917-89da3f986647"},"spec":{"volumes":[
            {"name":"config","configMap":{"name":"settings","defaultMode":292,"items":[{"key":"mode","path":"nested/mode","mode":256}]}},
            {"name":"secret","secret":{"secretName":"credentials"}}
        ],"containers":[{"volumeMounts":[{"name":"config","mountPath":"/etc/project/config"},{"name":"secret","mountPath":"/etc/project/secret","readOnly":false}]}]}})
    }
    fn files(values: &[(&str, &[u8], u32)]) -> Payload {
        values
            .iter()
            .map(|(name, bytes, mode)| {
                (
                    (*name).into(),
                    File {
                        bytes: bytes.to_vec(),
                        mode: *mode,
                    },
                )
            })
            .collect()
    }
    #[test]
    fn admitted_volumes_become_private_readonly_cri_mounts() {
        let p = fixture();
        let mounts = mounts(&p, &p["spec"]["containers"][0], Path::new("/private/uid")).unwrap();
        assert_eq!(mounts[0].host_path, "/private/uid/volumes/config");
        assert_eq!(mounts[0].container_path, "/etc/project/config");
        assert!(mounts.iter().all(|m| m.readonly));
        let parsed = sources(&p).unwrap();
        assert_eq!(parsed.len(), 2);
        assert_eq!(
            (parsed[0].kind, parsed[0].object, parsed[0].mode),
            ("configmaps", "settings", 292)
        );
        assert_eq!(
            (parsed[1].kind, parsed[1].object, parsed[1].mode),
            ("secrets", "credentials", 0o644)
        );
    }
    /// KP-22: a bound claim mounts the node-local directory the API's
    /// provisioner published, read-write unless the Pod says otherwise.
    #[test]
    fn bound_claims_mount_the_provisioned_directory_not_a_projection() {
        let mut p = fixture();
        p["spec"]["volumes"] = json!([
            {"name":"data","persistentVolumeClaim":{"claimName":"web-data"}},
            {"name":"config","configMap":{"name":"settings"}}
        ]);
        p["spec"]["containers"][0]["volumeMounts"] = json!([
            {"name":"data","mountPath":"/var/data"},
            {"name":"data","mountPath":"/var/ro","readOnly":true},
            {"name":"config","mountPath":"/etc/project/config"}
        ]);
        // A claim is not a projection: it contributes no ConfigMap/Secret source.
        assert_eq!(sources(&p).unwrap().len(), 1);
        let cri = mounts(&p, &p["spec"]["containers"][0], Path::new("/private/uid")).unwrap();
        assert_eq!(
            cri[0].host_path,
            "/var/lib/hedronetes/local-path/team-a_web-data"
        );
        assert!(!cri[0].readonly);
        assert!(cri[1].readonly);
        assert_eq!(cri[2].host_path, "/private/uid/volumes/config");
        // The claim's subPath is part of the host path.
        p["spec"]["volumes"][0]["persistentVolumeClaim"]["subPath"] = json!("nested");
        assert_eq!(
            mounts(&p, &p["spec"]["containers"][0], Path::new("/private/uid")).unwrap()[0]
                .host_path,
            "/var/lib/hedronetes/local-path/team-a_web-data/nested"
        );
    }
    #[test]
    fn item_selection_optional_keys_binary_bytes_and_limits() {
        let mut p = fixture();
        let data = BTreeMap::from([
            ("mode".into(), vec![0, 255, 10]),
            ("unused".into(), b"omit".to_vec()),
        ]);
        let s = sources(&p).unwrap();
        let selected = payload(&s[0], data.clone()).unwrap();
        assert_eq!(selected.len(), 1);
        assert_eq!(selected["nested/mode"].bytes, [0, 255, 10]);
        assert_eq!(selected["nested/mode"].mode, 0o400);
        assert!(payload(&s[0], BTreeMap::new()).is_err());
        p["spec"]["volumes"][0]["configMap"]["optional"] = json!(true);
        assert!(payload(&sources(&p).unwrap()[0], BTreeMap::new())
            .unwrap()
            .is_empty());
        p["spec"]["volumes"][0]["configMap"]["items"] = json!([]);
        let all = payload(&sources(&p).unwrap()[0], data).unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all["mode"].mode, 0o444);
        assert!(payload(
            &sources(&p).unwrap()[0],
            BTreeMap::from([("mode".into(), vec![0; LIMIT + 1])])
        )
        .is_err());
        assert!(payload(
            &sources(&p).unwrap()[0],
            BTreeMap::from([("../escape".into(), vec![])])
        )
        .is_err());
    }
    #[test]
    fn atomic_projection_updates_removes_and_recovers_without_restart_state() {
        let root = tempfile::tempdir().unwrap();
        let first = files(&[
            ("nested/value", b"first\0binary", 0o444),
            ("remove", b"old", 0o644),
        ]);
        project(root.path(), &first).unwrap();
        let old = current(root.path()).unwrap().unwrap();
        let mut held = fs::File::open(root.path().join("nested/value")).unwrap();
        assert_eq!(
            fs::metadata(root.path().join("nested/value"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o444
        );
        project(root.path(), &first).unwrap();
        assert_eq!(
            current(root.path()).unwrap().unwrap(),
            old,
            "unchanged restart must preserve generation"
        );
        let second = files(&[
            ("nested/value", b"second", 0o440),
            ("added/bytes", &[0, 255, 1], 0o444),
        ]);
        project(root.path(), &second).unwrap();
        assert_ne!(current(root.path()).unwrap().unwrap(), old);
        assert!(!old.exists());
        assert!(!root.path().join("remove").try_exists().unwrap());
        assert_eq!(
            fs::read(root.path().join("nested/value")).unwrap(),
            b"second"
        );
        assert_eq!(
            fs::read(root.path().join("added/bytes")).unwrap(),
            [0, 255, 1]
        );
        let mut bytes = vec![];
        held.read_to_end(&mut bytes).unwrap();
        assert_eq!(
            bytes, b"first\0binary",
            "already-open file retains old inode"
        );
        // Crash after data-link publication but before user-link reconciliation.
        fs::remove_file(root.path().join("added")).unwrap();
        let stale = tempfile::Builder::new()
            .prefix(GENERATION)
            .tempdir_in(root.path())
            .unwrap()
            .keep();
        symlink(stale.file_name().unwrap(), root.path().join("..data_tmp")).unwrap();
        project(root.path(), &second).unwrap();
        assert_eq!(
            fs::read(root.path().join("added/bytes")).unwrap(),
            [0, 255, 1]
        );
        assert!(!stale.exists());
        assert!(fs::symlink_metadata(root.path().join("..data_tmp")).is_err());
        project(root.path(), &Payload::new()).unwrap();
        assert!(!root.path().join("nested").exists());
        assert!(!root.path().join("added").exists());
    }
    #[test]
    fn projection_never_follows_an_injected_host_symlink() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let target = outside.path().join("keep");
        fs::write(&target, b"untouched").unwrap();
        symlink(outside.path(), root.path().join("..data")).unwrap();
        assert!(project(root.path(), &files(&[("keep", b"replace", 0o644)])).is_err());
        assert_eq!(fs::read(&target).unwrap(), b"untouched");
        fs::remove_file(root.path().join("..data")).unwrap();
        project(root.path(), &files(&[("keep", b"local", 0o644)])).unwrap();
        let active = current(root.path()).unwrap().unwrap();
        fs::remove_file(active.join("keep")).unwrap();
        symlink(&target, active.join("keep")).unwrap();
        assert!(project(root.path(), &files(&[("keep", b"replace", 0o644)])).is_err());
        assert_eq!(fs::read(&target).unwrap(), b"untouched");
    }
}
