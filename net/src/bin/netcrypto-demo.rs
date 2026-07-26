//! `netcrypto-demo` - the rheo-net Phase N3a proof cell (docs/NETSTACK.md §3).
//! It runs **every crypto primitive against its published RFC/NIST test vector**,
//! plus decrypt round trips, tamper rejections, and the two-randomness-class /
//! nonce-safe API behaviour. Pure compute - no netdev, no queue traffic. It
//! exits `0x42` only if every check passes; the `netcrypto` test kernel asserts
//! that code on all three ISAs.
//!
//! Every expected value below is a real published vector, independently verified
//! (ChaCha20/Poly1305/AEAD from RFC 8439, SHA from NIST/RFC 6234, HKDF from RFC
//! 5869, X25519 from RFC 7748, Ed25519 from RFC 8032, AES-GCM from the
//! McGrew-Viega GCM-spec / NIST test cases). No vector is fabricated.

#![no_std]
#![no_main]

extern crate alloc;

use librheo::println;
use rheo_net::crypto::{
    Aead, SealingKey,
    aead::{self, NonceError},
    aesgcm, chacha, chachapoly, hash, kdf, kx, poly1305, rand, sign,
};

/// Failure code (0 = success); the first failing check wins.
const OK_CODE: i32 = 0x42;

/// Compile-time hex parser so the vectors can be pasted as the exact published
/// hex strings (less error-prone than hand-split byte arrays).
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
        println!("netcrypto-demo: FAIL at check {code}");
        return code;
    }
    println!("netcrypto-demo: all primitives vector-proven OK");
    OK_CODE
}

fn run() -> Result<(), i32> {
    chacha20_block()?;
    poly1305_vector()?;
    chacha20poly1305_aead()?;
    sha2_vectors()?;
    hkdf_vector()?;
    x25519_vectors()?;
    ed25519_vectors()?;
    aes_gcm_vectors()?;
    two_randomness_classes()?;
    nonce_safety()?;
    Ok(())
}

/// RFC 8439 §2.3.2 - the ChaCha20 block function.
fn chacha20_block() -> Result<(), i32> {
    let key: [u8; 32] = h("000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f");
    let nonce: [u8; 12] = h("000000090000004a00000000");
    let expected: [u8; 64] = h(concat!(
        "10f1e7e4d13b5915500fdd1fa32071c4c7d1f4c733c068030422aa9ac3d46c4e",
        "d2826446079faa0914c2d705d98b02a2b5129cd1de164eb9cbd083e8a2503c4e"
    ));
    let mut out = [0u8; 64];
    chacha::block(&key, 1, &nonce, &mut out);
    if out != expected {
        return Err(10);
    }
    println!("netcrypto-demo: ChaCha20 block (RFC 8439 2.3.2) OK");
    Ok(())
}

/// RFC 8439 §2.5.2 - the Poly1305 MAC.
fn poly1305_vector() -> Result<(), i32> {
    let key: [u8; 32] = h("85d6be7857556d337f4452fe42d506a80103808afb0db2fd4abff6af4149f51b");
    let msg = b"Cryptographic Forum Research Group";
    let expected: [u8; 16] = h("a8061dc1305136c6c22b8baf0c0127a9");
    if poly1305::tag(&key, msg) != expected {
        return Err(20);
    }
    println!("netcrypto-demo: Poly1305 (RFC 8439 2.5.2) OK");
    Ok(())
}

/// RFC 8439 §2.8.2 - the ChaCha20-Poly1305 AEAD (from scratch), + a decrypt round
/// trip + a tampered-tag rejection.
fn chacha20poly1305_aead() -> Result<(), i32> {
    let key: [u8; 32] = h("808182838485868788898a8b8c8d8e8f909192939495969798999a9b9c9d9e9f");
    let nonce: [u8; 12] = h("070000004041424344454647");
    let aad: [u8; 12] = h("50515253c0c1c2c3c4c5c6c7");
    let pt = b"Ladies and Gentlemen of the class of '99: If I could offer you only one tip for the future, sunscreen would be it.";
    let ct_expected: [u8; 114] = h(concat!(
        "d31a8d34648e60db7b86afbc53ef7ec2a4aded51296e08fea9e2b5a736ee62d6",
        "3dbea45e8ca9671282fafb69da92728b1a71de0a9e060b2905d6a5b67ecd3b36",
        "92ddbd7f2d778b8c9803aee328091b58fab324e4fad675945585808b4831d7bc",
        "3ff4def08e4b7a9de576d26586cec64b6116"
    ));
    let tag_expected: [u8; 16] = h("1ae10b594f09e26a7e902ecbd0600691");

    let aead = chachapoly::ChaCha20Poly1305::new(key);
    let (ct, tag) = aead.seal(&nonce, &aad, pt);
    if ct.as_slice() != ct_expected.as_slice() {
        return Err(30);
    }
    if tag != tag_expected {
        return Err(31);
    }
    // Decrypt round trip.
    match aead.open(&nonce, &aad, &ct, &tag) {
        Some(dec) if dec.as_slice() == pt.as_slice() => {}
        _ => return Err(32),
    }
    // Tampered tag must be rejected.
    let mut bad_tag = tag;
    bad_tag[0] ^= 0x01;
    if aead.open(&nonce, &aad, &ct, &bad_tag).is_some() {
        return Err(33);
    }
    // Tampered ciphertext must be rejected.
    let mut bad_ct = ct.clone();
    bad_ct[0] ^= 0x01;
    if aead.open(&nonce, &aad, &bad_ct, &tag).is_some() {
        return Err(34);
    }
    println!("netcrypto-demo: ChaCha20-Poly1305 AEAD (RFC 8439 2.8.2) + roundtrip + tamper OK");
    Ok(())
}

