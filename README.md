# Hedronetes (h3s)

> **Status:** In production use as daily-driver / dogfood agentic node beside K8s/k3s (v0.9.1). Hardening: further stress testing before broad production recommend; durable HA + conformance targeted for v1.0.0. Not a toy reference.


<p align="center">
  <img src="assets/hedronetes-seal-dark.jpeg" alt="Hedronetes (h3s) product mark" width="220" />
</p>

<p align="center">
  <a href="LICENSE"><img src="https://img.shields.io/badge/License-Apache--2.0-C9A227?style=flat&colorA=111111" alt="Apache-2.0" /></a>
  <a href="https://github.com/VirtualMachinist/hedronetes-h3s/releases/tag/v0.9.1"><img src="https://img.shields.io/badge/Release-v0.9.1-C9A227?style=flat&colorA=111111" alt="Release v0.9.1" /></a>
  <a href="https://github.com/VirtualMachinist/hedronetes-h3s/actions/workflows/ci.yml"><img src="https://img.shields.io/github/actions/workflow/status/VirtualMachinist/hedronetes-h3s/ci.yml?style=flat&label=CI&colorA=111111&color=C9A227" alt="CI" /></a>
  <a href="https://www.rust-lang.org"><img src="https://img.shields.io/badge/Rust-0042DB?style=flat&colorA=111111&logo=rust&logoColor=C9A227" alt="Rust" /></a>
</p>

## Add an agentic node to the cluster you already run.

**Hedronetes (h3s)** is an agentic runtime plane you **add** to Kubernetes.

h3s complements **Kubernetes and k3s**. It is not a migration off your cluster.

Teams on EKS, GKE, AKS, or any stock kube API keep their control plane. They join an **h3s node** (or a small operator-managed pool) so agents get a tight, Rust-native runtime with stock `kubectl` — without rewriting the cluster.

> Kubernetes-compatible. Rust-native. Built to sit **beside** your existing control plane — including k3s — not to replace it.

Status: **0.9.1** · Apache-2.0 · API target Kubernetes **v1.34** · Linux amd64 / arm64

## What this is

- A **Kubernetes-compatible** distribution: `h3s server` / `h3s agent`, SQLite by default, stock `kubectl` and Helm against a focused API surface.
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
| **C — RuntimeClass / pool** | Label/taint a node pool; `restricted-v1` agents on stock runtime | **Day-0 on-ramp** before a custom node joins |
| **B — operator + CRDs** | AgentFleet-style lifecycle above raw Pods | **Grow-up** when workloads need richer control |
| **Nested h3s** | Team sandbox API inside the stock cluster | **Middle** option — explicit advanced chapter |
| **Federation** | Multi-cluster views | **Enterprise only** — not the default README path |

## Relationship to k3s

- **Lineage:** same “small cluster, one binary” idea as k3s.
- **Posture:** h3s can stand alone like a compact distribution **and** can join a wider kube estate as the agentic plane. Choosing h3s does **not** mean abandoning k3s or upstream Kubernetes.

Inspired by the k3s single-binary shape; reimplemented in Rust without embedding the Go control plane. See [`SPEC.md`](./SPEC.md) for design detail.

## Lab / standalone cluster (secondary)

For CI, labs, and small clusters: run `h3s server` and `h3s agent` as a self-contained binary pair (see [Binary](#binary)). This path exercises the full control plane. Production teams on EKS/GKE typically start by joining an existing cluster instead.

## Docs

- Full product specification: [`SPEC.md`](./SPEC.md).

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

StatefulSet, Job, DaemonSet, PVC, and NetworkPolicy are in development.

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
| NodePort / LoadBalancer | in developent | — |

## Platform services (Facet + HedronDB)

Deploy as **shared cluster services** — not per-agent PVCs:

- **[Facet](https://github.com/VirtualMachinist/facet)** — API recipe / run-history client (Lattice).
- **[HedronDB](https://github.com/VirtualMachinist/hedrondb)** — durable intent store with HQL.

Agents reach them via normal Services and workload identity; platform durability does not require mounting a store PVC into every agent Pod.

## Binary

```text
h3s server   # control plane + datastore + supervisor (+ embedded agent)
h3s agent    # kubelet + kube-proxy + CNI + runtime + tunnel client
```

```bash
cargo run -p h3s -- --help
cargo run -p h3s -- server --help
cargo run -p h3s -- agent --help
```

## In Development

- Full Kubernetes conformance and durable high availability multi-control-plane (see [Status](#status)).
- StatefulSet, Job, DaemonSet, PVC, and NetworkPolicy in core binary.
- Ingress, mesh, and GitOps.
- Seamless Kubernetes integration.

## Status

**v0.9.0** is the first release that actually runs a cluster. **v0.9.1** is the structure substrate (split API dispatch, one PodRuntimeProfile, restarting supervisor). This product works; further stress testing is needed before recommending it for production despite internal use. **Durable high availability with Kubernetes conformance ships with v1.0.0.**

A multi-node h3s cluster — native `server` + separate `agent` — runs workloads with stock `kubectl` and Helm. Proven on Colima VMs running NixOS, Debian, and Fedora.

## License

Apache-2.0
