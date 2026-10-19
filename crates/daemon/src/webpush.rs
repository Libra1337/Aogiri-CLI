//! Web Push message encryption (RFC 8291, `aes128gcm` of RFC 8188, one
//! record): the daemon encrypts each notification for the phone's browser,
//! so the relay passes on ciphertext it cannot read.

use ring::{aead, agreement, hkdf, rand::SecureRandom};

/// The record size written in the header (the whole message is one record).
const RECORD_SIZE: u32 = 4096;

struct Len(usize);

impl hkdf::KeyType for Len {
    fn len(&self) -> usize {
        self.0
    }
}

fn expand(prk: &hkdf::Prk, info: &[u8], len: usize) -> Result<Vec<u8>, String> {
    let mut out = vec![0u8; len];
    prk.expand(&[info], Len(len))
        .and_then(|okm| okm.fill(&mut out))
        .map_err(|_| "hkdf failed".to_string())?;
    Ok(out)
}

/// Encrypts `plaintext` for the browser key `ua_public` (65-byte P-256
/// point) and `auth_secret` (16 bytes).
pub fn encrypt(plaintext: &[u8], ua_public: &[u8], auth_secret: &[u8]) -> Result<Vec<u8>, String> {
    let rng = ring::rand::SystemRandom::new();
    let key = agreement::EphemeralPrivateKey::generate(&agreement::ECDH_P256, &rng)
        .map_err(|_| "key generation failed".to_string())?;
    let mut salt = [0u8; 16];
    rng.fill(&mut salt).map_err(|_| "rng failed".to_string())?;
    encrypt_with(plaintext, ua_public, auth_secret, key, &salt)
}

fn encrypt_with(
    plaintext: &[u8],
    ua_public: &[u8],
    auth_secret: &[u8],
    key: agreement::EphemeralPrivateKey,
    salt: &[u8; 16],
) -> Result<Vec<u8>, String> {
    if auth_secret.len() != 16 {
        return Err("the subscription's auth secret is not 16 bytes".to_string());
    }
    if plaintext.len() + 1 + 16 > RECORD_SIZE as usize - 86 {
        return Err("the notification is too long".to_string());
    }
    let as_public = key
        .compute_public_key()
        .map_err(|_| "key generation failed".to_string())?;
    let as_public = as_public.as_ref().to_vec();
    // agree_ephemeral checks that the browser's point is on the curve.
    let peer = agreement::UnparsedPublicKey::new(&agreement::ECDH_P256, ua_public);
    let shared = agreement::agree_ephemeral(key, &peer, |secret| secret.to_vec())
        .map_err(|_| "the subscription's p256dh key is not a P-256 point".to_string())?;

    let mut key_info = b"WebPush: info\0".to_vec();
    key_info.extend_from_slice(ua_public);
    key_info.extend_from_slice(&as_public);
    let prk_key = hkdf::Salt::new(hkdf::HKDF_SHA256, auth_secret).extract(&shared);
    let ikm = expand(&prk_key, &key_info, 32)?;
    let prk = hkdf::Salt::new(hkdf::HKDF_SHA256, salt).extract(&ikm);
    let cek = expand(&prk, b"Content-Encoding: aes128gcm\0", 16)?;
    let nonce = expand(&prk, b"Content-Encoding: nonce\0", 12)?;

    let sealing = aead::LessSafeKey::new(
        aead::UnboundKey::new(&aead::AES_128_GCM, &cek).map_err(|_| "aes key".to_string())?,
    );
    let mut record = plaintext.to_vec();
    // The last record's padding delimiter.
    record.push(2);
    sealing
        .seal_in_place_append_tag(
            aead::Nonce::try_assume_unique_for_key(&nonce).map_err(|_| "nonce".to_string())?,
            aead::Aad::empty(),
            &mut record,
        )
        .map_err(|_| "encryption failed".to_string())?;

    let mut message = Vec::with_capacity(86 + record.len());
    message.extend_from_slice(salt);
    message.extend_from_slice(&RECORD_SIZE.to_be_bytes());
    message.push(as_public.len() as u8);
    message.extend_from_slice(&as_public);
    message.extend_from_slice(&record);
    Ok(message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};

    fn b64(text: &str) -> Vec<u8> {
        URL_SAFE_NO_PAD.decode(text.replace(' ', "")).unwrap()
    }

    // RFC 8291 section 5 and appendix A. ring takes no private key from
    // outside, so the sender's key comes from a fixed "random" source whose
    // bytes are the RFC's private key.
    #[test]
    fn matches_the_rfc_8291_example() {
        #[allow(deprecated)]
        let rng = ring::test::rand::FixedSliceRandom {
            bytes: &b64("yfWPiYE-n46HLnH0KqZOF1fJJU3MYrct3AELtAQ-oRw"),
        };
        let key = agreement::EphemeralPrivateKey::generate(&agreement::ECDH_P256, &rng).unwrap();
        let salt: [u8; 16] = b64("DGv6ra1nlYgDCS1FRnbzlw").try_into().unwrap();
        let message = encrypt_with(
            b"When I grow up, I want to be a watermelon",
            &b64("BCVxsr7N_eNgVRqvHtD0zTZsEc6-VV-JvLexhqUzORcx aOzi6-AYWXvTBHm4bjyPjs7Vd8pZGH6SRpkNtoIAiw4"),
            &b64("BTBZMqHH6r4Tts7J_aSIgg"),
            key,
            &salt,
        )
        .unwrap();
        let want = b64(concat!(
            "DGv6ra1nlYgDCS1FRnbzlwAAEABBBP4z9KsN6nGRTbVYI_c7VJSPQTBtkgcy27ml",
            "mlMoZIIgDll6e3vCYLocInmYWAmS6TlzAC8wEqKK6PBru3jl7A_yl95bQpu6cVPT",
            "pK4Mqgkf1CXztLVBSt2Ks3oZwbuwXPXLWyouBWLVWGNWQexSgSxsj_Qulcy4a-fN"
        ));
        assert_eq!(
            URL_SAFE_NO_PAD.encode(message),
            URL_SAFE_NO_PAD.encode(want)
        );
    }

    #[test]
    fn refuses_a_key_that_is_not_a_point() {
        let mut bad = vec![4u8; 65];
        bad[64] = 1;
        assert!(encrypt(b"x", &bad, &[0; 16]).is_err());
        assert!(encrypt(b"x", &[4; 3], &[0; 16]).is_err());
        let ua = b64("BCVxsr7N_eNgVRqvHtD0zTZsEc6-VV-JvLexhqUzORcxaOzi6-AYWXvTBHm4bjyPjs7Vd8pZGH6SRpkNtoIAiw4");
        assert!(encrypt(b"x", &ua, &[0; 8]).is_err(), "auth secret length");
        assert!(encrypt(&[b'x'; 4000], &ua, &[0; 16]).is_err(), "too long");
        assert_eq!(encrypt(b"x", &ua, &[0; 16]).unwrap().len(), 86 + 2 + 16);
    }
}
