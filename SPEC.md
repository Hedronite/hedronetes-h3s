# Hedronetes (h3s)

**A Kubernetes-compatible cluster distribution in one Rust binary.**

Version 0.9.0 · M1 · 9 September 2026 · Apache-2.0

Tagline: *k3s, written in Rust, without embedding a Go control plane.*

---

## 0. How to read this document

This is the product specification for Hedronetes. It is intended to be detailed
enough to stand up a Cargo workspace and reject the wrong design in review.

Normative language: **MUST**, **SHOULD**, **MAY**, **MUST NOT**.

Comparison targets are explicit:

| Compare against | Why |
|---|---|
| **k3s** | The product we are building. Fair benchmark for packaging, UX, and conformance. |
| **kubeadm Kubernetes** | What we are *not* trying to be. |
| **Rūsternetes / rk8s / Krustlet** | Reference designs. Patterns, not code we vendor blindly. |

h3s does not claim Rūsternetes’ Sonobuoy percentage, Anvil proofs, or Xline
maturity as shipped defaults. Those are optional later integrations.

---

## 1. Charter

Hedronetes (short name **h3s**, binary `h3s`) is a CNCF-conformant-intent
Kubernetes distribution written in Rust.

It copies k3s’s *product shape*:

- one statically linked binary
- `server` and `agent` personalities
- SQLite for one server (shipped default); Postgres for more than one server (shipped on `512f220de42ed989f937ad7625a7de732247797b`, driver `tokio-postgres` shipped on `174367d34e643b78a3e5a654b02e460de5ada3bf`); etcd, MySQL, and Xline not implemented
- batteries included and `--disable=`-able
- edge, homelab, CI, air-gap first
- stock `kubectl`, Helm, and YAML

It does **not** copy k3s’s implementation shape. k3s embeds upstream Go
Kubernetes as goroutines inside a launcher. h3s reimplements the control plane
and node agent as native Tokio tasks, with a typed kubelet and a pluggable
store, and *depends* on the existing Rust ecosystem rather than reinventing it.

### 1.1 Why this exists

k3s proved the product. The remaining gaps it cannot close without leaving Go:

- five logical components still means five Go heaps, even in one process
- the kubelet is still the implicit phase machine from upstream
- the OCI runtime is still runc (Go + C for `fork` / namespaces)
- LIST/watch memory cliffs are a documented `kube-apiserver` failure mode
- reconcile liveness cannot be machine-checked in the Go controllers

h3s exists to ship k3s’s UX with those properties designed in.

### 1.2 Name

**Hedronetes** — *hedron* (a unified solid) + *-netes* (from Kubernetes).
Stylized **h3s** the same way Kubernetes is k8s and the lightweight distro is k3s.

| Artifact | Path / value |
|---|---|
| Binary | `h3s` |
| Config | `/etc/hedronetes/config.yaml` |
| kubeconfig | `/etc/hedronetes/h3s.yaml` |
| Data dir | `/var/lib/hedronetes/` |
| Token | `/var/lib/hedronetes/server/node-token` |
| License | Apache-2.0 |
| Default API listen | `:6443` |

---

## 2. Goals and non-goals

### 2.1 Goals

1. Stock `kubectl` and Helm 3 work against `:6443` with no custom client.
2. Kubernetes **v1.34** wire format at first release (GVK, ObjectMeta,
   spec/status, watch, resourceVersion), pinned via `k8s-openapi` feature
   `v1_34`. One minor pinned per h3s release, same cadence philosophy as k3s.
3. CRI v1, CNI v1.1, CSI remain plugin contracts. h3s speaks them; it does
   not absorb the plugin ecosystem.
4. Single-node `h3s server` is useful in under a minute on Linux amd64/arm64.
5. HA is first-class: three servers + agents, join tokens, supervisor tunnel.
6. Default OCI runtime is **youki**, not runc.
7. Default kube-proxy datapath is **nftables**, not iptables.
8. Storage is a trait. SQLite is the shipped backend for one server;
   Postgres (`tokio-postgres`) is the shipped backend for more than one
   server (`512f220de42ed989f937ad7625a7de732247797b`,
   `174367d34e643b78a3e5a654b02e460de5ada3bf`); etcd and MySQL are not
   implemented.
9. Kubelet pod lifecycle is a compile-time state machine.

### 2.2 Non-goals

- Replace EKS, GKE, or AKS.
- Windows nodes.
- Rewrite CSI drivers, cloud controller managers, Cilium, Calico, or `kubectl`.
- 100% kubeadm feature parity (in-tree cloud providers, in-tree volume
  plugins, dockershim, legacy APIs).
- Beat crun on OCI spawn latency.
- Absorb every upstream KEP at Kubernetes velocity on day one.
- Invent a new YAML dialect or require a custom kubectl.
- Treat Rūsternetes, Anvil, or Xline as already-shipped defaults.
- Embed a console or a record store in the `h3s` binary, or use
  HedronDB as the kube registry. See §2.4.

### 2.3 What “done the right way” means relative to k3s

| k3s | h3s |
|---|---|
| Embeds upstream `kube-apiserver` / scheduler / controller-manager / kubelet as goroutines | Native Rust components as Tokio tasks |
| Kine translates etcd API to SQL after the fact | `Storage` trait; SQL is a first-class backend, not a shim unless we must |
| runc default | youki default, runc/crun allowed |
| iptables-first kube-proxy | nftables-first, eBPF optional |
| Go kubelet phase loop | Typed FSM (Krustlet design, CRI-backed) |
| GOGC knob to tame idle RSS | One allocator, no GC |
| client-go / generated clientsets | kube-rs + k8s-openapi |

The fair comparison is k3s, not kubeadm. Winning vs kubeadm on binary size and
RAM is table stakes. Winning vs k3s is: no GOGC, Rust kubelet, youki, trait
store, typed pod states.

### 2.4 Agentic planes (adjacent products)

This section is the product source of truth for how h3s sits beside
adjacent retrieve, record, and console products. It does not change the
k3s-shaped contract in §§1–2.3 or §6. Agentic use is potential, not a
shipped feature.

#### Thesis (potential, not shipped)

h3s **MAY** be used as an agentic Kubernetes-shaped runtime: agents as
first-class cluster citizens, addressed through the same stock API as
workloads. That potential **MUST NOT** change the product contract —
one binary, `server` / `agent`, stock `kubectl` and Helm, Kubernetes
wire format on `:6443`. Agentic use is an application of that contract,
not a fork of it.

Nothing in this section is a v0.1 or v0.2 deliverable.

#### Three-plane client model

Future integration **MUST** treat these as separate planes. Clients
compose them. The `h3s` binary implements only the cluster plane.

| Plane | Product | Role |
|---|---|---|
| **Cluster** | h3s | Stock Kubernetes API on `:6443`. The runtime. |
| **Retrieve** | Turso / lattice-style retrieval | Recall at read time. |
| **Record** | HedronDB | Durable record and reconcile beside Turso. |

**Cluster plane.** Facet (or any other console) is a *client* of h3s.
It **MUST NOT** be embedded in the `h3s` binary. The console speaks the
Kubernetes API the same way `kubectl` does.

