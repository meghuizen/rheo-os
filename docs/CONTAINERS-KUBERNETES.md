# Containers and Kubernetes

**Status:** Draft v0.1. Expands ARCHITECTURE.md 4.9.

Position: Kubernetes is not ported; it is **absorbed**. Its genuinely great
idea - declarative desired state, reconciliation loops, an extensible typed
API - becomes the OS's control-plane personality. Its other half - a pile of
compensation for Linux missing primitives - is deleted because the
primitives exist natively.

## 1. The container primitive

(The substrate capacity to actually run this - cells and threads past the
current fixed caps, per-bundle budgets, vcores, the CRI-shaped runtime - is
owned by docs/SUBSTRATE.md pillar 8.)

- A **cell** is the container: one address space, one capability set, its
  queues. No namespaces stack, no cgroups, no seccomp - isolation is not
  assembled from seven mechanisms; it is the native unit.
- A **cell group** replaces the pod: cells placed together, sharing a
  capability bundle (queues, memory grants) and living under one lease.
  Sidecar patterns become either library code or separate cells with narrow
  grants into the group - and the biggest sidecar (the service mesh) is
  deleted outright because identity and mTLS are native.
- **Images** are content-addressed sealed object sets. OCI images import
  through a converter (layers flattened into sealed objects, dedup by hash);
  native images are just manifests of hashes. Image identity = code identity
  = what attestation vouches for (SECURITY-IDENTITY.md section 1).

## 2. What disappears, and why

| Kubernetes component | Replaced by |
|---|---|
| kubelet + containerd/CRI | The host reconciler (PID 1) running cells natively |
| CNI, kube-proxy, Service VIPs | Identity-resolved, capability-gated queue endpoints (NETWORKING.md 6) |
| Service mesh sidecars | Native workload identity + mTLS + policy-as-grants |
| Secrets in etcd | Capabilities minted to attested identities (SECURITY-IDENTITY.md 5) |
| NetworkPolicy, PSP/PSA, most RBAC | Capability issuance; admission = the moment grants are minted or refused |
| Device plugins / DRA | Engines and typed memory in the topology graph, first class |

## 3. What is kept and promoted

- **The state plane:** a distributed desired-state store replaces etcd + API
  server. Records are typed objects (real IDL schemas, not YAML-plus-
  annotations), access is capability-scoped per record set, versions are
  HLC-ordered, and **watch is a completion queue** - controllers receive
  changes through the same async mechanism as all I/O.
- **Controllers are ordinary cells** holding exactly two kinds of grants:
  watch this slice of desired state, write that slice of actual state. The
  operator/CRD model survives fully - minus today's every-operator-is-
  cluster-admin problem, because an operator cannot exceed its grants.
- **Namespaces become sub-trust-domains:** a tenant is a branch of the
  identity tree with delegated, bounded capability-minting rights.
  Multi-tenancy is cryptographic, not label-based.

## 4. Scheduling and workloads

- The placement engine consumes the full topology graph (engines, memory
  kinds, NVLink/fabric islands, network distance) and supports
  **all-or-nothing gang placement** natively - what Volcano/Kueue bolt on is
  core, because AI training jobs are a primary customer.
- Workload types: cell-sets (Deployments/Jobs analog), cron, and the
  **graph job** - a dependency graph spanning hosts (compute nodes, RDMA
  transfer nodes, collectives) scheduled as one unit. Kubernetes has no such
  concept; frameworks fake it above the API.
- "This service is the most important" is a **resource contract**, not a
  priority: reservations across CPU, memory floors, I/O bandwidth, and
  residency, admission-checked once (ARCHITECTURE.md 4.2).
- Drains, rollouts, and migration are explicit checkpoint/restore or
  kill-and-reschedule operations - priced honestly, never transparent.

## 5. The compatibility edge

A translator speaks the real Kubernetes API outward so kubectl, Helm, and
GitOps tooling work from day one.

| Kubernetes API object | Maps to |
|---|---|
| Pod | Cell group |
| Deployment/Job/CronJob | Cell-set / graph-job controllers |
| Service | Identity-named queue endpoint in discovery |
| Secret/ConfigMap | Minted capability / typed state object |
| NetworkPolicy | Grant policy (compiled to issuance rules) |
| ResourceQuota/LimitRange | Capability budgets |
| Node | Host + its attestation evidence |

Known translation losses, stated: hostPath, privileged pods, hostNetwork,
node SSH-as-admin, and tooling that assumes pod IPs or a Linux /proc
underneath (node exporters, some CNI-era operators). These are the price of
deleting the compensation layers; the edge documents each with its nearest
native equivalent.

## 6. Metering and autoscaling

Every grant carries budget and usage counters, so autoscalers and billing
read real per-capability numbers instead of sampling /proc. Scale signals
(queue depths, reservation misses, pressure events) come from the event
stream with flow context - the autoscaler is one more controller cell.

## 7. Failure semantics

Leases do the work: a cell group's lease expiring *is* the eviction; a host
lease expiring *is* node failure; controllers observe expiry events and
reconcile. Partition behavior follows the cluster fundamentals
(ARCHITECTURE.md 4.8): the data plane keeps working within a partition until
leases expire; only membership and placement need consensus.
