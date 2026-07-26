//! A full TLS 1.3 1-RTT handshake driven **in-cell** (RFC 8446 §2, §4) -
//! docs/NETSTACK.md §15. [`run_handshake`] plays both a client and a server
//! endpoint over an in-process byte path: it exchanges ClientHello/ServerHello,
//! runs the [`keyschedule`] to matching handshake + application traffic keys on
//! both sides, and completes the authenticated flight - EncryptedExtensions,
//! Certificate, CertificateVerify (Ed25519), and both Finished MACs - with the
//! client verifying the certificate signature, the CertificateVerify signature,
//! and the server Finished, then sending its own Finished for the server to
//! verify. Every post-ServerHello message rides the [`record`] AEAD layer.
//!
//! This is deterministic and network-free (the N2a/N3a proof philosophy): fixed
//! ephemerals make it reproducible, no NIC is touched. A live handshake to a
//! remote peer is deferred (N3c/N4) - there is no deterministic TLS server under
//! SLIRP.

use alloc::vec::Vec;

use super::keyschedule::{self, Secret};
use super::msg::{self, HandshakeType, Transcript};
use super::record::{ContentType, RecordKeys};
use super::{CipherSuite, TlsError};
use crate::crypto::{kx, sign};

/// The server's identity: its X.509 certificate (DER) and the Ed25519 secret seed
/// whose public key is the certificate's subject public key. The server signs the
/// CertificateVerify with this seed.
pub struct ServerIdentity {
    pub cert_der: Vec<u8>,
    pub ed25519_seed: [u8; 32],
}

/// The result of a completed handshake: the negotiated suite, the four
/// application-data record key streams (client/server x write/read - a write on
/// one side shares the secret with the read on the other), and the client-side
/// verification outcomes plus the server's check of the client Finished.
pub struct HandshakeOutput {
    pub suite: CipherSuite,
    pub client_app_write: RecordKeys,
    pub client_app_read: RecordKeys,
    pub server_app_write: RecordKeys,
    pub server_app_read: RecordKeys,
    /// The client verified the certificate's own signature.
    pub cert_verified: bool,
    /// The client verified the CertificateVerify signature under the cert key.
    pub certificate_verify_ok: bool,
    /// The client verified the server Finished MAC.
    pub server_finished_ok: bool,
    /// The server verified the client Finished MAC.
    pub client_finished_ok: bool,
    /// Both sides derived identical client/server application traffic secrets.
    pub keys_match: bool,
}

/// The RFC 8446 §4.4.3 CertificateVerify signed content: 64 `0x20` octets, the
/// context string, a `0x00` separator, then the transcript hash.
fn certificate_verify_content(context: &[u8], transcript_hash: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(64 + context.len() + 1 + transcript_hash.len());
    v.extend(core::iter::repeat_n(0x20u8, 64));
    v.extend_from_slice(context);
    v.push(0x00);
    v.extend_from_slice(transcript_hash);
    v
}

const SERVER_CV_CONTEXT: &[u8] = b"TLS 1.3, server CertificateVerify";

/// Build the record keys for one traffic secret + suite.
fn record_keys(suite: CipherSuite, secret: &Secret) -> RecordKeys {
    let key = keyschedule::traffic_key(secret, suite.key_len());
    let iv = keyschedule::traffic_iv(secret);
    RecordKeys::new(suite, &key, iv)
}