**Retrieve plane.** Recall at read time is a Turso / lattice-style
concern. h3s does not own that path.

**Record plane.** HedronDB holds durable record/reconcile state *beside*
Turso. It **MUST** remain out-of-process relative to h3s. It **MUST NOT**
become `h3s-storage`, an etcd replacement, or any other kube registry /
store backend.

#### MUST NOT

- Embed Facet, or any console, inside the `h3s` binary.
- Embed HedronDB inside the `h3s` binary.
- Use HedronDB as the Kubernetes registry or `Storage` backend.
- Cut over a dual source of truth between Turso and HedronDB without a
  separate, explicit product decision. This spec does not make that
  decision.
- Vendor the Rūsternetes or Krustlet trees as the path to agentic
  features. Same rule as §4 and §21: ideas and interfaces, not git
  subtrees.

#### Phasing

Facet and HedronDB binding work **MUST NOT** start before a working
apiserver exists (P2-class: stock `kubectl` against `:6443` for the
v0.1 type set). Until then, this section is design constraint only.

After in-tree controllers exist, h3s **MAY** grow AgentRun-style custom
resources served through the stock Kubernetes API. That work is
design-only until those controllers exist. Those CRDs, if added, still
travel over the stock API. They are not a reason to embed a console or
a record store.

See also `COMPANION_SPEC.md` (h3s-cp) for the companion overlay (Facet action, HedronDB intent, Herdr habitat, Turso engine). Parent wins on conflict; this section is not amended by the companion.

---

## 3. Process model

Mirror k3s. Same binary, two subcommands.

```
h3s server    # control plane + datastore + supervisor
              # also runs a local agent unless --disable-agent
h3s agent     # kubelet + kube-proxy + CNI + runtime + tunnel client
```

### 3.1 Single-server (default)

One host, `h3s server`, embedded SQLite (shipped default), embedded agent. This is the laptop,
CI, and appliance path. More than one `h3s server` is a Postgres shape —
shipped on `512f220de42ed989f937ad7625a7de732247797b` (`--store=postgres`,
one primary, two servers; not HA; see §3.3).

```
┌─────────────────────────────────────────────┐
│                 h3s server                  │
│  apiserver  scheduler  controllers  store   │
│  supervisor                                 │
│  ──────── embedded agent ─────────────────  │
│  kubelet    kube-proxy   CNI   containerd   │
└─────────────────────────────────────────────┘
```

### 3.2 Multi-agent

One server, N agents. Agents open a rustls WebSocket tunnel to
`wss://<server>:6443/v1-h3s/connect` so the kubelet API (`10250`) and CRI
socket stay localhost-only on workers. This is k3s’s tunnel/Konnectivity
equivalent and is **required**, not optional, for the default agent path.
Do not skip it. A Rust apiserver without this tunnel is not a k3s.

### 3.3 High availability

More than one `h3s server` shares **one Postgres primary** via
`--store=postgres` with `--datastore-endpoint` (a `postgres://` URL) —
shipped on `512f220de42ed989f937ad7625a7de732247797b`; the
`PostgresStore` driver is `tokio-postgres`, shipped on
`174367d34e643b78a3e5a654b02e460de5ada3bf`. Two `h3s server` processes
can share that primary; a create on the first is read on the second.
This is not the HA milestone: no Raft, no leader election, no read
replica, no automatic failover, and etcd/MySQL/Xline are not
implemented. Agents join a fixed registration address (VIP or load
balancer) on `:6443`. After join, each agent learns the current
apiserver endpoint list and load-balances.

SQLite **MUST NOT** be used with more than one server. Same rule as k3s.
`h3s agent` has no datastore: agents hold no registry state; all reads
and writes go through the API server.

### 3.4 Ports

| Port | Role | Notes |
|---|---|---|
| 6443 | Kubernetes API + supervisor | Combined default, like k3s |
| 9345 | Supervisor only | Optional split |
| 10250 | kubelet | Localhost-only when tunnel is on |
| 2379–2380 | etcd | HA store only — etcd not implemented; n/a until a HA store lands |
| CNI overlay | per backend | flannel VXLAN default |

### 3.5 Join and identity

- Shared cluster token at `/var/lib/hedronetes/server/node-token`.
- Separate `--agent-token` MAY exist so agents cannot join as servers.
- `--cluster-init` is not implemented; multi-server setup is `--store=postgres` + `--datastore-endpoint` (shipped on `512f220de42ed989f937ad7625a7de732247797b`).
- Additional servers point at the same Postgres primary: `h3s server --store=postgres --datastore-endpoint postgres://…` (shipped; two servers can share one primary). Registration through a fixed `--server https://vip:6443` address is **planned, not shipped** — no VIP/load-balancer join exists yet.
- Agents: `h3s agent --server https://vip:6443 --token …` (shipped agent join; the agent has no datastore)
- Node password / NodeRestriction as in k3s/Kubernetes.

---

## 4. Workspace and crate map

```
hedronetes/
  Cargo.toml                  # workspace
  crates/
    h3s/                      # multicall binary
    h3s-apiserver/            # Axum + rustls API server
    h3s-storage/              # Storage trait + backends
    h3s-scheduler/            # Filter / Score / Preempt
    h3s-controllers/          # in-tree loops on kube-rs
    h3s-kubelet/              # typed pod FSM + CRI client
    h3s-proxy/                # nftables kube-proxy
    h3s-cri/                  # CRI v1 protos + tonic client
    h3s-cni/                  # CNI plugin invocation
    h3s-auth/                 # RBAC, SA tokens, NodeRestriction, bootstrap certs
    h3s-supervisor/           # agent tunnel
    h3s-certs/                # cluster PKI
    h3s-deploy/               # auto-apply manifests (k3s AddOn analog)
    h3s-api/                  # thin helpers over k8s-openapi
    h3s-packaging/            # embed containerd/youki/CNI/pause bits
```

**Do not vendor Rūsternetes as a blob.** Reuse its *ideas* (storage trait,
Tokio all-in-one, crate layout). Reject its Docker/bollard kubelet and SSE
watch protocol.

**Do not copy 31 controllers by hand.** In-tree controllers are kube-rs
`Controller` loops. Third-party operators use the same library.

---

## 5. Dependencies

Dependencies are first-class. h3s is an assembly with a small amount of
original code in the slots that do not already exist.

### 5.1 Workspace pins (normative starting set)

Versions are starting pins and will float with the lockfile. Feature flags
in this table are required defaults.

| Crate | Role | Notes |
|---|---|---|
| `tokio` 1 (`full`) | Runtime | Every component is a task |
| `axum` + `hyper` + `tower` + `tower-http` | API server HTTP | |
| `rustls` + `tokio-rustls` + `rcgen` | TLS | **No OpenSSL in default features** |
| `tonic` + `prost` | CRI, etcd, webhooks, CSI socket | |
| `serde` / `serde_json` / `serde_yaml` | Wire format | `camelCase`, skip-none |
| `json-patch` + `jsonptr` | SSA / JSON patch / merge patch | |
| `thiserror` + `anyhow` | Errors | library vs binary |
| `tracing` + `tracing-subscriber` + `metrics` + `prometheus-client` | Observability | |
| `clap` 4 | CLI | multicall |
| `figment` or clap+serde | `/etc/hedronetes/config.yaml` | |
| `async-trait` + `futures` + `tokio-util` | Async glue | |
| `uuid` + `chrono` or `jiff` | IDs / times | match k8s-openapi |
| `nix` + `rustix` + `caps` + `procfs` + `cgroups-rs` | Node syscalls | |

