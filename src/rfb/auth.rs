//! Security-type math, kept pure so unit tests can pin it to fixed vectors. The I/O around it is in
//! [`super::handshake`].
//!
//! Apple's login (type 30) follows libvncclient's `HandleARDAuth`: Diffie-Hellman over the
//! server's group, MD5 of the shared secret (left-padded to the key length) as an AES-128 key, and
//! AES-128-ECB over a 128-byte block holding the username and password, each NUL-terminated in its
//! own 64-byte half and padded with random bytes.

use aes::Aes128;
use anyhow::{Context as _, bail, ensure};
use des::Des;
use des::cipher::{BlockCipherEncrypt as _, KeyInit as _, array::Array};
use md5::{Digest as _, Md5};
use num_bigint::BigUint;
use zeroize::Zeroizing;

use crate::base::Res;

/// Size of the encrypted ARD credentials block.
pub const ARD_BLOCK_LEN: usize = 128;
/// Each ARD field is 64 bytes including its NUL, so 63 usable bytes.
pub const ARD_FIELD_MAX: usize = 63;
/// Key lengths we accept from a server. Apple sends 128 (a 1024-bit group); the bounds only stop
/// a hostile server from making us allocate or exponentiate without limit.
pub const ARD_KEY_LEN: std::ops::RangeInclusive<usize> = 16..=512;

/// VNC authentication (type 2): DES-encrypt the 16-byte challenge under the password.
///
/// The key is the password's first 8 bytes, NUL-padded, with the bits of each byte reversed: a
/// quirk of the original VNC implementation that every server expects.
#[must_use]
pub fn vnc_response(password: &[u8], challenge: &[u8; 16]) -> [u8; 16] {
    let mut key = Zeroizing::new([0u8; 8]);
    for (k, p) in key.iter_mut().zip(password) {
        *k = p.reverse_bits();
    }
    let cipher = Des::new(&Array::from(*key));
    let mut response = *challenge;
    let (blocks, _) = Array::slice_as_chunks_mut(&mut response);
    cipher.encrypt_blocks(blocks);
    response
}

/// What an ARD server sends after the client picks type 30.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArdChallenge {
    pub generator: u16,
    /// The prime modulus; its length is the key length.
    pub prime: Vec<u8>,
    pub server_public: Vec<u8>,
}

/// What the client sends back: the encrypted credentials, then its public key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArdResponse {
    pub ciphertext: [u8; ARD_BLOCK_LEN],
    pub public_key: Vec<u8>,
}

/// Answers an ARD challenge with a fresh private key and random padding.
///
/// # Errors
///
/// When the challenge is degenerate, a credential does not fit its 63-byte field or holds a NUL,
/// or the OS has no randomness to give.
pub fn ard_response(challenge: &ArdChallenge, username: &str, password: &str) -> Res<ArdResponse> {
    let mut private = Zeroizing::new(vec![0u8; challenge.prime.len()]);
    let mut padding = Zeroizing::new([0u8; ARD_BLOCK_LEN]);
    getrandom::fill(&mut private).context("no randomness for the ARD private key")?;
    getrandom::fill(padding.as_mut()).context("no randomness for the ARD padding")?;
    ard_response_with(challenge, username, password, &private, &padding)
}

/// [`ard_response`] with the private key material and padding supplied, so tests are repeatable.
///
/// # Errors
///
/// As [`ard_response`].
pub fn ard_response_with(challenge: &ArdChallenge, username: &str, password: &str, private: &[u8], padding: &[u8; ARD_BLOCK_LEN]) -> Res<ArdResponse> {
    let key_len = challenge.prime.len();
    ensure!(ARD_KEY_LEN.contains(&key_len), "the server's ARD key length ({key_len} bytes) is out of range");
    ensure!(challenge.server_public.len() == key_len, "the server's ARD public key does not match its key length");

    // A server that picks weak parameters already receives the credentials it asked for, so these
    // checks only reject parameters that would make the exchange meaningless.
    let prime = BigUint::from_bytes_be(&challenge.prime);
    let generator = BigUint::from(challenge.generator);
    let server_public = BigUint::from_bytes_be(&challenge.server_public);
    let two = BigUint::from(2u8);
    let limit = &prime - &two;
    ensure!(prime.bit(0) && prime > BigUint::from(3u8), "the server's ARD modulus is not an odd prime");
    ensure!(generator >= two && generator <= limit, "the server's ARD generator is degenerate");
    ensure!(server_public >= two && server_public <= limit, "the server's ARD public key is degenerate");

    // A private exponent in [2, p - 2]. `num-bigint` neither zeroizes nor runs in constant time;
    // the exponent is ephemeral and only ever used against the server we are logging in to.
    let exponent = BigUint::from_bytes_be(private) % (&prime - BigUint::from(3u8)) + &two;
    let public_key = left_pad(&generator.modpow(&exponent, &prime), key_len)?;
    let shared = Zeroizing::new(left_pad(&server_public.modpow(&exponent, &prime), key_len)?);

    let block = ard_credentials_block(username, password, padding)?;
    let ciphertext = ard_encrypt(&shared, &block);
    Ok(ArdResponse { ciphertext, public_key })
}

