//! `nettls-demo` - the rheo-net Phase N3b proof cell (docs/NETSTACK.md §15). It
//! proves the from-scratch TLS 1.3 stack three ways, exiting `0x42` only if all
//! pass:
//!
//! 1. **RFC 8448 §3 known-answer test** - the authoritative TLS 1.3 oracle. Fed
//!    the RFC's own ClientHello/ServerHello bytes + X25519 keys, our key schedule
//!    derives the RFC's early/handshake/master secrets, client/server handshake &
//!    application traffic secrets, the write keys + IVs, and both Finished MACs
//!    **byte-for-byte**. This proves spec-correctness with no live peer.
//! 2. **In-cell full 1-RTT handshake** (both cipher suites) - a client and server
//!    endpoint complete a handshake, derive **matching** traffic keys, exchange an
//!    encrypted application record **both ways** (plaintext round-trips), and a
//!    **tampered record fails** the AEAD.
//! 3. **Minimal X.509** - parse a known Ed25519 test certificate, verify its
//!    signature (pass), and reject a tampered tbsCertificate (fail).
//!
//! Every RFC 8448 value below was fetched from the authoritative RFC text and is
//! hardcoded as the expected answer (never computed by the code under test).

#![no_std]
#![no_main]

extern crate alloc;

use alloc::vec::Vec;

use librheo::println;
use rheo_net::crypto::{hash::sha256, kx};
use rheo_net::tls::keyschedule as ks;
use rheo_net::tls::{CipherSuite, ContentType, ServerIdentity, run_handshake, x509};

const OK_CODE: i32 = 0x42;

/// Compile-time hex parser (as in `netcrypto-demo`) so RFC 8448 values paste as
/// their exact published hex.
const fn hexval(c: u8) -> u8 {
    match c {
        b'0'..=b'9' => c - b'0',
        b'a'..=b'f' => c - b'a' + 10,
        b'A'..=b'F' => c - b'A' + 10,
        _ => 0,
    }
}
const fn h<const N: usize>(s: &str) -> [u8; N] {
    let b = s.as_bytes();
    let mut out = [0u8; N];
    let mut i = 0;
    while i < N {
        out[i] = (hexval(b[i * 2]) << 4) | hexval(b[i * 2 + 1]);
        i += 1;
    }
    out
}

#[unsafe(no_mangle)]
extern "C" fn main() -> i32 {
    if let Err(code) = run() {
        println!("nettls-demo: FAIL at check {code}");
        return code;
    }
    println!("nettls-demo: TLS 1.3 - RFC 8448 KAT + in-cell handshake + X.509 all OK");
    OK_CODE
}

fn run() -> Result<(), i32> {
    rfc8448_kat()?;
    handshake_suite(CipherSuite::Aes128GcmSha256, 200)?;
    handshake_suite(CipherSuite::ChaCha20Poly1305Sha256, 300)?;
    x509_verify_reject()?;
    Ok(())
}