### 5.2 Kubernetes ecosystem — reuse, do not reinvent

| Crate | Role |
|---|---|
| `kube` (runtime, derive, ws, rustls-tls) | Client, reflector, controller runtime |
| `k8s-openapi` feature `v1_34` at first release | API types |
| `schemars` | CRD schemas |
| `kubert` | Admin server, graceful shutdown, index, lease helpers (Linkerd production code) |
| `kube-derive` | In-tree and out-of-tree CRDs |

### 5.3 Store backends

| Crate | Backend |
|---|---|
| `rusqlite` (sqlite) | One server — shipped default |
| `tokio-postgres` | More than one server — shipped on `174367d34e643b78a3e5a654b02e460de5ada3bf` |
| — (none) | MySQL: not implemented |
| `etcd-client` | Not implemented |
| Xline client | Not implemented |

### 5.4 Node

| Crate / project | Role |
|---|---|
| `h3s-cri` (generated from kubernetes CRI protos) | CRI v1 client |
| containerd rust-extensions (`containerd-client`, shims) | Talk to containerd |
| **youki / `libcontainer`** | Default OCI runtime |
| `rscni` | CNI plugin exec |
| `nftables` / `mnl` / `nftnl` | Default kube-proxy |
| `aya` / `aya-ebpf` | Optional feature `ebpf-proxy` |

containerd itself remains a Go process. That is accepted. A Rust CRI *server*
is a later optimization, not a v1 blocker. h3s MAY embed containerd the way
k3s does (extract on boot into the data dir).

### 5.5 Optional, not default

| Integration | When |
|---|---|
| Anvil / Verus verified RS, Deployment, STS | Feature `verified-controllers`, extra CI. Not required to boot. |
| Kubewarden | Wasm admission, opt-in |
| Xline | `--store=xline` — not implemented; surface in design only |
| Spegel-like registry mirror | After v0.3 |
| Gateway API | Via Traefik first, native later |

### 5.6 Explicitly rejected dependencies

| Reject | Why |
|---|---|
| `bollard` / Docker Engine API | Not CRI. Rūsternetes shortcut. |
| OpenSSL as default TLS | rustls only in default features |
| Embedding `kubernetes/kubernetes` via cgo or a sidecar apiserver | That is k3s. h3s owns the apiserver. |
| SSE as the watch protocol | Compatibility fork. Watch is chunked HTTP. |

---

## 6. Compatibility contract

### 6.1 MUST

- Stock `kubectl` against the API server.
- Helm 3.
- kube-rs operators and any client that speaks the Kubernetes REST+watch API.
- Watch over chunked HTTP with `resourceVersion`, bookmark events, and `410 Gone`
  when the watch window is missed or compacted. **Not SSE.**
- CRI v1 to containerd. **Not** Docker.
- CNI v1.1 plugins.
- CSI via external provisioner/attacher/node driver. h3s does not ship in-tree
  volume plugins.
- RBAC.
- Bound ServiceAccount tokens.
- NodeRestriction admission.
- Optimistic concurrency via `resourceVersion` (HTTP 409 on conflict).

### 6.2 SHOULD (phased)

- CEL `ValidatingAdmissionPolicy`
- Aggregated API servers
- EndpointSlice-only proxy path (no legacy Endpoints requirement after v0.2)
- Gateway API (via Traefik initially)

### 6.3 MUST NOT

- Invent a new manifest dialect.
- Require a Hedronetes-specific kubectl for ordinary operations. A `h3s kubectl`
  shim that points at the local kubeconfig is allowed as sugar.
- Break GVK / ObjectMeta / spec+status.

### 6.4 Conformance

| Milestone | Bar |
|---|---|
| v0.1 | Smoke: create ns, configmap, pod, deploy, service. `kubectl apply` works. |
| v0.2 | Sonobuoy certified-conformance *subset*: workloads + services. |
| v1.0 | Official conformance at or above k3s’s bar for the pinned Kubernetes minor. Public score. |

Do not advertise anyone else’s Sonobuoy number as h3s’s.

### 6.5 What we strip versus kubeadm

Same spirit as k3s, plus the Rust-specific cuts:

- in-tree cloud providers
- in-tree volume plugins
- dockershim / Docker as a runtime
- Windows
- legacy / deprecated APIs
- generated client-gen / informer-gen / deepcopy piles
- iptables as the default proxy

---

## 7. Storage

### 7.1 Trait (normative)

```rust
#[async_trait]
pub trait Storage: Send + Sync + 'static {
    async fn get(&self, key: &StoreKey) -> Result<Option<StoredObject>>;
    async fn list(&self, sel: ListSelect) -> Result<ObjectList>;
    async fn create(&self, obj: StoredObject) -> Result<StoredObject>;
    async fn update(
        &self,
        obj: StoredObject,
        rv: ResourceVersion,
    ) -> Result<StoredObject>; // Err(Conflict) → HTTP 409
    async fn delete(&self, key: &StoreKey, rv: ResourceVersion) -> Result<()>;
    async fn watch(&self, sel: WatchSelect) -> Result<WatchStream>;
    async fn compact(&self, rev: ResourceVersion) -> Result<()>;
    async fn lease_grant(&self, ttl: Duration) -> Result<Lease>;
    async fn lease_keepalive(&self, id: LeaseId) -> Result<()>;
}
```

Key layout:

- namespaced: `/registry/{resource}/{namespace}/{name}`
- cluster-scoped: `/registry/{resource}/{name}`

`resourceVersion` **MUST** map monotonically onto the backend revision.
Updates **MUST** be compare-and-swap on that revision.

### 7.2 Watch semantics (the load-bearing contract)

| Client request | Required behavior |
|---|---|
| `resourceVersion` unset | Consistent list from current revision, then watch |
| `resourceVersion=0` | Any consistent snapshot (cache OK) |
| `resourceVersion=N` | Start at exact N, or `410 Gone` if compacted / out of window |
| bookmarks | Periodic bookmark events so clients can advance RV without object changes |

The API server **SHOULD** serve watches and most lists from an in-memory watch
cache. Cache miss / stale RV falls back to the store. No watch cache exists
in this tree: SQLite and Postgres watches read the MVCC log and sleep 50ms
when a page is short (Postgres may wake sooner). See §7.4. Streaming encode lists
item-by-item. Do not materialize an entire `PodList` before writing the
response. That is the Go failure mode we refuse to copy.

### 7.3 Backends

| Flag | When | Multi-server |
|---|---|---|
| `--store=sqlite` | **Shipped default**: one `h3s server` | No |
| `--store=postgres` | **Shipped on `512f220de42ed989f937ad7625a7de732247797b`**: more than one `h3s server`; requires `--datastore-endpoint` with a `postgres://` URL | Yes — one primary |
| `--store=mysql` | Not implemented | — |
| `--store=etcd` | Not implemented | — |
| `--store=memory` | Tests | No |
| `--store=xline` | Not implemented | — |