/// The plaintext block: `username\0` at 0 and `password\0` at 64, the rest `padding`.
///
/// # Errors
///
/// When a credential is longer than 63 bytes or contains a NUL. Truncating would send a different
/// password than the user set, so it is an error instead.
pub fn ard_credentials_block(username: &str, password: &str, padding: &[u8; ARD_BLOCK_LEN]) -> Res<Zeroizing<[u8; ARD_BLOCK_LEN]>> {
    let mut block = Zeroizing::new(*padding);
    for (field, (label, value)) in block.as_chunks_mut::<{ ARD_BLOCK_LEN / 2 }>().0.iter_mut().zip([("username", username), ("password", password)]) {
        let bytes = value.as_bytes();
        if bytes.len() > ARD_FIELD_MAX || bytes.contains(&0) {
            bail!("the {label} must be at most {ARD_FIELD_MAX} bytes of UTF-8 with no NUL for Apple's login");
        }
        field[..bytes.len()].copy_from_slice(bytes);
        field[bytes.len()] = 0;
    }
    Ok(block)
}

/// AES-128-ECB over `block`, keyed by the MD5 of the (padded) shared secret.
#[must_use]
pub fn ard_encrypt(shared_secret: &[u8], block: &[u8; ARD_BLOCK_LEN]) -> [u8; ARD_BLOCK_LEN] {
    let cipher = Aes128::new(&Md5::digest(shared_secret));
    let mut out = *block;
    let (blocks, _) = Array::slice_as_chunks_mut(&mut out);
    cipher.encrypt_blocks(blocks);
    out
}

/// Big-endian bytes of `value`, left-padded with zeros to `len`. ARD hashes and sends the full
/// fixed-width field, leading zeros included.
fn left_pad(value: &BigUint, len: usize) -> Res<Vec<u8>> {
    let bytes = value.to_bytes_be();
    ensure!(bytes.len() <= len, "an ARD value outgrew its {len}-byte field");
    let mut out = vec![0u8; len - bytes.len()];
    out.extend_from_slice(&bytes);
    Ok(out)
}

#[cfg(test)]
mod unit {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use aes::cipher::BlockCipherDecrypt as _;
    use pretty_assertions::assert_eq;

    use super::*;
    use crate::tests::{OAKLEY_GROUP_2_PRIME, hex};

    #[test]
    fn vnc_response_matches_an_independent_vector() {
        // `openssl enc -des-ecb -nopad` with the bit-reversed key of "password" (0e86ceceeef64e26).
        let challenge: [u8; 16] = std::array::from_fn(|i| u8::try_from(i).unwrap());
        assert_eq!(hex(&vnc_response(b"password", &challenge)), "b866924125c8eebb9debc1db61c538e2");
    }

    #[test]
    fn vnc_response_uses_only_the_first_eight_bytes() {
        let challenge = [7u8; 16];
        assert_eq!(vnc_response(b"password", &challenge), vnc_response(b"password-and-more", &challenge));
        assert_ne!(vnc_response(b"password", &challenge), vnc_response(b"passwore", &challenge));
    }

