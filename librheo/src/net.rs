//! Networking (docs/LIBRHEO.md Phase F) - **deferred, a documented stub**.
//!
//! librheo's networking is designed as async sockets over the *same*
//! submit/complete machinery as `io` (an `Op` -> a completion future), so a
//! service parks a strand on a receive completion exactly like a file read. But
//! the transport underneath - a virtio-net driver, a loopback path, an address/
//! socket kernel object - does not exist yet, and networking is a **service**
//! (docs/ARCHITECTURE.md), not part of the always-linked foundation. Rather than
//! sink Phase F into a network stack, the surface is stubbed here with the
//! intended shape, and the real transport is future work (a `net` service cell
//! over virtio-net, with the socket as an `ObjectKind` and connect/accept as the
//! IO.md-6 cross-cell connect, reusing Phase E's `ipc` mechanism).
//!
//! Nothing here performs I/O; every call reports the feature is unavailable.

/// The error every stubbed networking call returns until the transport lands.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Unsupported;

/// A socket address (the shape a future API takes). Inert today.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct SocketAddr {
    pub ip: [u8; 4],
    pub port: u16,
}

/// Would open an async stream socket. Returns [`Unsupported`] - no transport yet.
pub fn connect(_addr: SocketAddr) -> Result<(), Unsupported> {
    Err(Unsupported)
}

/// Would bind/listen for connections. Returns [`Unsupported`] - no transport yet.
pub fn listen(_addr: SocketAddr) -> Result<(), Unsupported> {
    Err(Unsupported)
}