SQLite implementation is h3s’s own Kine-shaped MVCC table *behind the trait*,
in-process. We do not run a separate Kine process on the default path.
SQLite stays the one-server default.

Postgres is the multi-server store, shipped on
`512f220de42ed989f937ad7625a7de732247797b`: `PostgresStore` uses
`tokio-postgres` (shipped on `174367d34e643b78a3e5a654b02e460de5ada3bf`),
talks to one Postgres primary, and two `h3s server` processes can share
it — a create on the first is read on the second. On that store, a
revision exists, a stale `resourceVersion` gets a conflict, a watch
resumes, compaction fails the old watch, and a Geode-sealed payload is
absent from a raw SQL read and returned by `open`. With
`--secrets-encryption`, Secret payloads seal with the existing Geode
sealer. etcd, MySQL, and Xline stay not implemented — neither a default
HA nor an opt-in backend ships for them today.

HedronDB is not a `--store=` backend. See §2.4. Jev and pgvector are not
h3s store features either.

### 7.4 Maintenance (gc slice contract; not shipped)

This section is the contract for the gc slice (KP-37). It is not shipped.
`v0.11.1` on `0ea5b191d8489eb0feba1d06e7faeba8b6a0a323` does not run
compaction from the server and does not delete lease rows. One
`h3s server` stays on SQLite. The slice does not tag and is not the HA
milestone.

1. **Startup compact.** After the store opens and before `Api::new`,
   `run_server` awaits `store.maintain(1024)`. `maintain` reads the head.
   When `current - 1024` is above the compacted floor, it calls the
   existing `compact` with that revision. Otherwise the floor does not
   move. Compacting to the head is forbidden. The per-key baseline rule
   in `compact` stays. A store error from this awaited call fails server
   start.
2. **60 second task.** After that awaited call, a spawned task calls
   `maintain(1024)` every 60 seconds. A later error is logged and does not
   stop the API.
3. **Lease-row deletion.** `maintain` deletes `registry_leases` rows whose
   `expires_ms` is at or before now, on SQLite and on Postgres, and
   returns how many rows it deleted. It does not delete registry objects.
   Leases stay unattached to keys.
4. **Watch stays a 50ms poll.** SQLite and Postgres watches read the MVCC
   log and sleep 50ms when the page is short. Postgres may wake sooner.
   Those two call sites are the measured reason there is no watch cache.
   This slice does not add a cache and does not change the 50ms.
5. **Proof.** A SQLite storage test shows `maintain` moving the floor
   through `compact` and an expired lease row gone. A throwaway
   `h3s server` on a seeded SQLite data dir with a head of at least 1026,
   on a port other than 6443, answers `/readyz` with `ok` and returns
   HTTP 410 for `resourceVersion=1` after the startup `maintain`.

---

## 8. API server

- Axum + rustls. Default bind `0.0.0.0:6443`.
- Authn: X.509 client certs (kubeconfig), ServiceAccount bearer tokens,
  bootstrap tokens.
- Authz: RBAC. Node authorizer + NodeRestriction.
- Admission chain: NamespaceLifecycle, LimitRanger, ResourceQuota,
  ServiceAccount, NodeRestriction, PodSecurity (restricted default on
  non-system namespaces, configurable), mutating/validating webhooks, CEL
  when ready.
- Subresources required for v0.2: `status`, `scale`, `bind`, `eviction`,
  `log`, `exec`, `attach`, `portforward`.
- OpenAPI v3 discovery.
- Server-Side Apply with field managers shipped on
  `c3fd6b5be3649ee4cea272d699f4f7174502d990` (merged
  `c76d4f6d87bf1795780af3857dfb21e397c7ffad`): `kubectl apply --server-side`
  is supported, alongside client-side `kubectl apply`.
- Audit log optional.

The API server is the only component that talks to `Storage` for user-visible
reads and writes. Scheduler, controllers, and kubelet go through the API,
exactly as in Kubernetes. In all-in-one mode the “network hop” MAY be a
Tokio channel that still implements the same API types, but the authority
boundary stays: components do not write the store behind the API server’s
back.

---

## 9. Scheduler

Filter / Score / Preempt as async traits.

Required filters for v0.2: node unschedulable, taints/tolerations, node
selector, node affinity, resource fit.

Required scorers for v0.2: least allocated, balanced allocation.

v0.3: pod affinity/anti-affinity, topology spread, priority / preemption.

Plugins **SHOULD** be loadable as WASM components. In-process Rust plugins via
the traits are fine for built-ins. Do not invent a Go plugin ABI.

---

## 10. Controllers

In-tree controllers live in `h3s-controllers` and are kube-rs
`Controller::new(…).owns(…).run(reconcile, error_policy, ctx)` loops, with
kubert providing admin / ready / lease / shutdown.

### 10.1 v0.1 set

- Namespace
- ServiceAccount
- ReplicaSet
- Deployment
- EndpointSlice
- Node (status heartbeat consumer / taint eviction later)
- Garbage collector (owner-ref)

### 10.2 v0.2 set

- DaemonSet
- Job
- Service / Endpoints (legacy bridge)
- PersistentVolume binder
- Default storage class
- HelmChart / AddOn deploy controller

### 10.3 v0.3 set

- StatefulSet
- CronJob
- HPA
- PDB
- Job TTL
- CSR signer (cluster signing)

Cloud controllers stay out of tree.

### 10.4 Verified controllers

Anvil/Verus implementations of ReplicaSet, Deployment, and StatefulSet **MAY**
replace the default loops behind `--features verified-controllers`. They are
not the v1 boot path. CI MAY run them as an extra job.

---

## 11. Node agent — the part k3s still has to borrow

### 11.1 Kubelet

This is the original code h3s must write. Krustlet is the design reference
and is unmaintained / WASM-only. Rūsternetes talking Docker is rejected.

Pod lifecycle is a typed state machine. Illegal transitions do not compile.

```
Registered
    → SandboxCreating
        → SandboxReady
            → ContainersStarting
                → Ready
                    → Terminating
                        → Succeeded
                        → Failed
    → Failed          (any phase, on unrecoverable error)
```

Each state is a type. Transitions are trait methods that return the next
state. The runtime provider is a `CriRuntime` that speaks CRI v1 over tonic
to containerd.

Required for v0.2:

- pause / pod sandbox, then containers join the sandbox namespace
- init containers, then app containers
- probes: HTTP, TCP, exec (liveness, readiness, startup)
- volumes: emptyDir, hostPath, configMap, secret, projected, downwardAPI
- PVC via CSI node plugin
- resource limits → cgroups
- log retrieval, exec, attach via the API server → kubelet streaming
- node registration with capacity, allocatable, conditions
- eviction on memory/disk pressure (basic)
- CAS status updates (`resourceVersion`)

### 11.2 Runtime stack

```
kubelet  --CRI v1 gRPC-->  containerd  --OCI runtime v2-->  youki
```