/// NIST / RFC 6234 - SHA-256 and SHA-384 of "abc".
fn sha2_vectors() -> Result<(), i32> {
    let sha256_abc: [u8; 32] =
        h("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
    let sha384_abc: [u8; 48] = h(concat!(
        "cb00753f45a35e8bb5a03d699ac65007272c32ab0eded163",
        "1a8b605a43ff5bed8086072ba1e7cc2358baeca134c825a7"
    ));
    if hash::sha256(b"abc") != sha256_abc {
        return Err(40);
    }
    if hash::sha384(b"abc") != sha384_abc {
        return Err(41);
    }
    println!("netcrypto-demo: SHA-256 + SHA-384 (\"abc\", NIST/RFC 6234) OK");
    Ok(())
}

/// RFC 5869 Test Case 1 - HKDF-Extract + HKDF-Expand (HMAC-SHA256).
fn hkdf_vector() -> Result<(), i32> {
    let ikm = [0x0bu8; 22];
    let salt: [u8; 13] = h("000102030405060708090a0b0c");
    let info: [u8; 10] = h("f0f1f2f3f4f5f6f7f8f9");
    let prk_expected: [u8; 32] =
        h("077709362c2e32df0ddc3f0dc47bba6390b6c73bb50f9c3122ec844ad7c2b3e5");
    let okm_expected: [u8; 42] = h(concat!(
        "3cb25f25faacd57a90434f64d0362f2a2d2d0a90cf1a5a4c5db0",
        "2d56ecc4c5bf34007208d5b887185865"
    ));
    // IKM imported as an explicit external secret (the key side; never the fast RNG).
    let prk = kdf::hkdf_extract(&salt, &kdf::Ikm::import(&ikm));
    if prk.as_bytes() != &prk_expected {
        return Err(50);
    }
    let mut okm = [0u8; 42];
    if kdf::hkdf_expand(&prk, &info, &mut okm).is_err() {
        return Err(51);
    }
    if okm != okm_expected {
        return Err(52);
    }
    println!("netcrypto-demo: HKDF-SHA256 (RFC 5869 TC1) OK");
    Ok(())
}

/// RFC 7748 §5.2 (scalar mult) + §6.1 (a full DH), for X25519.
fn x25519_vectors() -> Result<(), i32> {
    let scalar: [u8; 32] = h("a546e36bf0527c9d3b16154b82465edd62144c0ac1fc5a18506a2244ba449ac4");
    let u: [u8; 32] = h("e6db6867583030db3594c1a424b15f7c726624ec26b3353b10a903a6d0ab1c4c");
    let out_expected: [u8; 32] =
        h("c3da55379de9c6908e94ea4df28d084f32eccf03491c71f754b4075577a28552");
    if kx::x25519(scalar, u) != out_expected {
        return Err(60);
    }
    // §6.1 Diffie-Hellman: derive both public keys from base, then the shared secret.
    let a_priv: [u8; 32] = h("77076d0a7318a57d3c16c17251b26645df4c2f87ebc0992ab177fba51db92c2a");
    let b_priv: [u8; 32] = h("5dab087e624a8a4b79e17f8b83800ee66f3bb1292618b6fd1c2f8b27ff88e0eb");
    let a_pub_expected: [u8; 32] =
        h("8520f0098930a754748b7ddcb43ef75a0dbf3a0d26381af4eba4a98eaa9b4e6a");
    let shared_expected: [u8; 32] =
        h("4a5d9d5ba4ce2de1728e3bf480350f25e07e21c947d19e3376f09b3c1e161742");
    let a_pub = kx::public_key(a_priv);
    let b_pub = kx::public_key(b_priv);
    if a_pub != a_pub_expected {
        return Err(61);
    }
    let s1 = kx::diffie_hellman(a_priv, b_pub);
    let s2 = kx::diffie_hellman(b_priv, a_pub);
    if s1 != shared_expected || s2 != shared_expected {
        return Err(62);
    }
    println!("netcrypto-demo: X25519 scalarmult + DH (RFC 7748 5.2/6.1) OK");
    Ok(())
}

/// RFC 8032 §7.1 - Ed25519 sign + verify (TEST 1 empty msg, TEST 3 non-empty),
/// + tampered-signature and tampered-message rejections.
fn ed25519_vectors() -> Result<(), i32> {
    // TEST 1: empty message.
    let seed1: [u8; 32] = h("9d61b19deffd5a60ba844af492ec2cc44449c5697b326919703bac031cae7f60");
    let pub1: [u8; 32] = h("d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a");
    let sig1: [u8; 64] = h(concat!(
        "e5564300c360ac729086e2cc806e828a84877f1eb8e5d974d873e06522490155",
        "5fb8821590a33bacc61e39701cf9b46bd25bf5f0595bbe24655141438e7a100b"
    ));
    if sign::public_key(&seed1) != pub1 {
        return Err(70);
    }
    if sign::sign(&seed1, b"") != sig1 {
        return Err(71);
    }
    if !sign::verify(&pub1, b"", &sig1) {
        return Err(72);
    }
    // TEST 3: 2-byte message 0xaf82.
    let seed3: [u8; 32] = h("c5aa8df43f9f837bedb7442f31dcb7b166d38535076f094b85ce3a2e0b4458f7");
    let pub3: [u8; 32] = h("fc51cd8e6218a1a38da47ed00230f0580816ed13ba3303ac5deb911548908025");
    let msg3: [u8; 2] = h("af82");
    let sig3: [u8; 64] = h(concat!(
        "6291d657deec24024827e69c3abe01a30ce548a284743a445e3680d7db5ac3ac",
        "18ff9b538d16f290ae67f760984dc6594a7c15e9716ed28dc027beceea1ec40a"
    ));
    if sign::sign(&seed3, &msg3) != sig3 {
        return Err(73);
    }
    if !sign::verify(&pub3, &msg3, &sig3) {
        return Err(74);
    }
    // Tampered signature must be rejected.
    let mut bad_sig = sig3;
    bad_sig[0] ^= 0x01;
    if sign::verify(&pub3, &msg3, &bad_sig) {
        return Err(75);
    }
    // Tampered message must be rejected.
    if sign::verify(&pub3, b"af83", &sig3) {
        return Err(76);
    }
    println!("netcrypto-demo: Ed25519 sign/verify (RFC 8032 7.1) + tamper OK");
    Ok(())
}

/// GCM-spec (McGrew-Viega) / NIST test cases - AES-128-GCM + AES-256-GCM, with a
/// decrypt round trip + a tampered-tag rejection.
fn aes_gcm_vectors() -> Result<(), i32> {
    let iv: [u8; 12] = h("cafebabefacedbaddecaf888");
    let aad: [u8; 20] = h("feedfacedeadbeeffeedfacedeadbeefabaddad2");
    let pt: [u8; 60] = h(concat!(
        "d9313225f88406e5a55909c5aff5269a86a7a9531534f7da2e4c303d8a318a72",
        "1c3c0c95956809532fcf0e2449a6b525b16aedf5aa0de657ba637b39"
    ));
    // AES-128-GCM (Test Case 4).
    let key128: [u8; 16] = h("feffe9928665731c6d6a8f9467308308");
    let ct128: [u8; 60] = h(concat!(
        "42831ec2217774244b7221b784d0d49ce3aa212f2c02a4e035c17e2329aca12e",
        "21d514b25466931c7d8f6a5aac84aa051ba30b396a0aac973d58e091"
    ));
    let tag128: [u8; 16] = h("5bc94fbc3221a5db94fae95ae7121a47");
    let g128 = aesgcm::Aes128Gcm::new(key128);
    let (ct, tag) = g128.seal(&iv, &aad, &pt);
    if ct.as_slice() != ct128.as_slice() || tag != tag128 {
        return Err(80);
    }
    match g128.open(&iv, &aad, &ct, &tag) {
        Some(d) if d.as_slice() == pt.as_slice() => {}
        _ => return Err(81),
    }
    let mut bad = tag;
    bad[15] ^= 0x80;
    if g128.open(&iv, &aad, &ct, &bad).is_some() {
        return Err(82);
    }
    // AES-256-GCM (Test Case 16).
    let key256: [u8; 32] = h("feffe9928665731c6d6a8f9467308308feffe9928665731c6d6a8f9467308308");
    let ct256: [u8; 60] = h(concat!(
        "522dc1f099567d07f47f37a32a84427d643a8cdcbfe5c0c97598a2bd2555d1aa",
        "8cb08e48590dbb3da7b08b1056828838c5f61e6393ba7a0abcc9f662"
    ));
    let tag256: [u8; 16] = h("76fc6ece0f4e1768cddf8853bb2d551b");
    let g256 = aesgcm::Aes256Gcm::new(key256);
    let (ct2, tag2) = g256.seal(&iv, &aad, &pt);
    if ct2.as_slice() != ct256.as_slice() || tag2 != tag256 {
        return Err(83);
    }
    if g256.open(&iv, &aad, &ct2, &tag2).map(|d| d == pt.to_vec()) != Some(true) {
        return Err(84);
    }
    println!("netcrypto-demo: AES-128-GCM + AES-256-GCM (NIST/GCM-spec) + tamper OK");
    Ok(())
}

/// The two-randomness-class discipline: a key schedule is deterministic in its
/// (secret) IKM; public randomness is a distinct, non-deterministic side-stream;
/// the two never share a value or a type.
fn two_randomness_classes() -> Result<(), i32> {
    // A key schedule keyed from a FIXED imported IKM derives deterministically.
    let ikm = [0x42u8; 32];
    let derive = || {
        let prk = kdf::hkdf_extract(b"rheo-net salt", &kdf::Ikm::import(&ikm));
        let mut key = [0u8; 32];
        kdf::hkdf_expand_label(&prk, b"traffic key", b"transcript-hash", &mut key).unwrap();
        key
    };
    let k1 = derive();
    let k2 = derive();
    if k1 != k2 {
        return Err(90); // key schedule must be deterministic in the IKM
    }
    // A different IKM yields a different key.
    let other_prk = kdf::hkdf_extract(b"rheo-net salt", &kdf::Ikm::import(&[0x43u8; 32]));
    let mut k3 = [0u8; 32];
    kdf::hkdf_expand_label(&other_prk, b"traffic key", b"transcript-hash", &mut k3).unwrap();
    if k3 == k1 {
        return Err(91);
    }
    // Public randomness is a separate, non-deterministic stream (for non-secret
    // values), and never equals the key-schedule output.
    let mut pr = rand::public_random();
    let mut r1 = [0u8; 32];
    let mut r2 = [0u8; 32];
    pr.fill(&mut r1);
    pr.fill(&mut r2);
    if r1 == r2 {
        return Err(92); // two public draws must differ
    }
    if r1 == k1 {
        return Err(93);
    }
    // The type barrier is structural: `PublicRandom` yields only integers/bytes,
    // and `Ikm` has no constructor from them - so a public value can never be
    // used as key material. (This is a compile-time guarantee; nothing to assert
    // at runtime beyond that the streams are independent.)
    println!("netcrypto-demo: two randomness classes (key-schedule vs public) OK");
    Ok(())
}

/// The nonce-reuse hazard guard: `SealingKey` owns a monotonic nonce (never
/// reused), round-trips through `OpeningKey`, and refuses to seal after a fork.
fn nonce_safety() -> Result<(), i32> {
    let key: [u8; 32] = h("000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f");
    let mut sk = SealingKey::new(chachapoly::ChaCha20Poly1305::new(key), [0, 0, 0, 1]);
    let ok = aead::OpeningKey::new(chachapoly::ChaCha20Poly1305::new(key));

    let m1 = sk.seal(b"aad", b"hello one").map_err(|_| 100)?;
    let m2 = sk.seal(b"aad", b"hello two").map_err(|_| 101)?;
    // Distinct nonces for distinct messages (the whole point).
    if m1.nonce == m2.nonce {
        return Err(102);
    }
    // Each opens correctly with its own nonce.
    match ok.open(&m1.nonce, b"aad", &m1.ciphertext, &m1.tag) {
        Some(d) if d.as_slice() == b"hello one" => {}
        _ => return Err(103),
    }
    match ok.open(&m2.nonce, b"aad", &m2.ciphertext, &m2.tag) {
        Some(d) if d.as_slice() == b"hello two" => {}
        _ => return Err(104),
    }
    // A fork/restore bumps the epoch; the surviving key must refuse to seal
    // (otherwise it could replay a (key, nonce) pair).
    rand::bump_fork_epoch();
    match sk.seal(b"aad", b"after fork") {
        Err(NonceError::ReseedRequired) => {}
        _ => return Err(105),
    }
    println!("netcrypto-demo: nonce-safe SealingKey (monotonic + fork-guard) OK");
    Ok(())
}