/// RFC 8448 §3 "Simple 1-RTT Handshake" - the key schedule byte-for-byte.
fn rfc8448_kat() -> Result<(), i32> {
    // Ephemeral X25519 keys (RFC 8448 §3).
    let client_priv: [u8; 32] =
        h("49af42ba7f7994852d713ef2784bcbcaa7911de26adc5642cb634540e7ea5005");
    let client_pub: [u8; 32] =
        h("99381de560e4bd43d23d8e435a7dbafeb3c06e51c13cae4d5413691e529aaf2c");
    let server_priv: [u8; 32] =
        h("b1580eeadf6dd589b8ef4f2d5652578cc810e9980191ec8d058308cea216a21e");
    let server_pub: [u8; 32] =
        h("c9828876112095fe66762bdbf7c672e156d6cc253b833df1dd69b1b04e751f0f");
    let ecdhe_expected: [u8; 32] =
        h("8bd4054fb55b9d63fdfbacf9f04b9f0d35e6d63f537563efd46272900f89492d");

    // The ECDHE shared secret, both directions.
    if kx::x25519(client_priv, server_pub) != ecdhe_expected {
        return Err(10);
    }
    if kx::x25519(server_priv, client_pub) != ecdhe_expected {
        return Err(11);
    }
    let ecdhe = ecdhe_expected;

    // The ClientHello + ServerHello handshake-message bytes (RFC 8448 §3).
    let ch: [u8; 196] = h(concat!(
        "010000c00303cb34ecb1e78163ba1c38c6dacb196a6dffa21a8d9912ec18a2ef",
        "6283024dece7000006130113031302010000910000000b000900000673657276",
        "6572ff01000100000a00140012001d0017001800190100010101020103010400",
        "230000003300260024001d002099381de560e4bd43d23d8e435a7dbafeb3c06e",
        "51c13cae4d5413691e529aaf2c002b0003020304000d0020001e040305030603",
        "020308040805080604010501060102010402050206020202002d00020101001c",
        "00024001"
    ));
    let sh: [u8; 90] = h(concat!(
        "020000560303a6af06a4121860dc5e6e60249cd34c95930c8ac5cb1434dac155",
        "772ed3e2692800130100002e00330024001d0020c9828876112095fe66762bdb",
        "f7c672e156d6cc253b833df1dd69b1b04e751f0f002b00020304"
    ));
    // The server's authenticated flight (EncryptedExtensions || Certificate ||
    // CertificateVerify || Finished), 657 octets, RFC 8448 §3.
    let sflight: [u8; 657] = h(concat!(
        "080000240022000a00140012001d00170018001901000101010201030104001c",
        "00024001000000000b0001b9000001b50001b0308201ac30820115a003020102",
        "020102300d06092a864886f70d01010b0500300e310c300a0603550403130372",
        "7361301e170d3136303733303031323335395a170d3236303733303031323335",
        "395a300e310c300a0603550403130372736130819f300d06092a864886f70d01",
        "0101050003818d0030818902818100b4bb498f8279303d980836399b36c6988c",
        "0c68de55e1bdb826d3901a2461eafd2de49a91d015abbc9a95137ace6c1af19e",
        "aa6af98c7ced43120998e187a80ee0ccb0524b1b018c3e0b63264d449a6d38e2",
        "2a5fda430846748030530ef0461c8ca9d9efbfae8ea6d1d03e2bd193eff0ab9a",
        "8002c47428a6d35a8d88d79f7f1e3f0203010001a31a301830090603551d1304",
        "023000300b0603551d0f0404030205a0300d06092a864886f70d01010b050003",
        "81810085aad2a0e5b9276b908c65f73a7267170618a54c5f8a7b337d2df7a594",
        "365417f2eae8f8a58c8f8172f9319cf36b7fd6c55b80f21a03015156726096fd",
        "335e5e67f2dbf102702e608ccae6bec1fc63a42a99be5c3eb7107c3c54e9b9eb",
        "2bd5203b1c3b84e0a8b2f759409ba3eac9d91d402dcc0cc8f8961229ac9187b4",
        "2b4de100000f000084080400805a747c5d88fa9bd2e55ab085a61015b7211f82",
        "4cd484145ab3ff52f1fda8477b0b7abc90db78e2d33a5c141a078653fa6bef78",
        "0c5ea248eeaaa785c4f394cab6d30bbe8d4859ee511f602957b15411ac027671",
        "459e46445c9ea58c181e818e95b8c3fb0bf3278409d3be152a3da5043e063dda",
        "65cdf5aea20d53dfacd42f74f3140000209b9b141d906337fbd2cbdce71df4de",
        "da4ab42c309572cb7fffee5454b78f0718"
    ));

    // Early secret.
    let early_expected: [u8; 32] =
        h("33ad0a1c607ec03b09e6cd9893680ce210adf300aa1f2660e1b22e10f170f92a");
    let early = ks::early_secret(None);
    if early != early_expected {
        return Err(12);
    }
    // Derive-Secret(Early, "derived", "") - the handshake-stage derived secret.
    let derived_hs_expected: [u8; 32] =
        h("6f2615a108c702c5678f54fc9dbab69716c076189c48250cebeac3576c3611ba");
    if ks::derive_secret(&early, b"derived", &ks::empty_hash()) != derived_hs_expected {
        return Err(13);
    }
    // Handshake secret.
    let hs_expected: [u8; 32] =
        h("1dc826e93606aa6fdc0aadc12f741b01046aa6b99f691ed221a9f0ca043fbeac");
    let handshake = ks::handshake_secret(&early, &ecdhe);
    if handshake != hs_expected {
        return Err(14);
    }

    // Transcript hash CH || SH, and the client/server handshake traffic secrets.
    let th_ch_sh_expected: [u8; 32] =
        h("860c06edc07858ee8e78f0e7428c58edd6b43f2ca3e6e95f02ed063cf0e1cad8");
    let mut ch_sh = Vec::new();
    ch_sh.extend_from_slice(&ch);
    ch_sh.extend_from_slice(&sh);
    let th_ch_sh = sha256(&ch_sh);
    if th_ch_sh != th_ch_sh_expected {
        return Err(15);
    }
    let chs_expected: [u8; 32] =
        h("b3eddb126e067f35a780b3abf45e2d8f3b1a950738f52e9600746a0e27a55a21");
    let shs_expected: [u8; 32] =
        h("b67b7d690cc16c4e75e54213cb2d37b4e9c912bcded9105d42befd59d391ad38");
    let chs = ks::derive_secret(&handshake, b"c hs traffic", &th_ch_sh);
    let shs = ks::derive_secret(&handshake, b"s hs traffic", &th_ch_sh);
    if chs != chs_expected {
        return Err(16);
    }
    if shs != shs_expected {
        return Err(17);
    }

    // Derive-Secret(Handshake, "derived", "") + Master secret.
    let derived_master_expected: [u8; 32] =
        h("43de77e0c77713859a944db9db2590b53190a65b3ee2e4f12dd7a0bb7ce254b4");
    if ks::derive_secret(&handshake, b"derived", &ks::empty_hash()) != derived_master_expected {
        return Err(18);
    }
    let master_expected: [u8; 32] =
        h("18df06843d13a08bf2a449844c5f8a478001bc4d4c627984d5a41da8d0402919");
    let master = ks::master_secret(&handshake);
    if master != master_expected {
        return Err(19);
    }

    // Handshake write keys + IVs (server + client), AES-128-GCM (16-byte key).
    if ks::traffic_key(&shs, 16) != h::<16>("3fce516009c21727d0f2e4e86ee403bc") {
        return Err(20);
    }
    if ks::traffic_iv(&shs) != h::<12>("5d313eb2671276ee13000b30") {
        return Err(21);
    }
    if ks::traffic_key(&chs, 16) != h::<16>("dbfaa693d1762c5b666af5d950258d01") {
        return Err(22);
    }
    if ks::traffic_iv(&chs) != h::<12>("5bd3c71b836e0b76bb73265f") {
        return Err(23);
    }

    // Transcript hash CH..server Finished, and the application traffic secrets.
    let th_sf_expected: [u8; 32] =
        h("9608102a0f1ccc6db6250b7b7e417b1a000eaada3daae4777a7686c9ff83df13");
    let mut full = Vec::new();
    full.extend_from_slice(&ch);
    full.extend_from_slice(&sh);
    full.extend_from_slice(&sflight);
    let th_sf = sha256(&full);
    if th_sf != th_sf_expected {
        return Err(24);
    }
    let cap_expected: [u8; 32] =
        h("9e40646ce79a7f9dc05af8889bce6552875afa0b06df0087f792ebb7c17504a5");
    let sap_expected: [u8; 32] =
        h("a11af9f05531f856ad47116b45a950328204b4f44bfb6b3a4b4f1f3fcb631643");
    let exp_master_expected: [u8; 32] =
        h("fe22f881176eda18eb8f44529e6792c50c9a3f89452f68d8ae311b4309d3cf50");
    let cap = ks::derive_secret(&master, b"c ap traffic", &th_sf);
    let sap = ks::derive_secret(&master, b"s ap traffic", &th_sf);
    let exp = ks::derive_secret(&master, b"exp master", &th_sf);
    if cap != cap_expected {
        return Err(25);
    }
    if sap != sap_expected {
        return Err(26);
    }
    if exp != exp_master_expected {
        return Err(27);
    }

    // Application write keys + IVs.
    if ks::traffic_key(&sap, 16) != h::<16>("9f02283b6c9c07efc26bb9f2ac92e356") {
        return Err(28);
    }
    if ks::traffic_iv(&sap) != h::<12>("cf782b88dd83549aadf1e984") {
        return Err(29);
    }
    if ks::traffic_key(&cap, 16) != h::<16>("17422dda596ed5d9acd890e3c63f5051") {
        return Err(30);
    }
    if ks::traffic_iv(&cap) != h::<12>("5b78923dee08579033e523d9") {
        return Err(31);
    }

    // Finished keys + verify_data (server: hash CH..CertificateVerify; client:
    // hash CH..server Finished). The server Finished is the last 36 octets of the
    // 657-octet flight, so CH..CertificateVerify = CH || SH || sflight[..621].
    let s_fin_key_expected: [u8; 32] =
        h("008d3b66f816ea559f96b537e885c31fc068bf492c652f01f288a1d8cdc19fc8");
    let s_fin_key = ks::finished_key(&shs);
    if s_fin_key != s_fin_key_expected {
        return Err(32);
    }
    let mut ch_sh_cert = Vec::new();
    ch_sh_cert.extend_from_slice(&ch);
    ch_sh_cert.extend_from_slice(&sh);
    ch_sh_cert.extend_from_slice(&sflight[..621]);
    let th_ch_cv = sha256(&ch_sh_cert);
    let s_verify_expected: [u8; 32] =
        h("9b9b141d906337fbd2cbdce71df4deda4ab42c309572cb7fffee5454b78f0718");
    if ks::verify_data(&s_fin_key, &th_ch_cv) != s_verify_expected {
        return Err(33);
    }
    let c_fin_key_expected: [u8; 32] =
        h("b80ad01015fb2f0bd65ff7d4da5d6bf83f84821d1f87fdc7d3c75b5a7b42d9c4");
    let c_fin_key = ks::finished_key(&chs);
    if c_fin_key != c_fin_key_expected {
        return Err(34);
    }
    let c_verify_expected: [u8; 32] =
        h("a8ec436d677634ae525ac1fcebe11a039ec17694fac6e98527b642f2edd5ce61");
    if ks::verify_data(&c_fin_key, &th_sf) != c_verify_expected {
        return Err(35);
    }

    println!("nettls-demo: RFC 8448 key schedule (secrets/keys/IVs/Finished) byte-for-byte OK");
    Ok(())
}