- Default containerd socket: `/run/hedronetes/containerd/containerd.sock`
  (or the extracted embed path under the data dir).
- Default OCI runtime: youki.
- `--oci-runtime=runc|crun` is allowed.
- Pause image is bundled and imported on first boot.
- RuntimeClass `wasm` MAY be added later using youki’s Wasm handlers or a
  Krustlet-style provider. Not v1.

### 11.3 kube-proxy

- Consumes EndpointSlices.
- Default datapath: nftables.
- Modes: ClusterIP (DNAT), NodePort (30000–32767), LoadBalancer
  (in conjunction with ServiceLB).
- Session affinity.
- Feature `ebpf-proxy` MAY swap in an aya datapath. Not default.

### 11.4 CNI

h3s invokes CNI plugins; it is not a CNI. Default plugin is flannel VXLAN,
shipped and auto-configured. `--cni=none` for users who will install
Cilium/Calico. NetworkPolicy: kube-router or a later native controller;
v0.2 MAY ship without NetworkPolicy.

---

## 12. Bundled addons

k3s parity. Deployed from `/var/lib/hedronetes/server/manifests` by
`h3s-deploy`. Files in that directory are applied and re-applied on change.
Packaged manifests are rewritten on start so users cannot permanently
corrupt them; user addons live alongside.

Disable with `--disable=name` or `--disable=name1,name2`.

| Addon | Default | Notes |
|---|---|---|
| coredns | on | Cluster DNS |
| servicelb | on | Shipped on `20abf97662a0c191ec1ba7ede78820ed9d9c6e02`; skipped with `--disable=traefik,servicelb` |
| traefik | on | Shipped on `20abf97662a0c191ec1ba7ede78820ed9d9c6e02`; skipped with `--disable=traefik,servicelb` |
| local-storage | on | local-path-provisioner |
| metrics-server | on | |
| helm-controller | off until v0.3 | HelmChart CRD, kube-rs implementation |
| flannel | on | `--flannel-backend=vxlan\|wireguard\|none` |

`--disable-network-policy`, `--disable-helm-controller`, `--disable-agent`,
`--disable-proxy` match the k3s flag philosophy.

---

## 13. Packaging and install

### 13.1 Binary

One multicall binary. musl static for the release artifact on linux
amd64/arm64. glibc builds allowed for distro packages.

On first server start, h3s extracts embedded bits into
`/var/lib/hedronetes/data/<sha>/` the way k3s extracts `k3s-root`:

- containerd (or a documented external containerd)
- youki
- CNI plugins (flannel, bridge, host-local, portmap)
- host utils as needed (nft, xtables-legacy compatibility only if required)
- pause image

### 13.2 Install UX

```bash
curl -sfL https://get.hedronetes.dev | sh -

# single node
h3s server

# multi-server store — shipped, not HA: two servers on one Postgres primary
# (`--store=postgres --datastore-endpoint postgres://…`); etcd, MySQL, Xline not implemented
h3s server --store=postgres --datastore-endpoint postgres://registry:5432/h3s
h3s server --store=postgres --datastore-endpoint postgres://registry:5432/h3s --token "$TOKEN"

