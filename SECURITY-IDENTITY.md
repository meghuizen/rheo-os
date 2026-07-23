# Security and Identity

**Status:** Draft v0.1. Expands ARCHITECTURE.md doctrines 1, 6, 7 and
subsystem 4.8.

The model in one sentence: **who you are is proven by attestation, and what
you may do is exactly the set of capabilities minted to that identity** -
there are no users, no root, no ambient permissions, and network policy is a
consequence of identity, not of IP addresses.

## 1. The identity tree (SPIFFE, made native)

SPIFFE's concepts map one-to-one; the difference is that here the OS itself
is the implementation, not an agent bolted on.

| SPIFFE concept | Lattice native form |
|---|---|
| Trust domain | The cluster's root of trust; sub-trust-domains are tenant namespaces with bounded minting rights |
| SPIFFE ID | Identity path: `trust-domain / tenant / workload`, bound to a cell |
| SVID (the credential) | Short-lived certificate/token issued to a cell after host attestation vouches for it |
| Workload attestation | The host kernel *is* the attestor: it measured the cell's image (content-addressed, so image hash = provable code identity) |
| SPIRE server/agent | The trust domain registration service (control plane) + each host's reconciler |

Chain of vouching: hardware root (TPM/DICE) -> measured boot -> host identity
-> host attests each cell it runs -> cell identity. Any peer can verify the
whole chain offline; no central broker sits in the data path.

## 2. Capabilities: the single mechanism

- **Local form:** an unforgeable kernel handle - typed, delegatable,
  epoch-versioned, budget-metered. Rust ownership makes handle lifecycle a
  compile-time property in system code (ARCHITECTURE.md section 7).
- **Cryptographic form (crossing hosts):** a signed, scoped, expiring token
  in deterministic CBOR (canonical encoding is mandatory - signatures over
  ambiguous encodings are a bug class). Attenuation is Biscuit/Macaroon-
  style: anyone holding a token can derive a *narrower* one offline; nobody
  can widen one. The receiving kernel verifies and converts it back into a
  local handle.
- **Monotonic attenuation** is the invariant the formal proofs cover
  (ARCHITECTURE.md 8.2): delegation never adds rights.

## 3. Revocation - honest semantics

Instant global revocation does not exist in a distributed system, so Lattice
does not pretend:

- Every cryptographic capability carries a **short TTL** (minutes-scale by
  default) and an **epoch**. Revocation = invalidating an epoch; propagation
  is bounded by TTL plus epoch-gossip latency.
- Leases self-fence: a host whose clock error bound e grows, or that loses
  contact with membership, sees its effective lease windows shrink and stops
  acting on expiring grants (fencing tokens make stale actors detectable at
  the resource).
- The stated contract everywhere: revocation is **eventual within a bounded
  window**, and the window is a queryable number, not folklore.

## 4. No users, no root

- UID 0 does not exist. Human access is an identity like any other: an SSH
  login authenticates a person, and a session cell is minted with that
  identity's entitled capability set (POSIX-PERSONALITY.md).
- "sudo" becomes **grant escalation**: a request to a policy service that may
  require approval or a second factor, minting additional short-lived
  capabilities. The escalation is itself an audit event.
- Operator break-glass exists as a pre-provisioned, heavily audited identity
  class with wide but still enumerated grants - wide is allowed; ambient is
  not.

## 5. Secrets

A secret is not a file in a store; it is a **capability minted to an attested
identity and delivered on its queue**. Consequences:

- Nothing sits at rest in a central database waiting to be exfiltrated
  (compare etcd-stored Kubernetes Secrets).
- Rotation is re-minting; expiry is the default state.
- TLS private keys for inline NIC crypto are programmed per queue via a key
  capability and are never readable back by the cell (NETWORKING.md).
- Where hardware supports it, cell-bound keys live in the TPM/TEE and only
  operations (sign/decrypt) are granted.

## 6. Network policy = capability issuance

- Internal (east-west) reachability requires a queue-pair grant; services
  without your capability are not addressable at all - there is no internal
  IP surface to scan or flood.
- mTLS identity, not source address, authenticates every fabric connection.
  "NetworkPolicy" as a runtime firewall check disappears; the question is
  whether the grant was ever minted.
- The internet edge is the exception, handled by gateway cells with explicit
  edge grants and the DDoS pipeline (NETWORKING.md section 5).

## 7. Audit

The grant chain **is** the audit log: every mint, delegation, escalation,
denial, and revocation is a typed event with flow context on the event
stream (ARCHITECTURE.md 4.10), HLC-ordered and capability-scoped for
readers. "Who could ever have reached X, and via which delegations" is a
query, not forensics.

## 8. Threat model summary

In scope, by construction:

- Compromised workload: bounded to its grants and budgets; lateral movement
  requires stealing *minted* capabilities, which are short-lived and scoped.
- Compromised vendor driver/blob: contained cell, device-only grants,
  no network, no other memory (the QCE doctrine).
- Malicious tenant: sub-trust-domain cannot name other tenants' objects;
  resource exhaustion is bounded by metered budgets.
- Device DMA attacks: every DMA is IOMMU-mediated per queue.
- Network attacker: identity-gated fabric; edge absorbs floods at hardware
  drop cost.
- Stolen disk / at-rest: content-addressed sealed objects + per-object
  encryption keys held as capabilities.

Explicitly out of scope initially (stated, not hidden):

- Physical attackers with bus interposers (until confidential-compute hosts,
  see BOOT.md section 6 and TARGET-ARCHITECTURES.md section 6).
- Microarchitectural side channels beyond the structural mitigations already
  in the design (SMT only within a cell, no shared entropy pool, bandwidth
  partitioning); full side-channel hardening is a per-deployment policy tier.
- A malicious trust-domain root: the root can mint anything; protecting the
  root is organizational plus HSM practice, not kernel magic.

## 9. What must be verified first

The capability core (mint/attenuate/revoke/check) and the attestation chain
are the two components everything else trusts. They get the formal-proof
budget before anything else is built (ARCHITECTURE.md 8.2), and the
cryptographic token codec gets continuous structure-aware fuzzing as the
single most attacked surface in the system.
