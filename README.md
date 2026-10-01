# Hedronetes (h3s)

> **Status:** In production use as daily-driver / dogfood agentic node beside K8s/k3s (v0.11.0). Hardening: further stress testing before broad production recommend; durable HA + conformance targeted for v1.0.0 (the two-server Postgres store on this line is not HA). Not a toy reference.


<p align="center">
  <img src="assets/hedronetes-seal-dark.jpeg" alt="Hedronetes (h3s) product mark" width="220" />
</p>

<p align="center">
  <a href="LICENSE"><img src="https://img.shields.io/badge/License-Apache--2.0-C9A227?style=flat&colorA=111111" alt="Apache-2.0" /></a>
  <a href="https://github.com/Hedronite/hedronetes-h3s/releases/tag/v0.11.0"><img src="https://img.shields.io/badge/Release-v0.11.0-C9A227?style=flat&colorA=111111" alt="Release v0.11.0" /></a>
  <a href="https://github.com/VirtualMachinist/hedronetes-h3s/actions/workflows/ci.yml"><img src="https://img.shields.io/github/actions/workflow/status/VirtualMachinist/hedronetes-h3s/ci.yml?style=flat&label=CI&colorA=111111&color=C9A227" alt="CI" /></a>
  <a href="https://www.rust-lang.org"><img src="https://img.shields.io/badge/Rust-0042DB?style=flat&colorA=111111&logo=rust&logoColor=C9A227" alt="Rust" /></a>
</p>

## Add an agentic node to the cluster you already run.

**Hedronetes (h3s)** is an agentic runtime plane you **add** to Kubernetes.

h3s complements **Kubernetes and k3s**. It is not a migration off your cluster.

Teams on EKS, GKE, AKS, or any stock kube API keep their control plane. They join an **h3s node** (or a small operator-managed pool) so agents get a tight, Rust-native runtime with stock `kubectl` — without rewriting the cluster.

> Kubernetes-compatible. Rust-native. Built to sit **beside** your existing control plane — including k3s — not to replace it.

Status: **0.11.0** · Apache-2.0 · API target Kubernetes **v1.34** · Linux amd64 / arm64

## What this is

- A **Kubernetes-compatible** distribution: `h3s server` / `h3s agent`, stock `kubectl` and Helm against a focused API surface. One `h3s server` runs on SQLite (shipped default). More than one `h3s server` shares one Postgres primary via `--store=postgres` (shipped on `512f220de42ed989f937ad7625a7de732247797b`; `PostgresStore` on `tokio-postgres` shipped on `174367d34e643b78a3e5a654b02e460de5ada3bf`). etcd, MySQL, and Xline are not implemented. `h3s agent` has no datastore.
- Control plane and kubelet path are **native Rust** (no embedded Go Kubernetes).
- **Complement posture:** run h3s as an **agentic node / pool inside a larger Kubernetes cluster** (EKS/GKE/…) **or** as its own small cluster for lab and CI. Same product; different seat.

## What this is not