# worker
h3s agent --server https://vip:6443 --token "$TOKEN"
```

`KUBECONFIG=/etc/hedronetes/h3s.yaml` is written with admin credentials.

Config file `/etc/hedronetes/config.yaml` is equivalent to flags. Flags win
on conflict.

### 13.3 Feature / flag surface (v1)

```
--store=sqlite|etcd|postgres|mysql|xline
--datastore-endpoint          # DSN for sql / etcd
--cluster-init
--server
--token / --token-file
--agent-token
--disable-agent
--disable-proxy
--disable=traefik,servicelb,metrics-server,helm-controller,coredns,local-storage
--container-runtime-endpoint
--oci-runtime=youki|runc|crun
--flannel-backend=vxlan|wireguard|none
--cluster-cidr
--service-cidr
--cluster-dns
--cluster-domain
--secrets-encryption
--tls-san
--bind-address
--https-listen-port
--data-dir
--write-kubeconfig
--kube-apiserver-arg
--kube-scheduler-arg
--kube-controller-manager-arg
--kubelet-arg
--kube-proxy-arg
```

Rootless is a phase-3 goal, not v1.

---

## 14. Observability and operations

- `h3s` logs on stdout via `tracing`. JSON with `--log-format=json`.
- Admin endpoints on the supervisor: `/readyz`, `/livez`, `/metrics`.
- metrics-server addon for `kubectl top`.
- Durability shipped on `e7e3d467b478a875bb7df818ef67bb38ed1d5c7a`
  (merged in PR #66 on `e45c1764e49ccf4e5da61dd060f77ee6e74002d6`); no
  `h3s etcd-snapshot` command exists in this tree, and the old etcd
  snapshot sentence stays not shipped:
  - **Token rotation.** `h3s token rotate --data-dir DIR` replaces
    `DIR/server/node-token` with one new value that passes `valid_token`
    (32–256 printable ASCII). It refuses while
    `DIR/server/db/.registry.lock` or `DIR/server/.node-token.lock` is
    held, does not log the secret, and prints the new secret once on
    stdout. After the next `h3s server` start, the previous token fails
    join and the new token passes. No TTL; no second token stays valid.
  - **Offline backup.** `h3s backup --data-dir DIR --output DEST` refuses
    while the registry lock is held; `DEST` must not already exist. The
    copy is `server/db/h3s.db`, `h3s.db-wal` and `h3s.db-shm` when those
    files exist, `server/tls/`, `server/ca.crt`, and `server/node-token`.
    It does not copy containerd or the rest of the runtime.
  - **Restore.** `h3s restore --data-dir DIR --from DEST` refuses while
    the registry lock is held and puts that same set back; a throwaway
    server on the restored dir answers `/readyz` with `ok`.
  - **Upgrade, written only.** Stop, replace the `h3s` binary, start with
    the same data dir and the same flags. No 0.9.1 → 0.11 schema jump is
    described.
- Backup of the shipped stores (present tense):
  - SQLite (shipped, one server): offline pair —
    `h3s backup --data-dir DIR --output DEST`, then
    `h3s restore --data-dir DIR --from DEST` (shipped on
    `e7e3d467b478a875bb7df818ef67bb38ed1d5c7a`).
  - Postgres (shipped, multi-server): `pg_dump` on the shared registry database.
  - etcd: not implemented, no backup path documented for it in this tag.
- Server-run compaction and expired lease-row deletion are the gc slice
  contract in §7.4, not shipped.
- Version output: `h3s --version` prints h3s version, pinned Kubernetes
  minor, youki version, containerd version.

---

## 15. Security baseline

- rustls everywhere. No default OpenSSL.
- Cluster CA generated at init; node and client certs issued from it.
- kubelet serving certs rotated.
- Bound SA tokens (no permanent tokens for default SAs).
- NodeRestriction + node authorizer.
- Secrets encryption at rest shipped on `43548fc5de88374dc2c9a8bb4d8998aae3808dbc` (merged `beaad03297300a9d84508cedd242163427797247`): Secret payloads are sealed with Geode `seal` / `open` (GDE1); Facet is the agent path to that vault; h3s grows no second cipher.
- Pod Security restricted default for non-system namespaces, configurable.
- No anonymous write. Anonymous read limited to `/readyz` `/livez` `/version`
  as in upstream.
- Agent tunnel: workers do not expose 10250 or CRI on the reachable network.

Memory safety of kubelet + youki is a product claim. It does not replace
RBAC, admission, or seccomp.

---

## 16. Roadmap and definition of done

### One release axis

This document was drafted against the v0.9.0 tree (see the header). Its
original planning milestones were labeled `v0.0`–`v0.3` plus `v1.0`; the
project ships releases as `v0.9.x`, `v0.10.x`, and reserves `v1.0.0`. Those
old labels are **mapped onto the release axis below**, not carried as a
second versioning axis.

| Release | Milestone | Old planning label it realizes |
| --- | --- | --- |
| **v0.9.0** (shipped, tag) | **M1 subset** — the first release that runs a cluster: two hosts, restricted non-root Pods, ClusterIP | “v0.1 — kind, but native” plus the single-server parts of “v0.2 — k3s-shaped” that already existed at that tag |
| **v0.9.1** (shipped, tag) | structure substrate: split API dispatch, one PodRuntimeProfile, restarting supervisor | — (honesty and structure release between M1 and the k3s-shaped milestone) |
| **v0.10.0** (shipped, tag) | **k3s-shaped single-server** — one SQLite server, N agents, default Pods, bound tokens, ClusterIP and NodePort, local-path PVC | the remainder of “v0.2 — k3s-shaped” that holds without HA |
| **v0.11.0** (shipped, tag) | Server-Side Apply, opt-in Geode secrets encryption, default-on Traefik and ServiceLB | the three behaviors restored after the contract strike; not the HA milestone |
| **v0.11.1** (shipped, tag) | Postgres one-primary store, plus `h3s token rotate`, `h3s backup`, and `h3s restore` | the store half of “v0.3 — HA and hardening”, and the durability commands; the HA half stays reserved |
| **v1.0.0** (reserved) | **M2** — HA milestone: Raft/leader election, automatic failover, security MUSTs, official conformance (not the Postgres store itself; etcd/MySQL/Xline not implemented) | the rest of “v0.3 — HA and hardening” merged with the old “v1.0” conformance list |

### v0.9.0 — M1 subset (shipped)

- API server CRUD + watch for core/v1 (Namespace, ConfigMap, Secret, Pod,
  Service, Node, ServiceAccount) and apps/v1 (Deployment, ReplicaSet); RBAC
  bootstrap; ReplicaSet and Deployment controllers; scheduler with
  resource fit, nodeSelector, affinity, and tolerations; CRI kubelet on
  containerd; agent join with token; supervisor tunnel; nftables
  kube-proxy for IPv4 ClusterIP; single writing SQLite registry.
- The runtime profile is the M1 Pod Security subset: non-root UID, dropped
  ALL capabilities, no privilege escalation, RuntimeDefault seccomp.

### v0.9.1 — structure substrate (shipped)

- Split API dispatch, a single PodRuntimeProfile shared by API admission
  and kubelet, and a supervisor that restarts failed node tasks. Claims in
  the README match this tree; where they did not, the docs moved.

### v0.10.0 — k3s-shaped single-server (shipped)

Goal: the released two-host cluster behaves as one honest k3s-shaped
system rather than a collection of parts.

- Default Pods reach Ready end to end: API admission and kubelet agree on
  the same runtime profile, and the documented default Pod runs.
- The API serve loop outlives controller and scheduler death; a stopped
  task restarts, and an in-flight `kubectl` call is not cancelled.
- The server takes an exclusive lock on the SQLite registry; a second
  server against the same data directory fails closed.
- **v0.10.0 shipped no HA flags**, and v0.10.0 itself has no Postgres. The
  multi-server Postgres store lands later on this line
  (`512f220de42ed989f937ad7625a7de732247797b`): `--store=postgres` with
  `--datastore-endpoint` talks to one primary; `--cluster-init` remains
  unimplemented, and etcd is not implemented.
- Cluster add-ons (CoreDNS) and `--disable=` follow the shape documented
  in [`docs/addons.md`](./docs/addons.md).
- NodePort, local-path PVC, and bound ServiceAccount tokens ship in
  v0.10.0. CoreDNS remains not shipped. Traefik and ServiceLB ship in
  v0.11.0. The HA upgrade path is the shipped `--store=postgres` +
  `--datastore-endpoint` shape (`512f220de42ed989f937ad7625a7de732247797b`);
  `--cluster-init` and etcd stay unimplemented.

**DoD:** one SQLite server plus agents, `kubectl apply` of the documented
default Pod reaches Running behind a ClusterIP Service, and killing
in-process control tasks does not kill the API.

### v0.11.0 — apply, sealed secrets, and default add-ons (shipped)

- Server-Side Apply field managers. `kubectl apply --server-side` stores
  `managedFields`. A second manager on a different field leaves the first
  manager's fields in place.
- Secrets encryption at rest is opt-in with `--secrets-encryption`. Payloads
  are sealed with Geode `seal` / `open`. Facet is the agent path. h3s does
  not grow a second cipher. With the flag off, Secret data stays readable.
- Traefik and ServiceLB are default-on. A Service of type LoadBalancer
  receives an address. An Ingress is served. `--disable=traefik,servicelb`
  leaves both off.
- Not HA. Not conformance. Not `v1.0.0`. CoreDNS is not shipped.
  NodeRestriction is not shipped.

### v0.11.1 — postgres store and durability (shipped)

- `--store=postgres` with `--datastore-endpoint` talks to one Postgres primary.
  Two `h3s server` processes share it. SQLite stays the one-server default.
  Not leader election, not automatic failover, not a read replica.
  etcd, MySQL, and Xline are not implemented. `h3s agent` has no datastore.
- `h3s token rotate` replaces `server/node-token` with one new valid token.
  It refuses a held registry lock or token lock, prints the secret once on
  stdout, and does not log it. After the next server start the old token
  fails and the new token passes. No TTL. No second valid token.
- `h3s backup` and `h3s restore` copy the registry, its wal and shm when
  present, `server/tls/`, `server/ca.crt`, and `server/node-token`.
  Not containerd and not the rest of the runtime. The destination must not
  already exist. A throwaway restore answers `/readyz` with `ok`.
- Upgrade, written only: stop, replace the `h3s` binary, start with the
  same data dir and the same flags.
- Not HA. Not conformance. Not `v1.0.0`. No `h3s etcd-snapshot` command.

### gc slice — server compaction and lease GC (contract; not shipped; no tag)

Not shipped. The contract is §7.4: startup `maintain(1024)` awaited before
`Api::new`, a 60 second `maintain(1024)` task, deletion of expired
`registry_leases` rows on SQLite and Postgres, and the 50ms MVCC-log poll
kept as the reason there is no watch cache. Cargo stays `0.11.1`. Not HA.
No watch cache. No leader election.

### v1.0.0 — M2 (reserved; contains the old “v0.3” and “v1.0” scopes)

Not shipped. Nothing below is present tense.

- Durable high availability beyond the shipped two-server Postgres store:
  Raft/leader election, automatic failover, and read replicas (not
  shipped; a read replica is not claimed). etcd, MySQL, and Xline stay
  not implemented.
- Hardening still reserved here: NodeRestriction. Server-Side Apply,
  Geode secrets encryption, and Traefik and ServiceLB shipped in v0.11.0.
  Bound ServiceAccount tokens shipped in v0.10.0.
- Full Kubernetes conformance at the k3s bar for the pinned minor;
  official conformance runs, not claims.
- musl static release binaries, amd64 + arm64. Offline `h3s backup`,
  `h3s restore`, and the written upgrade sentence shipped in v0.11.1.
- Soak: 50 nodes, not 5,000.
- Security baseline in §15 complete.
- Idle single-node server+agent target: **≤ 250 MB RSS** excluding
  containerd and workloads (aspirational vs k3s ~300–500 MB after GOGC
  games). Publish the measured number; do not invent it.

**DoD:** kill one server, `kubectl` still works, agents stay Ready.

### After v1.0 (explicitly later)

- Xline store
- eBPF kube-proxy
- Anvil-verified in-tree loops
- Wasm RuntimeClass
- Rootless
- Spegel-like embedded registry
- Gateway API native
- NetworkPolicy native

---

## 17. Ownership matrix

| Component | Own / reuse | Source of design |
|---|---|---|
| Multicall binary, flags, data dir | Own | k3s UX |
| API server | Own | Kubernetes API + Rūsternetes crate shape |
| Watch / SSA / admission | Own | Kubernetes spec. Hard. |
| Storage trait + sqlite MVCC | Own | k3s/Kine idea, trait-first |
| etcd backend | Reuse `etcd-client` | etcd |
| Postgres/MySQL backend | Own adapter | Kine |
| Xline backend | Reuse client | Xline, optional |
| Scheduler | Own | Kubernetes Framework semantics |
| Controllers | Reuse kube-rs + kubert | Linkerd production pattern |
| Verified controllers | Optional reuse | Anvil |
| Kubelet FSM | Own | Krustlet paper / “Fistful of States” |
| CRI client | Own generated + thin | Kubernetes CRI |
| containerd | Reuse (embed binary) | containerd |
| OCI runtime | Reuse youki | youki |
| kube-proxy | Own | Kubernetes Service semantics, nftables |
| CNI invocation | Own thin | libcni / rscni |
| Flannel / Cilium / Calico | Reuse plugins | upstream |
| CSI | Speak gRPC, reuse drivers | upstream |
| kubectl | Reuse | kubernetes/kubectl |
| Helm | Reuse | Helm 3 |
| AddOn deploy | Own | k3s manifests dir |
| Supervisor tunnel | Own | k3s agent tunnel |
| PKI | Own | k3s + Kubernetes PKI |

---

## 18. Claims we will and will not make

### Will claim, when true

- Single binary, k3s-shaped UX.
- Stock kubectl.
- Default youki + CRI kubelet.
- Pluggable store.
- Typed kubelet states.
- No GC on the API and node paths.
- Memory-safe kernel-adjacent node path (kubelet + youki).
- A published Sonobuoy number *from our CI*.

### Will not claim

- “Faster Kubernetes.”
- “Drop-in for EKS.”
- Rūsternetes’ 94% as ours.
- Anvil proofs as the default controller implementation.
- Xline as more mature than etcd for HA.
- That assembling crates equals a 5,000-node soak.

---

## 19. Repository bootstrap (informative)

Suggested root `Cargo.toml`:

```toml
[workspace]
resolver = "2"
members = [
    "crates/h3s",
    "crates/h3s-api",
    "crates/h3s-apiserver",
    "crates/h3s-auth",
    "crates/h3s-certs",
    "crates/h3s-cni",
    "crates/h3s-controllers",
    "crates/h3s-cri",
    "crates/h3s-deploy",
    "crates/h3s-kubelet",
    "crates/h3s-packaging",
    "crates/h3s-proxy",
    "crates/h3s-scheduler",
    "crates/h3s-storage",
    "crates/h3s-supervisor",
]

