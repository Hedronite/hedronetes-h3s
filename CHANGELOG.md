# Changelog

All notable changes to h3s are recorded here. The version is the git tag,
the Cargo workspace version, and `h3s --version`.

## 0.11.1 — 2026-10-02

Compatible additions on 0.x. Not HA. Not conformance. Not `v1.0.0`.

### Added

- `--store=postgres` with `--datastore-endpoint` talks to one Postgres primary. Two `h3s server` processes share that primary. SQLite stays the one-server default. `PostgresStore` shipped on `174367d34e643b78a3e5a654b02e460de5ada3bf`. The server flag shipped on `512f220de42ed989f937ad7625a7de732247797b` (PR #62, merge `8bae4b55c48e0e46303a1e01310713b551970f2a`).
- `h3s token rotate` replaces `server/node-token` with one new token. It refuses a held registry lock or token lock, prints the secret once, and does not log it. After the next `h3s server` start the old token fails and the new token passes. No TTL.
- `h3s backup` and `h3s restore` copy the SQLite registry, its wal and shm files when present, `server/tls/`, `server/ca.crt`, and `server/node-token`. Not containerd. Graded on `e7e3d467b478a875bb7df818ef67bb38ed1d5c7a`, merged `e45c1764e49ccf4e5da61dd060f77ee6e74002d6` (PR #66).

### Changed

- The documented SQLite backup is the offline `h3s backup` / `h3s restore` pair. The upgrade sentence is written only: stop, replace the `h3s` binary, start with the same data dir and the same flags. Graded on `beddf9460e37cc20c3fb9263a7d125d1a65f5d36`, merged `571247a46ac0d371fa7af0dee6324cd1621eae8a` (PR #67).

### Not in this release

No leader election, no automatic failover, no read replica, no `h3s etcd-snapshot`, no etcd, MySQL, or Xline, no Kubernetes conformance, and no crates.io upload.

## 0.11.0 — 2026-10-01

Server-Side Apply, opt-in Geode secrets encryption, and default-on Traefik and ServiceLB. Tag `v0.11.0` at `ffd9b36e0253b42db297afdaa357902963a9d8da`. Not HA.