/// Run a full 1-RTT handshake in-cell for `suite`, authenticated by `server`.
/// Returns the negotiated application keys and the verification outcomes. Fails
/// only on a genuine protocol error (a decode failure or a rejected MAC/signature
/// that should abort a real handshake); a successful return with a `false`
/// verification flag never happens - a failed check returns `Err`.
pub fn run_handshake(
    suite: CipherSuite,
    server: &ServerIdentity,
) -> Result<HandshakeOutput, TlsError> {
    // Fixed ephemerals so the in-cell handshake is deterministic (not the RFC 8448
    // keys - those drive the KAT; these just have to be a valid pair each side).
    let client_eph_priv: [u8; 32] = [0x11; 32];
    let server_eph_priv: [u8; 32] = [0x22; 32];
    let client_eph_pub = kx::public_key(client_eph_priv);
    let server_eph_pub = kx::public_key(server_eph_priv);

    let mut client_ts = Transcript::new();
    let mut server_ts = Transcript::new();

    // --- ClientHello ---
    let ch_body = msg::build_client_hello(
        &[0xAA; 32],
        &[],
        &[
            CipherSuite::Aes128GcmSha256.to_u16(),
            CipherSuite::ChaCha20Poly1305Sha256.to_u16(),
        ],
        &client_eph_pub,
    );
    let ch = msg::handshake_message(HandshakeType::ClientHello, &ch_body);
    client_ts.add(&ch);

    // Server parses ClientHello, picks the suite, reads the client's share.
    let (ct, ch_parsed_body) = msg::parse_handshake(&ch)?;
    if ct != HandshakeType::ClientHello {
        return Err(TlsError::Decode);
    }
    let ch_info = msg::parse_client_hello(ch_parsed_body)?;
    if !ch_info.cipher_suites.contains(&suite.to_u16()) {
        return Err(TlsError::NoCommonSuite);
    }
    let client_share = ch_info.key_share_x25519.ok_or(TlsError::MissingKeyShare)?;
    server_ts.add(&ch);

    // --- ServerHello ---
    let sh_body = msg::build_server_hello(&[0xBB; 32], &[], suite.to_u16(), &server_eph_pub);
    let sh = msg::handshake_message(HandshakeType::ServerHello, &sh_body);
    server_ts.add(&sh);
    // Client parses ServerHello.
    let (ct, sh_parsed_body) = msg::parse_handshake(&sh)?;
    if ct != HandshakeType::ServerHello {
        return Err(TlsError::Decode);
    }
    let (chosen, server_share) = msg::parse_server_hello(sh_parsed_body)?;
    if CipherSuite::from_u16(chosen) != Some(suite) {
        return Err(TlsError::NoCommonSuite);
    }
    client_ts.add(&sh);

    // --- ECDHE + handshake secrets (both sides, must match) ---
    let server_ecdhe = kx::diffie_hellman(server_eph_priv, client_share);
    let client_ecdhe = kx::diffie_hellman(client_eph_priv, server_share);
    if server_ecdhe != client_ecdhe {
        return Err(TlsError::MissingKeyShare);
    }

    let early = keyschedule::early_secret(None);
    let handshake = keyschedule::handshake_secret(&early, &server_ecdhe);

    let th_ch_sh = server_ts.hash(); // == client_ts.hash() at this point
    let s_hs_secret = keyschedule::derive_secret(&handshake, b"s hs traffic", &th_ch_sh);
    let c_hs_secret = keyschedule::derive_secret(&handshake, b"c hs traffic", &th_ch_sh);

    let mut server_hs_write = record_keys(suite, &s_hs_secret);
    let mut client_hs_read = record_keys(suite, &s_hs_secret);
    let mut client_hs_write = record_keys(suite, &c_hs_secret);
    let mut server_hs_read = record_keys(suite, &c_hs_secret);

    // --- Server's authenticated flight (each message its own record) ---
    // EncryptedExtensions (empty).
    let ee = msg::handshake_message(HandshakeType::EncryptedExtensions, &[0x00, 0x00]);
    server_ts.add(&ee);
    let ee_rec = server_hs_write.encrypt(ContentType::Handshake, &ee);

    // Certificate.
    let cert_msg = build_certificate(&server.cert_der);
    server_ts.add(&cert_msg);
    let cert_rec = server_hs_write.encrypt(ContentType::Handshake, &cert_msg);

    // CertificateVerify (Ed25519 over the §4.4.3 content with the CH..Cert hash).
    let cv_content = certificate_verify_content(SERVER_CV_CONTEXT, &server_ts.hash());
    let cv_sig = sign::sign(&server.ed25519_seed, &cv_content);
    let cv_msg = build_certificate_verify(msg::SIG_ED25519, &cv_sig);
    server_ts.add(&cv_msg);
    let cv_rec = server_hs_write.encrypt(ContentType::Handshake, &cv_msg);

    // Finished (server): HMAC over the CH..CertificateVerify hash.
    let s_fin_key = keyschedule::finished_key(&s_hs_secret);
    let s_verify = keyschedule::verify_data(&s_fin_key, &server_ts.hash());
    let s_fin = msg::handshake_message(HandshakeType::Finished, &s_verify);
    server_ts.add(&s_fin);
    let s_fin_rec = server_hs_write.encrypt(ContentType::Handshake, &s_fin);

    // --- Client receives and verifies the flight ---
    let (_, ee_pt) = client_hs_read.decrypt(&ee_rec)?;
    client_ts.add(&ee_pt);

    let (_, cert_pt) = client_hs_read.decrypt(&cert_rec)?;
    let cert_der = parse_certificate(&cert_pt)?;
    let cert = super::x509::parse(cert_der)?;
    let cert_verified = cert.verify_self_signed();
    if !cert_verified {
        return Err(TlsError::BadCertificate);
    }
    // Transcript hash CH..Cert (for verifying CertificateVerify).
    client_ts.add(&cert_pt);
    let th_ch_cert = client_ts.hash();

    let (_, cv_pt) = client_hs_read.decrypt(&cv_rec)?;
    let (_scheme, sig) = parse_certificate_verify(&cv_pt)?;
    let cv_check_content = certificate_verify_content(SERVER_CV_CONTEXT, &th_ch_cert);
    let certificate_verify_ok = sign::verify(&cert.subject_public_key, &cv_check_content, &sig);
    if !certificate_verify_ok {
        return Err(TlsError::BadSignature);
    }
    client_ts.add(&cv_pt);
    // Transcript hash CH..CertificateVerify (for verifying the server Finished).
    let th_ch_cv = client_ts.hash();

    let (_, s_fin_pt) = client_hs_read.decrypt(&s_fin_rec)?;
    let (_, s_verify_recv) = msg::parse_handshake(&s_fin_pt)?;
    let expect = keyschedule::verify_data(&keyschedule::finished_key(&s_hs_secret), &th_ch_cv);
    let server_finished_ok = s_verify_recv == expect;
    if !server_finished_ok {
        return Err(TlsError::BadFinished);
    }
    client_ts.add(&s_fin_pt);

    // --- Application traffic secrets (both sides) over the CH..server-Finished hash ---
    let master = keyschedule::master_secret(&handshake);
    let th_to_sfin_server = server_ts.hash();
    let th_to_sfin_client = client_ts.hash();
    let c_ap_server = keyschedule::derive_secret(&master, b"c ap traffic", &th_to_sfin_server);
    let s_ap_server = keyschedule::derive_secret(&master, b"s ap traffic", &th_to_sfin_server);
    let c_ap_client = keyschedule::derive_secret(&master, b"c ap traffic", &th_to_sfin_client);
    let s_ap_client = keyschedule::derive_secret(&master, b"s ap traffic", &th_to_sfin_client);
    let keys_match = c_ap_server == c_ap_client && s_ap_server == s_ap_client;

    // --- Client Finished (over the CH..server-Finished hash), server verifies ---
    let c_fin_key = keyschedule::finished_key(&c_hs_secret);
    let c_verify = keyschedule::verify_data(&c_fin_key, &th_to_sfin_client);
    let c_fin = msg::handshake_message(HandshakeType::Finished, &c_verify);
    let c_fin_rec = client_hs_write.encrypt(ContentType::Handshake, &c_fin);
    let (_, c_fin_pt) = server_hs_read.decrypt(&c_fin_rec)?;
    let (_, c_verify_recv) = msg::parse_handshake(&c_fin_pt)?;
    let expect_c =
        keyschedule::verify_data(&keyschedule::finished_key(&c_hs_secret), &th_to_sfin_server);
    let client_finished_ok = c_verify_recv == expect_c && c_verify_recv == c_verify;
    if !client_finished_ok {
        return Err(TlsError::BadFinished);
    }

    Ok(HandshakeOutput {
        suite,
        client_app_write: record_keys(suite, &c_ap_client),
        client_app_read: record_keys(suite, &s_ap_client),
        server_app_write: record_keys(suite, &s_ap_server),
        server_app_read: record_keys(suite, &c_ap_server),
        cert_verified,
        certificate_verify_ok,
        server_finished_ok,
        client_finished_ok,
        keys_match,
    })
}

