---
name: Bug report
about: Report a defect against h3s
title: ""
labels: bug
assignees: ""
---

**Version**

- h3s release/tag, or commit SHA if built from source:
- Host OS / arch:

**What you did**

Exact command(s) and manifest(s) — redact tokens, certs, and any secret values.

**What happened**

Observed behavior: logs, `kubectl` output, exit codes.

**What you expected**

Expected behavior.

**Honesty note**

h3s ships an explicit subset (see [SPEC.md §16](../SPEC.md)). Anything listed there as "owed" or "in development" (NodePort, LoadBalancer, local-path PVC, CoreDNS, HA, conformance) is a **known limitation, not a bug** — use the feature request template unless the defect contradicts documented behavior.
