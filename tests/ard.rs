//! Apple's login (security type 30) against a mock of the macOS Screen Sharing server. rustvncserver
//! has no type 30, so the server side of the exchange is played here with its own key agreement
//! and decryption, sharing nothing with the client's code but the crates.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use aes::{
    Aes128,
    cipher::{BlockCipherDecrypt as _, KeyInit as _, array::Array},
};
use md5::{Digest as _, Md5};
use num_bigint::BigUint;
use peephole::{
    credentials::{Credentials, Secret},
    rfb::{AuthError, PixelFormat, Version, handshake, proto::security},
    tests::OAKLEY_GROUP_2_PRIME,
};
use pretty_assertions::assert_eq;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _, DuplexStream, duplex};

/// What the mock server saw.
#[derive(Debug)]
struct Seen {
    username: String,
    password: String,
    ciphertext: Vec<u8>,
}

fn field(block: &[u8]) -> String {
    let end = block.iter().position(|&b| b == 0).expect("a NUL-terminated field");
    String::from_utf8(block[..end].to_vec()).unwrap()
}

fn padded(value: &BigUint) -> Vec<u8> {
    let bytes = value.to_bytes_be();
    [vec![0; 128 - bytes.len()], bytes].concat()
}

/// Plays macOS: announces 003.889, offers `offered`, runs the type-30 exchange, and accepts only
/// `expected` (username, password).
async fn mock_mac(mut s: DuplexStream, offered: &[u8], expected: (&str, &str)) -> Option<Seen> {
    s.write_all(b"RFB 003.889\n").await.unwrap();
    let mut version = [0u8; 12];
    s.read_exact(&mut version).await.unwrap();
    assert_eq!(&version, b"RFB 003.008\n", "answer Apple's 3.889 as 3.8, like libvncclient");

    s.write_all(&[u8::try_from(offered.len()).unwrap()]).await.unwrap();
    s.write_all(offered).await.unwrap();
    // A client without usable credentials hangs up instead of choosing.
    let chosen = s.read_u8().await.ok()?;
    assert_eq!(chosen, security::ARD);

    let prime = BigUint::from_bytes_be(&OAKLEY_GROUP_2_PRIME);
    let private = BigUint::from(0x0bad_cafe_f00d_u64);
    s.write_u16(2).await.unwrap();
    s.write_u16(128).await.unwrap();
    s.write_all(&OAKLEY_GROUP_2_PRIME).await.unwrap();
    s.write_all(&padded(&BigUint::from(2u8).modpow(&private, &prime))).await.unwrap();

    let mut ciphertext = [0u8; 128];
    let mut client_public = [0u8; 128];
    s.read_exact(&mut ciphertext).await.unwrap();
    s.read_exact(&mut client_public).await.unwrap();
    let shared = padded(&BigUint::from_bytes_be(&client_public).modpow(&private, &prime));
    let cipher = Aes128::new(&Md5::digest(&shared));
    let mut plain = ciphertext;
    let (blocks, _) = Array::slice_as_chunks_mut(&mut plain);
    cipher.decrypt_blocks(blocks);
    let seen = Seen {
        username: field(&plain[..64]),
        password: field(&plain[64..]),
        ciphertext: ciphertext.to_vec(),
    };

    if (seen.username.as_str(), seen.password.as_str()) == expected {
        s.write_u32(0).await.unwrap();
        assert_eq!(s.read_u8().await.unwrap(), 1, "shared flag");
        s.write_all(&[0x0d, 0x80, 0x08, 0xba]).await.unwrap();
        s.write_all(&PixelFormat::RGBX.to_bytes()).await.unwrap();
        s.write_u32(7).await.unwrap();
        s.write_all(b"Mac Pro").await.unwrap();
    } else {
        let reason = b"Authentication failed";
        s.write_u32(1).await.unwrap();
        s.write_u32(u32::try_from(reason.len()).unwrap()).await.unwrap();
        s.write_all(reason).await.unwrap();
    }
    Some(seen)
}

fn creds(user: Option<&str>, password: &str) -> Credentials {
    Credentials {
        username: user.map(str::to_owned),
        password: Some(Secret::new(password.to_owned())),
    }
}

#[tokio::test]
async fn logs_in_with_apples_login() {
    let (mut client, server) = duplex(4096);
    let mac = tokio::spawn(mock_mac(server, &[security::VNC, security::ARD], ("alice", "correct horse")));

    let done = handshake(&mut client, &creds(Some("alice"), "correct horse")).await.unwrap();

    let seen = mac.await.unwrap().unwrap();
    assert_eq!((seen.username.as_str(), seen.password.as_str()), ("alice", "correct horse"));
    assert_eq!(done.version, Version::V3_8);
    assert_eq!(done.security, security::ARD);
    assert_eq!((done.init.width, done.init.height, done.init.name.as_str()), (3456, 2234, "Mac Pro"));
}

#[tokio::test]
async fn a_rejected_login_is_terminal_and_never_echoes_the_password() {
    let (mut client, server) = duplex(4096);
    let mac = tokio::spawn(mock_mac(server, &[security::ARD], ("alice", "correct horse")));

    let error = handshake(&mut client, &creds(Some("alice"), "battery staple")).await.unwrap_err();

    assert_eq!(mac.await.unwrap().unwrap().password, "battery staple");
    assert!(error.is::<AuthError>(), "{error:#}");
    let message = format!("{error:#}");
    assert!(message.contains("Authentication failed"), "{message}");
    assert!(!message.contains("battery staple"), "{message}");
}

#[tokio::test]
async fn needs_a_username_for_apples_login() {
    let (mut client, server) = duplex(4096);
    let mac = tokio::spawn(mock_mac(server, &[security::ARD], ("alice", "pw")));

    let error = handshake(&mut client, &creds(None, "pw")).await.unwrap_err();
    drop(client);

    assert!(mac.await.unwrap().is_none(), "nothing may be sent without a username");
    assert!(error.is::<AuthError>(), "{error:#}");
    assert!(format!("{error:#}").contains("PEEPHOLE_USER"), "{error:#}");
}

#[tokio::test]
async fn pads_the_credentials_with_fresh_randomness() {
    let mut ciphertexts = Vec::new();
    for _ in 0..2 {
        let (mut client, server) = duplex(4096);
        let mac = tokio::spawn(mock_mac(server, &[security::ARD], ("alice", "pw")));
        handshake(&mut client, &creds(Some("alice"), "pw")).await.unwrap();
        ciphertexts.push(mac.await.unwrap().unwrap().ciphertext);
    }
    assert_ne!(ciphertexts[0], ciphertexts[1]);
}