[workspace.package]
edition = "2021"
license = "Apache-2.0"
repository = "https://github.com/VirtualMachinist/hedronetes-h3s"
version = "0.9.0"

[workspace.dependencies]
tokio = { version = "1", features = ["full"] }
axum = "0.8"
rustls = "0.23"
tonic = "0.13"
prost = "0.13"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
serde_yaml = "0.9"
thiserror = "2"
tracing = "0.1"
clap = { version = "4", features = ["derive", "env"] }
kube = { version = "2", features = ["runtime", "derive", "ws", "rustls-tls"] }
k8s-openapi = { version = "0.26", features = ["v1_34"] }
async-trait = "0.1"
```

Exact versions are locked at first `cargo generate-lockfile`. The pin
philosophy: one Kubernetes minor per h3s minor.

---

## 20. Decision log (frozen by this spec)

1. Product is k3s-shaped, implementation is native Rust. Not “k3s but we
   rewrite runc.”
2. kubectl and the Kubernetes API are the compatibility nucleus.
3. CRI to containerd, youki as OCI. No Docker.
4. Watch is chunked HTTP. No SSE.
5. Storage is a trait. SQLite default, etcd default HA.
6. Xline, Anvil, eBPF proxy, Wasm RuntimeClass are opt-in later.
7. Linux amd64/arm64 only.
8. Apache-2.0.
9. Fair benchmark is k3s.
10. Do not ship a number we have not run.
11. Agentic use is adjacent, not in-process. Facet and HedronDB stay
    out of the `h3s` binary. HedronDB is not the kube store. A dual
    source of truth between Turso and HedronDB is a separate product
    decision. See §2.4.

---

## 21. Design provenance

This section records what Hedronetes takes from prior art, and what it
refuses. It does not change any requirement in §§1–20. Implementations
MUST follow those sections. This section exists so a later contributor
does not re-import a tree we already rejected, or miss a pattern we
already adopted.

Rule used throughout: **ideas and interfaces, not git subtrees.**

### 21.1 k3s — product we copy

| Take | Drop |
|---|---|
| `server` / `agent` personalities, same binary | Embedding upstream Go Kubernetes as goroutines |
| SQLite default, etcd default HA, SQL via a shim idea | Kine as a separate process on the default path |
| Join tokens, `--cluster-init`, `--disable=` addons | iptables as the default proxy |
| Supervisor tunnel so kubelet `:10250` is localhost-only | runc as the default OCI runtime |
| Manifests directory auto-deploy (AddOns) | In-tree cloud providers and volume plugins |
| Ports 6443 / 10250 / 2379, data-dir layout philosophy | GOGC as the memory story |
| Packaged CoreDNS, local-path, metrics-server, ServiceLB, Traefik, Flannel | The k3s launcher wrapping `kube-apiserver` |

k3s is the UX and packaging contract. It is not the implementation.

### 21.2 Kubernetes / etcd — wire and store semantics

| Take | Drop |
|---|---|
| GVK, ObjectMeta, spec/status, RBAC, SSA, admission | In-tree volume plugins, dockershim, Windows, legacy APIs |
| Watch over chunked HTTP: `resourceVersion`, bookmarks, `410 Gone` | Generated client-gen / informer-gen piles |
| Key layout `/registry/{resource}/{namespace}/{name}` | Running etcd as the only store |
| CRI v1, CNI v1.1, CSI as plugin contracts | Rewriting CSI drivers, CNI plugins, or kubectl |
| Scheduler Filter / Score / Preempt semantics | Go scheduler plugin ABI |

The `/registry/...` key layout is Kubernetes-on-etcd, not a rusternetes invention. h3s keeps it so etcd tooling and mental models transfer.

### 21.3 kube-rs + kubert — controller runtime we depend on

| Take | Drop |
|---|---|
| `kube` client, reflector, `Controller::run` | Hand-rolling 31 reconcile loops |
| `k8s-openapi` as the type crate (`v1_34`) | A parallel `common` types crate |
| kubert admin / ready / lease / shutdown helpers | Inventing a second controller runtime |

This is living dependency, not prior art to reimplement. Linkerd already ships this combination.

### 21.4 rusternetes — map of the territory, not the ground

Rūsternetes (calfonso/rusternetes) is a ground-up Rust Kubernetes with a
storage trait, Tokio all-in-one mode, and a wide API surface. It is
citation-grade. It is not a foundation.

| Take (idea) | Drop (implementation) |
|---|---|
| Storage as a first-class trait with pluggable backends | Their backend code, Rhino, and “any database” framing |
| All-in-one as Tokio tasks in one process | Compose-of-five-containers as the product |
| Crate cuts (apiserver / store / kubelet / proxy / scheduler) | Hand-rolled resource types in `crates/common` |
| Proof that a Rust apiserver crate can exist | Watch over **SSE** |
| Notes on which conformance tests hurt (read their write-ups) | Their Sonobuoy percentage as ours |
| | Kubelet via **bollard / Docker Engine API** |
| | `--skip-auth` as a default path |
| | 31 hand-copied controllers instead of kube-rs |
| | Web console as a v1 deliverable |

Provenance note on the storage trait: the *trait-shaped store* is a
rusternetes idea we keep. The *SQLite MVCC table that stands in for
etcd* is a k3s/Kine idea we keep. h3s combines them: Kine-shaped
sqlite, behind a rusternetes-shaped trait, with etcd as default HA.
§17’s “k3s/Kine idea, trait-first” refers to the sqlite table. This
section is the resolution.

Do not vendor the rusternetes tree. Do not git-subtree it. Do not
advertise 94% as an h3s number.

### 21.5 Krustlet + Krator — paper we copy, not a fork

Krustlet is a CNCF project archived 30 September 2024. Krator (the
sister state-machine operator runtime) was archived in 2024. Both are
unmaintained. The reusable surface is the design in Kevin Flansburg’s
**“A Fistful of States”** (Deis Labs, 2020), not the repositories.

| Take | Drop |
|---|---|
| Each pod phase is a type | The Krustlet / Krator git trees |
| Transitions are methods that return the next state | wasm32-wasi / wasmtime as the default runtime |
| Illegal transitions do not compile | Toleration-gated “this node only runs Wasm” as the product |
| Provider split: kubelet framework vs runtime | Pins to kube 0.5x / Kubernetes ~1.21 |
| Status emission tied to the current state | Forking Krustlet to obtain a container kubelet |
| Later: a Wasm RuntimeClass *provider* on a CRI kubelet | Krator as the controller runtime (kube-rs replaced it) |

Normative mapping onto §11.1 — specified here so `h3s-kubelet` does
not have to re-derive the Provider boundary from the blog:

```
PodState:     Registered | SandboxCreating | SandboxReady
              | ContainersStarting | Ready | Terminating
              | Succeeded | Failed