/// Build a Certificate message body (RFC 8446 §4.4.2) carrying one DER cert with
/// an empty request context and empty per-entry extensions.
fn build_certificate(cert_der: &[u8]) -> Vec<u8> {
    let mut entry = Vec::new();
    // cert_data<1..2^24-1>
    let l = cert_der.len();
    entry.extend_from_slice(&[(l >> 16) as u8, (l >> 8) as u8, l as u8]);
    entry.extend_from_slice(cert_der);
    entry.extend_from_slice(&[0x00, 0x00]); // extensions<0..2^16-1> = empty

    let mut body = Vec::new();
    body.push(0x00); // certificate_request_context<0..2^8-1> = empty
    let ll = entry.len();
    body.extend_from_slice(&[(ll >> 16) as u8, (ll >> 8) as u8, ll as u8]);
    body.extend_from_slice(&entry);
    msg::handshake_message(HandshakeType::Certificate, &body)
}

/// Parse a Certificate message, returning the first entry's DER bytes.
fn parse_certificate(msg_bytes: &[u8]) -> Result<&[u8], TlsError> {
    let (kind, body) = msg::parse_handshake(msg_bytes)?;
    if kind != HandshakeType::Certificate || body.len() < 4 {
        return Err(TlsError::Decode);
    }
    let ctx_len = body[0] as usize;
    let mut p = 1 + ctx_len;
    if p + 3 > body.len() {
        return Err(TlsError::Decode);
    }
    let _list_len =
        ((body[p] as usize) << 16) | ((body[p + 1] as usize) << 8) | body[p + 2] as usize;
    p += 3;
    if p + 3 > body.len() {
        return Err(TlsError::Decode);
    }
    let cert_len =
        ((body[p] as usize) << 16) | ((body[p + 1] as usize) << 8) | body[p + 2] as usize;
    p += 3;
    if p + cert_len > body.len() {
        return Err(TlsError::Decode);
    }
    Ok(&body[p..p + cert_len])
}