- **Not** “rip out EKS/GKE and move to h3s.”
- **Not** a claim of full Kubernetes conformance (see [Implemented API](#implemented-api); StatefulSet/Job/PVC/NetworkPolicy still out).
- **Not** federation-as-default (optional enterprise packaging later — not the default install story).

## How EKS/GKE teams use it

1. Keep the managed control plane and existing workloads.
2. Add an **h3s agentic node** (Virtual Kubelet–style custom node) **or** a light **RuntimeClass / dedicated pool** — operators and AgentFleet CRDs as the richer path when ready.
3. Schedule agent workloads onto that plane with ordinary `kubectl`. Shared platform services (API recipe store, durable run history) live as normal cluster services — not one PVC glued to every agent pod.

### Deployment patterns

| Pattern | Role | When |
| --- | --- | --- |
| **A — agentic node** | h3s registers as a Node; stock scheduler places Pods with selectors/affinity | **Primary** — join the cluster you already run |
| **C — labeled pool** | Label/taint dedicated Nodes; the scheduler places agent workloads by nodeSelector, nodeAffinity, and tolerations | **Day-0 on-ramp** before a custom node joins |
| **B — operator + CRDs** | AgentFleet-style lifecycle above raw Pods | **Grow-up** when workloads need richer control |
| **Nested h3s** | Team sandbox API inside the stock cluster | **Middle** option — explicit advanced chapter |
| **Federation** | Multi-cluster views | **Enterprise only** — not the default README path |

## Relationship to k3s

- **Lineage:** same “small cluster, one binary” idea as k3s.
- **Posture:** h3s can stand alone like a compact distribution **and** can join a wider kube estate as the agentic plane. Choosing h3s does **not** mean abandoning k3s or upstream Kubernetes.

Inspired by the k3s single-binary shape; reimplemented in Rust without embedding the Go control plane. See [`SPEC.md`](./SPEC.md) for design detail.

## Lab / standalone cluster (secondary)

For CI, labs, and small clusters: run `h3s server` and `h3s agent` as a self-contained binary pair (see [Binary](#binary)). This path exercises the full control plane. Production teams on EKS/GKE typically start by joining an existing cluster instead.

## CI boundaries

GitHub CI on `ubuntu-latest` runs three jobs (no Sonobuoy, no conformance suites):

- **`cargo build and test`** (`build` job) on the current Rust toolchain: `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets --all-features --locked -- -D warnings`, `cargo build --workspace --locked`, and `cargo test --workspace --locked`.
- **Rust 1.88 minimum support** (`minimum-rust` job): `cargo +1.88.0 test --workspace --locked`.
- **Dependency advisories** (`dependency-advisories` job): `cargo deny check advisories`.

CI proves the Rust workspace compiles, lints, and unit/integration-tests that run in-process. It does **not** run the working cluster topology: two-node server+agent with containerd, Flannel, and the nftables proxy is the **documented lab** — see [`server-agent`](./docs/server-agent.md), [`supervisor-tunnel`](./docs/supervisor-tunnel.md), and [`service-networking`](./docs/service-networking.md); the harness lives in `integration/tower/`. A change can pass CI and still need a lab check at release time.

## Docs

- Full product specification: [`SPEC.md`](./SPEC.md).
- Security: how to report vulnerabilities — [`SECURITY.md`](./SECURITY.md).
- Design and checkpoint notes under [`docs/`](./docs): [`addons`](./docs/addons.md) · [`server-agent`](./docs/server-agent.md) · [`workload-api`](./docs/workload-api.md) · [`workload-controllers`](./docs/workload-controllers.md) · [`scheduler`](./docs/scheduler.md) · [`pod-runtime`](./docs/pod-runtime.md) · [`admission`](./docs/admission.md) · [`serviceaccounts`](./docs/serviceaccounts.md) · [`service-networking`](./docs/service-networking.md) · [`service-endpoints`](./docs/service-endpoints.md) · [`node-access`](./docs/node-access.md) · [`node-cidr-allocation`](./docs/node-cidr-allocation.md) · [`strategic-patches`](./docs/strategic-patches.md) · [`configuration-volumes`](./docs/configuration-volumes.md) · [`security-foundation`](./docs/security-foundation.md) · [`supervisor-tunnel`](./docs/supervisor-tunnel.md) · [`worker-bootstrap`](./docs/worker-bootstrap.md) · [`cri-runtime`](./docs/cri-runtime.md) · [`flannel-cni`](./docs/flannel-cni.md) · [`flannel-packaging`](./docs/flannel-packaging.md) · [`api-foundation`](./docs/api-foundation.md). Documents titled “checkpoint” are historical build-phase records; see [`SPEC.md`](./SPEC.md) §16 for the current roadmap.

## Implemented API

Stock `kubectl` and Helm work against these kinds only (Kubernetes **v1.34** wire format):

| Group | Version | Kind | Scope |
| --- | --- | --- | --- |
| core | v1 | Namespace | cluster |
| core | v1 | ConfigMap | namespaced |
| core | v1 | Secret | namespaced |
| core | v1 | Pod | namespaced |
| core | v1 | Node | cluster |
| core | v1 | Service | namespaced |
| core | v1 | ServiceAccount | namespaced |
| apps | v1 | Deployment | namespaced |
| apps | v1 | ReplicaSet | namespaced |
| discovery.k8s.io | v1 | EndpointSlice | namespaced |
| coordination.k8s.io | v1 | Lease | namespaced |
| rbac.authorization.k8s.io | v1 | Role | namespaced |
| rbac.authorization.k8s.io | v1 | RoleBinding | namespaced |
| rbac.authorization.k8s.io | v1 | ClusterRole | cluster |
| rbac.authorization.k8s.io | v1 | ClusterRoleBinding | cluster |

StatefulSet, Job, DaemonSet, PVC, and NetworkPolicy are **not shipped**; they are in development.

## Supported workloads

### Pod (`restricted-v1`)

The API server and kubelet enforce one runtime profile on every Pod (Pod Security):

- `automountServiceAccountToken: false`
- `enableServiceLinks: false`
- Explicit non-root UID with dropped ALL capabilities, `allowPrivilegeEscalation: false`, and RuntimeDefault seccomp

Published example:

```bash
kubectl apply -f examples/supported-pod.yaml
```

### Service

| Type | Admitted | Dataplane |
| --- | --- | --- |
| ClusterIP | yes | IPv4 TCP/UDP nftables proxy |
| Headless (`clusterIP: None`) | yes | no virtual IP (in development) |
| ExternalName | yes | no dataplane rules |
| NodePort / LoadBalancer | LoadBalancer shipped on `20abf97662a0c191ec1ba7ede78820ed9d9c6e02`; NodePort not shipped (in development) | LoadBalancer via ServiceLB; NodePort — |

## Add-ons

- Flavor follows k3s: optional cluster add-ons manifest-managed by the server, each skippable with `--disable=`.
- **Shipped today:** `h3s server --disable-agent`, and the packaged add-ons **Traefik + ServiceLB** (default-on; `--disable=traefik,servicelb` skips both — see [Shipped](#shipped)).
- **CoreDNS:** **not shipped**. The cluster PKI issues the `system:coredns` client identity, so CoreDNS can be run manually against a lab cluster, but no add-on manager applies it today.
- **`--disable=`** (k3s-style, comma-separated add-on names): shipped for Traefik and ServiceLB; see the add-on table in `SPEC.md` §12.
- **Flannel:** an explicit host daemon (systemd unit via the Nix flake), not a h3s add-on and never a DaemonSet. Run it per node yourself. See [`docs/addons.md`](./docs/addons.md).

## Platform services (Facet, Geode, HedronDB)

Deploy as **shared cluster services** — not per-agent PVCs:

- **[Facet](https://github.com/VirtualMachinist/facet)** — API recipe and run-history client. Agents call the cluster, and Geode, through Facet. `geode agent serve` is already driven from a Facet collection.
- **[Geode](https://github.com/Hedronite/geode)** — custody. Secret encryption at rest uses Geode `seal` / `open` (GDE1). h3s does not grow a second cipher.
- **[HedronDB](https://github.com/VirtualMachinist/hedrondb)** — durable intent store with HQL.

Agents reach them via normal Services and workload identity; platform durability does not require mounting a store PVC into every agent Pod.

## Binary

```text
h3s server   # control plane + datastore + supervisor (+ embedded agent)
h3s agent    # kubelet + kube-proxy + CNI + runtime + tunnel client (no datastore)
```

```bash
cargo run -p h3s -- --help
cargo run -p h3s -- server --help
cargo run -p h3s -- agent --help
```

## Shipped

The first three ship in tag `v0.11.0`. They were not in tag `v0.10.0`. The store rows land on the postgres line after `v0.11.0`; they are not a tag claim. Each row cites the commit that shipped the behavior.

- **Traefik and ServiceLB.** Shipped on `20abf97662a0c191ec1ba7ede78820ed9d9c6e02`, merged `1fe27f507fe249b00473f79ae20a1f66870ced88`. Default-on packaged add-ons: a LoadBalancer Service receives an address, an Ingress is served, and `--disable=traefik,servicelb` leaves both off.
- **Server-Side Apply.** Shipped on `c3fd6b5be3649ee4cea272d699f4f7174502d990`, merged `c76d4f6d87bf1795780af3857dfb21e397c7ffad`. Field managers; `kubectl apply --server-side` works.
- **Secrets encryption at rest.** Shipped on `43548fc5de88374dc2c9a8bb4d8998aae3808dbc`, merged `beaad03297300a9d84508cedd242163427797247`. Secret payloads sealed with Geode `seal` / `open`; Facet is the agent path to that vault; h3s grows no second cipher.
- **Multi-server Postgres store.** `PostgresStore` on `tokio-postgres` shipped on `174367d34e643b78a3e5a654b02e460de5ada3bf`: a revision, conflict on a stale `resourceVersion`, watch resume, compaction failing the old watch, and a Geode seal whose plaintext is absent from a raw SQL read and returned by `open`.
- **`--store=postgres`.** Shipped on `512f220de42ed989f937ad7625a7de732247797b`: requires `--datastore-endpoint` with a `postgres://` URL and talks to one Postgres primary; two `h3s server` processes share it — a create on the first is read on the second. SQLite stays the one-server default. `--secrets-encryption` on that store uses the existing Geode sealer. etcd, MySQL, and Xline are not implemented; `h3s agent` has no datastore.

## In Development

- Full Kubernetes conformance and durable high availability multi-control-plane (see [Status](#status)).
- StatefulSet, Job, DaemonSet, and NetworkPolicy in core binary.
- Ingress via Traefik shipped (see [Shipped](#shipped)); mesh and GitOps in development.
- Seamless Kubernetes integration.

## Status

**v0.9.0** is the first release that actually runs a cluster. **v0.9.1** is the structure substrate (split API dispatch, one PodRuntimeProfile, restarting supervisor). **v0.10.0** is the k3s-shaped single-server release: default Pods, bound ServiceAccount tokens, ClusterIP and NodePort, local-path PVC, and an API that survives controller death. One SQLite server. Not HA. **v0.11.0** adds Server-Side Apply, opt-in Geode secrets encryption, and default-on Traefik and ServiceLB. Store posture: one `h3s server` uses SQLite (shipped default); more than one `h3s server` shares one Postgres primary with `--store=postgres` plus `--datastore-endpoint` (shipped on `512f220de42ed989f937ad7625a7de732247797b`; `PostgresStore` on `tokio-postgres` shipped on `174367d34e643b78a3e5a654b02e460de5ada3bf`; two servers, no HA — no leader election, no automatic failover, no read replica); `--secrets-encryption` on that store seals Secret payloads with Geode; etcd, MySQL, and Xline stay not implemented; `h3s agent` has no datastore. Jev, HedronDB, and pgvector are companion products, not h3s store features. Further stress testing is still needed before recommending it for production despite internal use. **Durable high availability with Kubernetes conformance ships with v1.0.0.**

A multi-node h3s cluster — native `server` + separate `agent` — runs workloads with stock `kubectl` and Helm. Proven on Colima VMs running NixOS, Debian, and Fedora.

## License

Apache-2.0
