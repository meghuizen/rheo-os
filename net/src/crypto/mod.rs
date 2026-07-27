//! rheo-net crypto primitive layer (docs/NETSTACK.md §3, Phase N3a) - the
//! foundation the security transports (TLS 1.3 / WireGuard / IPsec, N3+) build
//! on. It follows the **hybrid** posture: the ChaCha20-Poly1305 AEAD is
//! **from-scratch** on our own ChaCha20 (`kernel/src/rng/chacha.rs` lineage) and
//! a from-scratch Poly1305; the rest lean on **doc-named audited RustCrypto
//! crates** (each named in NETSTACK.md §3, pinned, `no_std`, `default-features =
//! false`), wrapped behind rheo-net's own API so the external surface stays
//! native. Every primitive is proven against its **RFC/NIST test vector** on all
//! three ISAs by the `netcrypto` proof.
//!
//! Inventory (crate-backed vs from-scratch):
//! - **from-scratch**: [`chacha`] (ChaCha20 block/keystream), [`poly1305`]
//!   (the 130-bit MAC), [`chachapoly`] (the ChaCha20-Poly1305 AEAD).
//! - **crate-backed**: [`hash`] (SHA-256/384 - `sha2`), [`kdf`] (HKDF-SHA256 -
//!   `hkdf`), [`kx`] (X25519 - `x25519-dalek`), [`sign`] (Ed25519 -
//!   `ed25519-dalek`), [`aesgcm`] (AES-128/256-GCM - `aes-gcm`).
//!
//! **Two randomness classes, never conflated** ([`rand`] + [`kdf`]): a fast
//! public-only side-stream ([`rand::PublicRandom`], for nonces/cookies/txids) vs
//! key schedules ([`kdf`], HKDF over a transcript keyed from the attested per-cell
//! DRBG). The types have no bridge - public randomness cannot become a key.
//!
//! **Nonce-reuse hazard** ([`aead`]): real AEAD use goes through
//! [`aead::SealingKey`], which owns a monotonic nonce counter (a nonce cannot be
//! replayed) and refuses to seal after a fork/restore until reseeded.
//!
//! Deferred to N3b (documented, NETSTACK.md §14): the TLS 1.3 handshake state
//! machine + record layer, X.509 certificate parsing/validation, and the
//! WireGuard/IPsec protocol machinery. N3a is the vetted **primitives** only.

pub mod aead;
pub mod aesgcm;
pub mod chacha;
pub mod chachapoly;
pub mod hash;
pub mod kdf;
pub mod kx;
pub mod poly1305;
pub mod rand;
pub mod sign;

pub use aead::{Aead, NonceError, OpeningKey, Sealed, SealingKey};
pub use chachapoly::ChaCha20Poly1305;