/// Build a CertificateVerify message body (RFC 8446 §4.4.3):
/// `SignatureScheme(2) || signature<0..2^16-1>`.
fn build_certificate_verify(scheme: u16, signature: &[u8; 64]) -> Vec<u8> {
    let mut body = Vec::with_capacity(4 + 64);
    body.extend_from_slice(&scheme.to_be_bytes());
    body.extend_from_slice(&(signature.len() as u16).to_be_bytes());
    body.extend_from_slice(signature);
    msg::handshake_message(HandshakeType::CertificateVerify, &body)
}

/// Parse a CertificateVerify message, returning `(scheme, signature)`.
fn parse_certificate_verify(msg_bytes: &[u8]) -> Result<(u16, [u8; 64]), TlsError> {
    let (kind, body) = msg::parse_handshake(msg_bytes)?;
    if kind != HandshakeType::CertificateVerify || body.len() < 4 {
        return Err(TlsError::Decode);
    }
    let scheme = u16::from_be_bytes([body[0], body[1]]);
    let sig_len = u16::from_be_bytes([body[2], body[3]]) as usize;
    if sig_len != 64 || 4 + sig_len != body.len() {
        return Err(TlsError::Decode);
    }
    let mut sig = [0u8; 64];
    sig.copy_from_slice(&body[4..]);
    Ok((scheme, sig))
}