Transition:   (State, Event) -> Result<State>
              // unrepresentable pairs do not type-check

CriRuntime:   run_sandbox, stop_sandbox,
              start_container, stop_container, remove_container,
              exec, attach, port_forward,
              pull_image, image_status

VolumeManager, ProbeManager, EvictionManager:
              sibling tasks, not methods on CriRuntime.
```

`CriRuntime` is the Krustlet **Provider** role, bound to CRI v1 rather
than wasmtime. It is the only runtime provider at v1. A `WasmRuntime`
that implements the same surface for RuntimeClass `wasm` is after v1
and does not revive the Krustlet binary.

An early `krustlet-cri` experiment (c. 2020) tried the container path
and stalled on volumes, networking, exec, and backoff. That is evidence
the Provider split is the extractable idea, and the repo is not a
shortcut. The lesson that undocumented kubelet behavior has to be read
from `kubernetes/kubernetes`, not from the API spec alone, also stands.

### 21.6 What is original to h3s

Prior art supplied pieces. The assembly is ours:

- k3s product + native Rust control plane + CRI kubelet + youki + trait
  store, as one distro
- Supervisor tunnel implemented in Rust (`h3s-supervisor`)
- nftables-first kube-proxy
- In-process SQLite MVCC *behind* `Storage`, not a separate Kine process
- Compatibility contract that names the two rusternetes shortcuts
  (bollard, SSE) as bugs
- Packaging / extract-on-boot of containerd + youki + CNI in a musl
  `h3s` binary

If a later design conflicts with one of the sources above, **k3s UX and
the Kubernetes wire protocol win.** Prior art does not outrank §6.

### 21.7 youki, containerd, Anvil, Xline

| Source | Take | Do not take as default |
|---|---|---|
| youki / libcontainer | Default OCI runtime | Claim it beats crun on spawn latency |
| containerd | CRI server (supervised / embedded binary) | Rewrite the CRI server in Rust for v1 |
| Anvil / Verus | Optional verified RS/Deploy/STS loops | Required compile; “formally verified control plane” as the v1 claim |
| Xline | `--store=xline` surface in design | Default HA store; not implemented in this line |

### 21.8 How to use this section in review

A change is in scope if it implements a “Take” row against the
contracts in §§6–11.

A change is out of scope if it:

- vendors rusternetes, Krustlet, or Krator as source
- reintroduces bollard, SSE watches, or a parallel type crate
- treats Anvil or Xline as the boot path
- advertises another project’s conformance score
- embeds Facet or HedronDB in the `h3s` binary, or uses HedronDB as
  the kube store (§2.4)

When a design is borrowed after this spec ships, add a row here in the
same keep/drop shape rather than scattering provenance across charter
text.

---


## 22. Companion planes

Adjacent products that sit **beside** Hedronetes are specified in
[`COMPANION_SPEC.md`](./COMPANION_SPEC.md) (h3s-cp): Facet (action),
HedronDB (intent), Herdr (habitat), and Turso/libSQL as an engine option
(not a fifth plane).

The companion is an **overlay**. It does not amend, relax, or override
§§0–21. If a sentence in the companion conflicts with this document,
**this document wins.**

---

*End of specification. Companion files: `COMPANION_SPEC.md` (h3s-cp),
`Cargo.toml` workspace sketch, `architecture.mmd`.*