/// A known Ed25519 self-signed test certificate (DER), generated once by openssl
/// (`req -x509 -newkey ed25519`) - a real cert, hardcoded, not committed as a
/// fixture. Its subject public key is `32142a51...8f35`.
const TEST_CERT_DER: [u8; 328] = h(concat!(
    "308201443081f7a00302010202141dd6277cb3a4f05ff4e8e30b2b5b63d9a143",
    "7cad300506032b657030183116301406035504030c0d7268656f2d6e65742074",
    "657374301e170d3236303732363132323831375a170d33363037323331323238",
    "31375a30183116301406035504030c0d7268656f2d6e65742074657374302a30",
    "0506032b657003210032142a51fffcac956ca6a32441e8d9f2927e1324fd7855",
    "6450953999e52b8f35a3533051301d0603551d0e0416041415eb797fa50c6787",
    "52ef8d2a0b79ad9b6218f3b0301f0603551d2304183016801415eb797fa50c67",
    "8752ef8d2a0b79ad9b6218f3b0300f0603551d130101ff040530030101ff3005",
    "06032b6570034100285db26f12e04ff7b805ff1a2445e36d2dfabf35943b7847",
    "574b702bf1ee9e12d2f4276dc6bcee5bcf0a7de0782099b68cfd19d5733b6f5e",
    "d5c7113ceaa22708"
));
/// The Ed25519 secret seed whose public key is the cert's subject key (openssl
/// `pkey`). The in-cell server signs its CertificateVerify with this.
const TEST_CERT_SEED: [u8; 32] =
    h("f53d8117f9e99f188570860eebf9bc5349a378b95dd8ab18659d5d738e12f3d9");