    #[test]
    fn ard_cipher_matches_an_independent_vector() {
        // Python `hashlib.md5(bytes(127) + b'\x04')` as the key, then
        // `openssl enc -aes-128-ecb -nopad`; also the vector in madeye/rv's ard.rs.
        let mut secret = [0u8; 128];
        secret[127] = 4;
        let block = ard_credentials_block("alice", "secret", &[0x5a; 128]).unwrap();
        assert_eq!(
            hex(&ard_encrypt(&secret, &block)),
            concat!(
                "b688ebc3ba7889802d934560aff19f8e5b14c915fac67cb836f0caea911bf394",
                "5b14c915fac67cb836f0caea911bf3945b14c915fac67cb836f0caea911bf394",
                "6fde4f62502d59a78bf11a25b940214f5b14c915fac67cb836f0caea911bf394",
                "5b14c915fac67cb836f0caea911bf3945b14c915fac67cb836f0caea911bf394",
            )
        );
    }

    #[test]
    fn credentials_block_lays_out_both_fields() {
        let block = ard_credentials_block("bob", "pw", &[0xee; 128]).unwrap();
        assert_eq!(&block[..5], b"bob\0\xee");
        assert_eq!(&block[64..68], b"pw\0\xee");
        assert_eq!(block[127], 0xee);
    }

    #[test]
    fn credentials_must_fit_their_fields() {
        let pad = [0u8; 128];
        assert!(ard_credentials_block(&"a".repeat(63), &"é".repeat(31), &pad).is_ok());
        for (user, password) in [(&"a".repeat(64)[..], "pw"), ("user", &"é".repeat(32)[..]), ("us\0er", "pw"), ("user", "p\0w")] {
            let error = ard_credentials_block(user, password, &pad).unwrap_err().to_string();
            assert!(error.contains("at most 63 bytes"), "{error}");
        }
    }

    /// Plays the server's side of the exchange with its own private key and checks that it can
    /// decrypt what the client sent.
    #[test]
    fn ard_round_trips_through_a_server_side_key_agreement() {
        let prime = BigUint::from_bytes_be(&OAKLEY_GROUP_2_PRIME);
        let server_private = BigUint::from(0x1234_5678_9abc_def0_u64);
        let server_public = left_pad(&BigUint::from(2u8).modpow(&server_private, &prime), 128).unwrap();
        let challenge = ArdChallenge {
            generator: 2,
            prime: OAKLEY_GROUP_2_PRIME.to_vec(),
            server_public,
        };

        let response = ard_response(&challenge, "alice", "correct horse").unwrap();

        assert_eq!(response.public_key.len(), 128);
        let client_public = BigUint::from_bytes_be(&response.public_key);
        let shared = left_pad(&client_public.modpow(&server_private, &prime), 128).unwrap();
        let cipher = Aes128::new(&Md5::digest(&shared));
        let mut plain = response.ciphertext;
        let (blocks, _) = Array::slice_as_chunks_mut(&mut plain);
        cipher.decrypt_blocks(blocks);
        assert_eq!(&plain[..6], b"alice\0");
        assert_eq!(&plain[64..78], b"correct horse\0");
    }

    #[test]
    fn ard_is_deterministic_given_its_randomness() {
        let challenge = ArdChallenge {
            generator: 2,
            prime: OAKLEY_GROUP_2_PRIME.to_vec(),
            server_public: left_pad(&BigUint::from(5u8), 128).unwrap(),
        };
        let a = ard_response_with(&challenge, "u", "p", &[1; 128], &[2; 128]).unwrap();
        let b = ard_response_with(&challenge, "u", "p", &[1; 128], &[2; 128]).unwrap();
        assert_eq!(a, b);
        let c = ard_response_with(&challenge, "u", "p", &[3; 128], &[2; 128]).unwrap();
        assert_ne!(a.public_key, c.public_key);
    }

    #[test]
    fn ard_rejects_degenerate_parameters() {
        let ok = ArdChallenge {
            generator: 2,
            prime: OAKLEY_GROUP_2_PRIME.to_vec(),
            server_public: left_pad(&BigUint::from(5u8), 128).unwrap(),
        };
        let mut even = ok.clone();
        even.prime[127] &= 0xfe;
        let mut short = ok.clone();
        short.prime.truncate(8);
        short.server_public.truncate(8);
        let mut public_one = ok.clone();
        public_one.server_public = left_pad(&BigUint::from(1u8), 128).unwrap();
        let mut generator_one = ok.clone();
        generator_one.generator = 1;
        let mut mismatched = ok.clone();
        mismatched.server_public.pop();
        for bad in [even, short, public_one, generator_one, mismatched] {
            assert!(ard_response(&bad, "u", "p").is_err(), "{bad:?}");
        }
        assert!(ard_response(&ok, "u", "p").is_ok());
    }
}
