# Security Policy

## Supported versions

| Version | Supported |
| --- | --- |
| v0.9.x | Security fixes only |
| v0.10.x (next release line) | Yes |
| older / other tags | Not supported |

Pre-release builds on `main` and feature branches are unsupported; upgrade before reporting against them.

## Reporting a vulnerability

Do **not** open a public GitHub issue for a suspected vulnerability; public disclosure before a fix ships endangers running clusters.

1. Use GitHub's private vulnerability reporting for [hedronetes-h3s](https://github.com/VirtualMachinist/hedronetes-h3s/security/advisories/new). It is encrypted to the maintainers and creates a draft advisory we can publish after the fix.
2. If private reporting is unavailable, contact a maintainer directly through their GitHub profile and request a private channel.

Include: affected version or tag, the exact command or manifest that reproduces the issue, observed vs. expected behavior, and any logs with credentials redacted.

We acknowledge reports within a few business days and aim to publish a patch, a tagged release, and the advisory together.

## Coordinated disclosure

Please keep findings private until the patched release and advisory are published. We credit reporters in the advisory on request (opt-in, by name or handle).

## Scope and honest boundaries

This project ships a Kubernetes-compatible cluster with an explicit subset and explicit limits. The following are known design boundaries, not vulnerabilities in themselves — a report that races one of them for privilege escalation, however, is in scope:

- No embedded Go control plane; the API surface is the focused set listed in `README.md` ("Implemented API").
- Every Pod is managed under one runtime profile (`restricted-v1`-style: non-root UID, dropped ALL capabilities, no privilege escalation, RuntimeDefault seccomp). Pods run in a more privileged shape cannot run.
- The default Pod path does not issue in-cluster ServiceAccount tokens; on the hard target (`v1.0.0`/M2) that becomes bound tokens with the Node authorizer and admission sharing one contract.
- Multi-server clustering, HA store backends, and official conformance are the `v1.0.0`/M2 milestone, not shipped defaults.

Secrets (data dir contents, certificates, tokens, and CI configuration) are out of scope unless the defect exposes them to other cluster actors. Infrastructure of the maintainers' lab hosts is out of scope entirely.