/// An in-cell full 1-RTT handshake for `suite`: matching keys, an app-data round
/// trip both ways, and a tamper rejection.
fn handshake_suite(suite: CipherSuite, base: i32) -> Result<(), i32> {
    let server = ServerIdentity {
        cert_der: TEST_CERT_DER.to_vec(),
        ed25519_seed: TEST_CERT_SEED,
    };
    let mut out = run_handshake(suite, &server).map_err(|_| base)?;
    if !(out.cert_verified
        && out.certificate_verify_ok
        && out.server_finished_ok
        && out.client_finished_ok
        && out.keys_match)
    {
        return Err(base + 1);
    }

    // Application record, client -> server.
    let ping = b"ping over tls 1.3";
    let rec = out
        .client_app_write
        .encrypt(ContentType::ApplicationData, ping);
    match out.server_app_read.decrypt(&rec) {
        Ok((ContentType::ApplicationData, pt)) if pt.as_slice() == ping.as_slice() => {}
        _ => return Err(base + 2),
    }
    // Application record, server -> client.
    let pong = b"pong over tls 1.3";
    let rec2 = out
        .server_app_write
        .encrypt(ContentType::ApplicationData, pong);
    match out.client_app_read.decrypt(&rec2) {
        Ok((ContentType::ApplicationData, pt)) if pt.as_slice() == pong.as_slice() => {}
        _ => return Err(base + 3),
    }
    // Tamper: flip a ciphertext byte; the AEAD must reject it (seq now aligned at
    // 1 on both server_app_write and client_app_read after the round trip).
    let mut rec3 = out
        .server_app_write
        .encrypt(ContentType::ApplicationData, pong);
    rec3[7] ^= 0x01;
    if out.client_app_read.decrypt(&rec3).is_ok() {
        return Err(base + 4);
    }

    println!("nettls-demo: in-cell 1-RTT handshake + app round trip + tamper-reject OK");
    Ok(())
}

/// Minimal X.509: parse the test cert, verify its Ed25519 self-signature (pass),
/// and reject a tampered tbsCertificate (fail).
fn x509_verify_reject() -> Result<(), i32> {
    let cert = x509::parse(&TEST_CERT_DER).map_err(|_| 400)?;
    if cert.subject_public_key
        != h::<32>("32142a51fffcac956ca6a32441e8d9f2927e1324fd78556450953999e52b8f35")
    {
        return Err(401);
    }
    if !cert.verify_self_signed() {
        return Err(402);
    }
    // Tamper one byte inside the tbsCertificate (offset ~30, in the serial/validity
    // region) and re-parse: the signature must now fail to verify.
    let mut bad = TEST_CERT_DER;
    bad[40] ^= 0x01;
    let bad_cert = x509::parse(&bad).map_err(|_| 403)?;
    if bad_cert.verify_self_signed() {
        return Err(404);
    }
    println!("nettls-demo: minimal X.509 parse + Ed25519 verify (pass) + tamper (reject) OK");
    Ok(())
}
