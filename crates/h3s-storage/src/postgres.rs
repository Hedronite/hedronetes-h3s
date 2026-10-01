//! Postgres registry: the same MVCC contract as the SQLite backend, written
//! against a real server so more than one h3s server can share one registry.
//!
//! Two tables carry it. `registry_meta` holds the revision and compaction floor
//! as one row; `registry_versions` is append-only and IS the event log - every
//! write inserts a row and takes the next revision from the meta row in the
//! same transaction, so a watch replays from that log and never from a
//! notification. `LISTEN/NOTIFY` only wakes a watcher early; missing a
//! notification costs latency, never an event.
use std::{
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use async_trait::async_trait;
use tokio_postgres::{Client, NoTls, Transaction};

use crate::*;

/// Geode custody envelope marker; mirrors `geode::ENVELOPE`.
const GEO_ENVELOPE: &[u8] = b"H3SGEO1:";

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS registry_meta (
    singleton BOOLEAN PRIMARY KEY DEFAULT TRUE CHECK (singleton),
    revision BIGINT NOT NULL,
    compacted BIGINT NOT NULL);
INSERT INTO registry_meta (singleton, revision, compacted) VALUES (TRUE, 0, 0)
    ON CONFLICT (singleton) DO NOTHING;
CREATE TABLE IF NOT EXISTS registry_versions (
    key TEXT NOT NULL,
    revision BIGINT NOT NULL UNIQUE,
    value BYTEA NOT NULL,
    kind SMALLINT NOT NULL CHECK (kind BETWEEN 0 AND 2),
    PRIMARY KEY (key, revision));
CREATE TABLE IF NOT EXISTS registry_leases (
    id BIGSERIAL PRIMARY KEY,
    ttl_ms BIGINT NOT NULL,
    expires_ms BIGINT NOT NULL);
CREATE INDEX IF NOT EXISTS registry_versions_revision ON registry_versions (revision);
";

/// One owned connection. A write takes the meta row and the event insert in
/// one transaction; the revision is assigned by that transaction, never by the
/// caller.
#[derive(Clone)]
pub struct PostgresStore {
    client: Arc<tokio::sync::Mutex<Client>>,
    secrets: Arc<dyn SecretsSealer>,
}

impl PostgresStore {
    /// Connect to the registry named by a `postgres://` URL.
    pub async fn open(url: &str) -> Result<Self> {
        let (client, connection) = tokio_postgres::connect(url, NoTls)
            .await
            .map_err(|e| Error::Worker(format!("postgres connect: {e}")))?;
        tokio::spawn(async move {
            if let Err(error) = connection.await {
                eprintln!("postgres registry connection ended: {error}");
            }
        });
        let store = Self {
            client: Arc::new(tokio::sync::Mutex::new(client)),
            secrets: Arc::new(Unsealed),
        };
        store.schema().await?;
        Ok(store)
    }

    /// Run Secret payloads through custody (Geode) in addition to the registry.
    /// Rows written without an envelope stay readable.
    pub fn with_secrets_sealer(mut self, sealer: impl SecretsSealer) -> Self {
        self.secrets = Arc::new(sealer);
        self
    }

    async fn schema(&self) -> Result<()> {
        self.client
            .lock()
            .await
            .batch_execute(SCHEMA)
            .await
            .map_err(db)?;
        Ok(())
    }

    fn seal(&self, key: &StoreKey, value: &mut Vec<u8>) -> Result<()> {
        if is_secret_key(key.as_str()) {
            *value = self.secrets.seal(value)?;
        }
        Ok(())
    }
}

fn db(error: tokio_postgres::Error) -> Error {
    // The server's own message is what names a constraint or a deadlock.
    match error.as_db_error() {
        Some(server) => Error::Worker(format!(
            "postgres registry: {} ({})",
            server.message(),
            server.code().code()
        )),
        None => Error::Worker(format!("postgres registry: {error}")),
    }
}
fn is_secret_key(key: &str) -> bool {
    key.strip_prefix("/registry/")
        .is_some_and(|tail| tail.split('/').next() == Some("secrets"))
}
fn open_row_value(sealer: &Arc<dyn SecretsSealer>, value: Vec<u8>) -> Result<Vec<u8>> {
    if value.starts_with(GEO_ENVELOPE) {
        return sealer
            .open(&value)
            .map_err(|e| Error::Custody(e.to_string()));
    }
    Ok(value)
}
fn revision_of(value: i64) -> Result<ResourceVersion> {
    u64::try_from(value).map_err(|_| Error::SchemaVersion(value))
}
fn revision_i64(rev: ResourceVersion) -> Result<i64> {
    i64::try_from(rev).map_err(|_| Error::Invalid("revision exceeds backend range".into()))
}
fn validate_revision(
    rev: ResourceVersion,
    current: ResourceVersion,
    floor: ResourceVersion,
) -> Result<()> {
    if rev < floor {
        return Err(Error::Compacted {
            requested: rev,
            floor,
        });
    }
    if rev > current {
        return Err(Error::FutureRevision {
            requested: rev,
            current,
        });
    }
    Ok(())
}

#[async_trait]
impl Storage for PostgresStore {
    async fn get(&self, key: &StoreKey) -> Result<Option<StoredObject>> {
        let guard = self.client.lock().await;
        let row = guard
            .query_opt(
                "SELECT revision, value, kind FROM registry_versions
                 WHERE key = $1 ORDER BY revision DESC LIMIT 1",
                &[&key.as_str()],
            )
            .await
            .map_err(db)?;
        let Some(row) = row else { return Ok(None) };
        let kind: i16 = row.get(2);
        if kind == 2 {
            return Ok(None);
        }
        let revision = revision_of(row.get(0))?;
        let value = open_row_value(&self.secrets, row.get(1))?;
        Ok(Some(StoredObject {
            key: key.clone(),
            revision,
            value,
        }))
    }

    async fn list(&self, sel: ListSelect) -> Result<ObjectList> {
        validate_prefix(&sel.prefix)?;
        if sel.limit == 0 || sel.limit > 4096 {
            return Err(Error::Invalid("LIST limit must be 1..4096".into()));
        }
        if sel
            .start_after
            .as_ref()
            .is_some_and(|key| !key.as_str().starts_with(&sel.prefix))
        {
            return Err(Error::Invalid(
                "continuation key is outside the selection".into(),
            ));
        }
        let mut guard = self.client.lock().await;
        let tx = guard.transaction().await.map_err(db)?;
        let (current, floor) = head(&tx).await?;
        let revision = sel.at_revision.filter(|r| *r != 0).unwrap_or(current);
        validate_revision(revision, current, floor)?;
        // One snapshot for the page: each key at its newest revision that is
        // not newer than the revision the page was taken at.
        let rows = tx
            .query(
                "SELECT v.key, v.revision, v.value FROM registry_versions v
                 WHERE left(v.key, length($1)) = $1 AND v.key > $2 AND v.kind <> 2
                 AND v.revision = (SELECT max(p.revision) FROM registry_versions p
                     WHERE p.key = v.key AND p.revision <= $3)
                 ORDER BY v.key LIMIT $4",
                &[
                    &sel.prefix,
                    &sel.start_after.as_ref().map(StoreKey::as_str).unwrap_or(""),
                    &revision_i64(revision)?,
                    &((sel.limit + 1) as i64),
                ],
            )
            .await
            .map_err(db)?;
        drop(tx);
        let mut items = rows
            .into_iter()
            .map(|row| {
                Ok(StoredObject {
                    key: StoreKey(row.get(0)),
                    revision: revision_of(row.get(1))?,
                    value: open_row_value(&self.secrets, row.get(2))?,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let next_after = if items.len() > sel.limit {
            items.pop();
            items.last().map(|object| object.key.clone())
        } else {
            None
        };
        Ok(ObjectList {
            items,
            revision,
            next_after,
        })
    }

    async fn create(&self, obj: StoredObject) -> Result<StoredObject> {
        self.mutate(obj, None, false).await
    }
    async fn update(&self, obj: StoredObject, rv: ResourceVersion) -> Result<StoredObject> {
        self.mutate(obj, Some(rv), false).await
    }
    async fn delete(&self, key: &StoreKey, rv: ResourceVersion) -> Result<()> {
        self.mutate(
            StoredObject {
                key: key.clone(),
                value: Vec::new(),
                revision: 0,
            },
            Some(rv),
            true,
        )
        .await?;
        Ok(())
    }

    async fn watch(&self, sel: WatchSelect) -> Result<WatchStream> {
        validate_prefix(&sel.prefix)?;
        if sel.bookmark_interval.is_zero() {
            return Err(Error::Invalid("bookmark interval must be positive".into()));
        }
        let requested = sel.after_revision.filter(|r| *r != 0);
        let anchor = {
            let mut guard = self.client.lock().await;
            let tx = guard.transaction().await.map_err(db)?;
            let (current, floor) = head(&tx).await?;
            let revision = requested.unwrap_or(current);
            validate_revision(revision, current, floor)?;
            revision
        };
        let store = self.clone();
        Ok(Box::pin(async_stream::try_stream! {
            if requested.is_none() {
                let mut next = None;
                // Every page of the initial replay is read at the revision of
                // the first page, so the replay is one consistent snapshot.
                let mut snapshot = None;
                loop {
                    let page = store
                        .list(ListSelect {
                            prefix: sel.prefix.clone(),
                            at_revision: Some(anchor),
                            start_after: next.clone(),
                            limit: 256,
                        })
                        .await?;
                    snapshot.get_or_insert(page.revision);
                    for object in page.items {
                        yield WatchEvent {
                            previous: None,
                            kind: EventKind::Added,
                            revision: object.revision,
                            object: Some(object),
                        };
                    }
                    match page.next_after {
                        Some(cursor_key) => next = Some(cursor_key),
                        None => break,
                    }
                }
                yield WatchEvent {
                    previous: None,
                    kind: EventKind::Bookmark,
                    revision: snapshot.unwrap_or(anchor),
                    object: None,
                };
            }
            let mut cursor = anchor;
            let mut bookmark = tokio::time::Instant::now();
            loop {
                let (events, next) = store.changes(sel.prefix.clone(), cursor).await?;
                let full = events.len() == 256;
                for event in events {
                    yield event;
                }
                cursor = next;
                if bookmark.elapsed() >= sel.bookmark_interval {
                    yield WatchEvent {
                        previous: None,
                        kind: EventKind::Bookmark,
                        revision: cursor,
                        object: None,
                    };
                    bookmark = tokio::time::Instant::now();
                }
                if !full {
                    // A notification only shortens this wait; the log above is
                    // what a watcher resumes from, so a missed wake is latency.
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            }
        }))
    }

    async fn compact(&self, rev: ResourceVersion) -> Result<()> {
        let mut guard = self.client.lock().await;
        let tx = guard.transaction().await.map_err(db)?;
        let (current, floor) = head(&tx).await?;
        if rev > current {
            return Err(Error::FutureRevision {
                requested: rev,
                current,
            });
        }
        if rev <= floor {
            return Ok(());
        }
        // Retain each key's baseline at the floor, plus all newer changes.
        tx.execute(
            "DELETE FROM registry_versions WHERE revision < $1 AND revision NOT IN
             (SELECT max(revision) FROM registry_versions WHERE revision <= $1 GROUP BY key)",
            &[&revision_i64(rev)?],
        )
        .await
        .map_err(db)?;
        tx.execute(
            "UPDATE registry_meta SET compacted = $1 WHERE singleton",
            &[&revision_i64(rev)?],
        )
        .await
        .map_err(db)?;
        tx.commit().await.map_err(db)?;
        Ok(())
    }

    async fn lease_grant(&self, ttl: Duration) -> Result<Lease> {
        let ttl_ms = i64::try_from(ttl.as_millis())
            .map_err(|_| Error::Invalid("lease TTL too large".into()))?;
        if ttl_ms <= 0 {
            return Err(Error::Invalid("lease TTL must be at least 1ms".into()));
        }
        let now = now_ms()?;
        let expires = now
            .checked_add(ttl_ms)
            .ok_or_else(|| Error::Invalid("lease expiry overflow".into()))?;
        let guard = self.client.lock().await;
        let row = guard
            .query_one(
                "INSERT INTO registry_leases (ttl_ms, expires_ms) VALUES ($1, $2) RETURNING id",
                &[&ttl_ms, &expires],
            )
            .await
            .map_err(db)?;
        let id: i64 = row.get(0);
        Ok(Lease {
            id: u64::try_from(id).map_err(|_| Error::SchemaVersion(id))?,
            ttl,
        })
    }

    async fn lease_keepalive(&self, id: LeaseId) -> Result<()> {
        let now = now_ms()?;
        let guard = self.client.lock().await;
        let changed = guard
            .execute(
                "UPDATE registry_leases SET expires_ms = $1 + ttl_ms
                 WHERE id = $2 AND expires_ms > $1",
                &[&now, &revision_i64(id)?],
            )
            .await
            .map_err(db)?;
        if changed == 0 {
            return Err(Error::LeaseExpired(id));
        }
        Ok(())
    }
}

impl PostgresStore {
    /// One write: the revision is taken from the meta row and the event row is
    /// inserted in the same transaction, so a concurrent writer can neither
    /// reuse a revision nor lose an event.
    async fn mutate(
        &self,
        mut obj: StoredObject,
        expected: Option<ResourceVersion>,
        delete: bool,
    ) -> Result<StoredObject> {
        let mut guard = self.client.lock().await;
        let tx = guard.transaction().await.map_err(db)?;
        let previous = current(&tx, &obj.key).await?;
        match (expected, previous.as_ref()) {
            (None, Some(_)) => return Err(Error::AlreadyExists(obj.key)),
            (Some(_), None) => return Err(Error::NotFound(obj.key)),
            (Some(expected), Some(old)) if expected != old.revision => {
                return Err(Error::Conflict {
                    expected,
                    actual: old.revision,
                })
            }
            _ => {}
        }
        let kind = if delete {
            EventKind::Deleted
        } else if previous.is_some() {
            EventKind::Modified
        } else {
            EventKind::Added
        };
        if delete {
            obj.value = previous.expect("validated existing object").value;
        } else {
            self.seal(&obj.key, &mut obj.value)?;
        }
        let row = tx
            .query_one(
                "UPDATE registry_meta SET revision = revision + 1 WHERE singleton RETURNING revision",
                &[],
            )
            .await
            .map_err(db)?;
        let new_revision = revision_of(row.get(0))?;
        let returned = if delete {
            obj.value.clone()
        } else {
            open_row_value(&self.secrets, obj.value.clone())?
        };
        tx.execute(
            "INSERT INTO registry_versions (key, revision, value, kind) VALUES ($1, $2, $3, $4)",
            &[
                &obj.key.as_str(),
                &revision_i64(new_revision)?,
                &obj.value,
                &kind_number(kind),
            ],
        )
        .await
        .map_err(db)?;
        tx.commit().await.map_err(db)?;
        Ok(StoredObject {
            revision: new_revision,
            value: returned,
            ..obj
        })
    }

    /// Changes strictly after `after`, with each object's previous live version
    /// read in the same snapshot, bounded to one batch.
    async fn changes(
        &self,
        prefix: String,
        after: ResourceVersion,
    ) -> Result<(Vec<WatchEvent>, ResourceVersion)> {
        let mut guard = self.client.lock().await;
        let tx = guard.transaction().await.map_err(db)?;
        let (current, floor) = head(&tx).await?;
        validate_revision(after, current, floor)?;
        let rows = tx
            .query(
                "SELECT v.key, v.revision, v.value, v.kind, p.revision, p.value, p.kind
                 FROM registry_versions v
                 LEFT JOIN registry_versions p ON p.key = v.key AND p.revision = (
                     SELECT max(prior.revision) FROM registry_versions prior
                     WHERE prior.key = v.key AND prior.revision < v.revision)
                 WHERE v.revision > $1 AND left(v.key, length($2)) = $2
                 ORDER BY v.revision LIMIT 256",
                &[&revision_i64(after)?, &prefix],
            )
            .await
            .map_err(db)?;
        drop(tx);
        let mut events = Vec::with_capacity(rows.len());
        for row in rows {
            let key: String = row.get(0);
            let revision = revision_of(row.get(1))?;
            let kind = match row.get::<_, i16>(3) {
                0 => EventKind::Added,
                1 => EventKind::Modified,
                _ => EventKind::Deleted,
            };
            let value = open_row_value(&self.secrets, row.get(2))?;
            let previous = match (row.get::<_, Option<i16>>(6), row.get::<_, Option<i64>>(4)) {
                (Some(0 | 1), Some(previous_revision)) => Some(StoredObject {
                    key: StoreKey(key.clone()),
                    revision: revision_of(previous_revision)?,
                    value: open_row_value(&self.secrets, row.get::<_, Vec<u8>>(5))?,
                }),
                _ => None,
            };
            events.push(WatchEvent {
                previous,
                kind,
                revision,
                object: Some(StoredObject {
                    key: StoreKey(key),
                    revision,
                    value,
                }),
            });
        }
        let next = if events.len() == 256 {
            events.last().expect("full batch").revision
        } else {
            current
        };
        Ok((events, next))
    }
}

async fn head(tx: &Transaction<'_>) -> Result<(ResourceVersion, ResourceVersion)> {
    let row = tx
        .query_one(
            "SELECT revision, compacted FROM registry_meta WHERE singleton",
            &[],
        )
        .await
        .map_err(db)?;
    Ok((revision_of(row.get(0))?, revision_of(row.get(1))?))
}
async fn current(tx: &Transaction<'_>, key: &StoreKey) -> Result<Option<StoredObject>> {
    let row = tx
        .query_opt(
            "SELECT revision, value, kind FROM registry_versions
             WHERE key = $1 ORDER BY revision DESC LIMIT 1",
            &[&key.as_str()],
        )
        .await
        .map_err(db)?;
    let Some(row) = row else { return Ok(None) };
    let kind: i16 = row.get(2);
    Ok((kind != 2).then(|| StoredObject {
        key: key.clone(),
        revision: revision_of(row.get(0)).expect("stored revision"),
        value: row.get(1),
    }))
}
fn kind_number(k: EventKind) -> i16 {
    match k {
        EventKind::Added => 0,
        EventKind::Modified => 1,
        EventKind::Deleted => 2,
        EventKind::Bookmark => unreachable!(),
    }
}
fn now_ms() -> Result<i64> {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| Error::Worker(e.to_string()))?;
    i64::try_from(elapsed.as_millis())
        .map_err(|_| Error::Invalid("clock exceeds backend range".into()))
}
#[cfg(test)]
mod live_tests {
    use super::*;
    use futures_util::StreamExt;
    use std::time::Duration;

    const SECRET: &str = "h3s-postgres-secret-plaintext";
    const GEODE: &str = "/opt/homebrew/bin/geode";

    fn url() -> String {
        std::env::var("H3S_POSTGRES_URL").expect("H3S_POSTGRES_URL names the throwaway primary")
    }
    fn object(key: &str, value: &str) -> StoredObject {
        StoredObject {
            key: StoreKey::new(key).unwrap(),
            value: value.as_bytes().to_vec(),
            revision: 0,
        }
    }
    /// A key nothing else in this registry uses, so runs cannot collide.
    fn unique(name: &str) -> String {
        format!(
            "/registry/configmaps/default/{}-{}",
            name,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        )
    }
    /// Start from an empty registry. These tests own the throwaway primary,
    /// and the command that runs them is serial.
    async fn reset() {
        let (client, connection) = tokio_postgres::connect(&url(), NoTls).await.unwrap();
        tokio::spawn(async move {
            let _ = connection.await;
        });
        client
            .batch_execute(
                "TRUNCATE registry_versions; TRUNCATE registry_leases;
                 UPDATE registry_meta SET revision = 0, compacted = 0 WHERE singleton;",
            )
            .await
            .unwrap();
    }
    /// The row exactly as the database holds it, bypassing the store.
    async fn raw(key: &str) -> Option<Vec<u8>> {
        let (client, connection) = tokio_postgres::connect(&url(), NoTls).await.unwrap();
        tokio::spawn(async move {
            let _ = connection.await;
        });
        client
            .query_opt(
                "SELECT value FROM registry_versions WHERE key = $1 ORDER BY revision DESC LIMIT 1",
                &[&key],
            )
            .await
            .unwrap()
            .map(|row| row.get(0))
    }

    /// The bar: writes assign the revision, a stale update conflicts, a watch
    /// resumes after a revision, and compaction fails an older watch.
    #[tokio::test]
    #[ignore = "needs a live Postgres; run on tower with H3S_POSTGRES_URL"]
    async fn revisions_conflicts_resumed_watch_and_compaction() {
        reset().await;
        let store = PostgresStore::open(&url()).await.unwrap();
        let key = StoreKey::new(unique("postgres")).unwrap();
        let first = store.create(object(key.as_str(), "one")).await.unwrap();
        assert!(first.revision > 0, "the write assigns the revision");
        assert_eq!(first.value, b"one".to_vec());
        let second = store
            .update(object(key.as_str(), "two"), first.revision)
            .await
            .unwrap();
        assert_eq!(second.revision, first.revision + 1, "the revision advances");
        let conflict = store
            .update(object(key.as_str(), "three"), first.revision)
            .await
            .unwrap_err();
        assert!(
            matches!(conflict, Error::Conflict { expected, actual } if expected == first.revision && actual == second.revision),
            "{conflict}"
        );
        assert_eq!(
            store.get(&key).await.unwrap().unwrap().value,
            b"two".to_vec()
        );
        // A watch resumes after a revision and reads the log, not a wake.
        let mut watch = store
            .watch(WatchSelect::new(
                "/registry/configmaps/".to_owned(),
                Some(second.revision),
            ))
            .await
            .unwrap();
        let updated = store
            .update(object(key.as_str(), "four"), second.revision)
            .await
            .unwrap();
        let event = tokio::time::timeout(Duration::from_secs(10), async {
            while let Some(item) = watch.next().await {
                let event = item.expect("a stream item");
                if event.object.as_ref().is_some_and(|o| o.key == key) {
                    return event;
                }
            }
            panic!("the watch ended before this key's revision");
        })
        .await
        .expect("a resumed watch delivers the next revision");
        assert_eq!(event.kind, EventKind::Modified);
        assert_eq!(event.revision, updated.revision);
        assert_eq!(event.object.unwrap().value, b"four".to_vec());
        assert_eq!(event.previous.unwrap().value, b"two".to_vec());
        // Compaction keeps a floor; a watch below it fails with the existing
        // compaction error rather than replaying silently.
        store.compact(updated.revision).await.unwrap();
        // Zero means "replay from the start", so ask for a real revision below
        // the floor: the first write of this run.
        let below = first.revision;
        let stale = store
            .watch(WatchSelect::new(
                "/registry/configmaps/".to_owned(),
                Some(below),
            ))
            .await;
        let failure = match stale {
            Err(failure) => failure,
            Ok(mut stream) => stream.next().await.expect("an item").unwrap_err(),
        };
        let Error::Compacted { requested, floor } = failure else {
            panic!("expected a compaction failure, got {failure}");
        };
        assert_eq!(requested, below);
        assert!(floor > below, "the floor moved past the request: {floor}");
        assert_eq!(
            Error::Compacted { requested, floor }.to_string(),
            format!("revision {requested} is older than compaction floor {floor}")
        );
    }

    /// A sealed Secret: SQL holds an envelope, `open` returns the plaintext.
    #[tokio::test]
    #[ignore = "needs a live Postgres and the tower geode binary"]
    async fn a_secret_is_sealed_at_rest_and_opened_through_the_store() {
        reset().await;
        let dir = tempfile::tempdir().unwrap();
        let sealer = GeodeSealer::create(GEODE, dir.path()).unwrap();
        let store = PostgresStore::open(&url())
            .await
            .unwrap()
            .with_secrets_sealer(sealer);
        let key = StoreKey::new(unique("db").replace("configmaps", "secrets")).unwrap();
        let stored = store.create(object(key.as_str(), SECRET)).await.unwrap();
        assert_eq!(stored.value, SECRET.as_bytes());
        let row = raw(key.as_str()).await.expect("the row exists");
        let human = String::from_utf8_lossy(&row);
        assert!(
            !human.contains(SECRET),
            "plaintext found in the row: {human}"
        );
        assert!(row.starts_with(b"H3SGEO1:"), "{human}");
        assert_eq!(
            store.get(&key).await.unwrap().unwrap().value,
            SECRET.as_bytes()
        );
        let listed = store
            .list(ListSelect::new("/registry/secrets/".to_owned()))
            .await
            .unwrap();
        assert!(
            listed
                .items
                .iter()
                .any(|item| item.value == SECRET.as_bytes()),
            "the store lists the opened plaintext"
        );
    }

    /// Two servers on one registry share the revision sequence; one of two
    /// concurrent writers loses the compare-and-set.
    #[tokio::test]
    #[ignore = "needs a live Postgres"]
    async fn two_connections_share_one_revision_sequence() {
        reset().await;
        let first = PostgresStore::open(&url()).await.unwrap();
        let second = PostgresStore::open(&url()).await.unwrap();
        let key = StoreKey::new(unique("shared")).unwrap();
        let created = first.create(object(key.as_str(), "one")).await.unwrap();
        let winner = second
            .update(object(key.as_str(), "two"), created.revision)
            .await
            .unwrap();
        assert!(winner.revision > created.revision, "the revision advances");
        let loser = first
            .update(object(key.as_str(), "three"), created.revision)
            .await
            .unwrap_err();
        assert!(
            matches!(loser, Error::Conflict { actual, .. } if actual == winner.revision),
            "{loser}"
        );
    }
}
