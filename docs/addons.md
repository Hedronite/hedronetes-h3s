# Add-ons: CoreDNS, the disable flag, and the Flannel host daemon

This page documents the add-on shape of h3s against what the code at
`feat/v0.10` ships today. Items marked **not shipped** are the v0.10
contract, not present-tense behavior.

## What ships now

- `h3s server --disable-agent` starts the control plane + datastore +
  supervisor without the embedded local agent. This is the only disable
  flag that exists in the binary today.
- Flannel is an explicit **host daemon**, not a Kubernetes add-on: each
  node runs the Flannel systemd unit (via the Nix flake). h3s does not
  embed, deploy, or manage it. See [`flannel-cni`](flannel-cni.md) and
  [`flannel-packaging`](flannel-packaging.md).
- The cluster PKI already issues the `system:coredns` client identity
  (`h3s-certs`), so a CoreDNS instance can authenticate against the API
  when one is run manually against a lab cluster.

## Not shipped (in development)

- **CoreDNS as a packaged in-cluster add-on.** The add-on manager that
  would apply CoreDNS manifests on server start does not exist yet, and
  no `h3s server` deployment today includes DNS.
- **`--disable=`** in the k3s sense (comma-separated add-on names, e.g.
  `--disable=coredns`) does not exist yet. When the add-on model lands,
  the flag disables the packaged add-ons by name; until then there is
  nothing for it to disable, and the honest flag set is the clap help of
  the current binary.

## Why this split

k3s ships CoreDNS, ServiceLB, Traefik, and local-path-provisioner as
manifests under an internal addon manager, each skippable with
`--disable=`. h3s is adopting the same shape on the v0.10.0/k3s-shaped
single-server line ([`SPEC.md`](../SPEC.md) §16), but the binaries treat
the flag layer as a claim only the code can verify: documentation must
not mark an add-on as running when the manager is not in the binary.
Host-daemon Flannel stays explicit because CNI on the node is outside
the control plane's blast radius by design.
