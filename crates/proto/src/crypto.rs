//! Per-packet AES-128-GCM (hardware accelerated on x86 AES-NI, ARMv8 CE, Apple silicon).

use ring::aead::{AES_128_GCM, Aad, LessSafeKey, Nonce, UnboundKey};

use crate::media::{HEADER_LEN, MediaHeader, TAG_LEN};

pub const KEY_LEN: usize = 16;

pub struct MediaCipher {
    key: LessSafeKey,
}

impl MediaCipher {
    pub fn new(key: &[u8; KEY_LEN]) -> Self {
        Self { key: LessSafeKey::new(UnboundKey::new(&AES_128_GCM, key).expect("key length")) }
    }

    pub fn random_key() -> [u8; KEY_LEN] {
        let mut k = [0u8; KEY_LEN];
        rand::fill(&mut k);
        k
    }

    /// `packet` = header || plaintext || [TAG_LEN spare bytes]. Encrypts in place.
    pub fn seal(&self, hdr: &MediaHeader, packet: &mut [u8]) {
        let (head, rest) = packet.split_at_mut(HEADER_LEN);
        let body_len = rest.len() - TAG_LEN;
        let (body, tag_out) = rest.split_at_mut(body_len);
        let tag = self
            .key
            .seal_in_place_separate_tag(Nonce::assume_unique_for_key(hdr.nonce()), Aad::from(&*head), body)
            .expect("seal");
        tag_out.copy_from_slice(tag.as_ref());
    }

    /// Decrypts in place; returns the plaintext shard slice.
    pub fn open<'a>(&self, hdr: &MediaHeader, packet: &'a mut [u8]) -> Option<&'a mut [u8]> {
        if packet.len() < HEADER_LEN + TAG_LEN {
            return None;
        }
        let (head, rest) = packet.split_at_mut(HEADER_LEN);
        self.key
            .open_in_place(Nonce::assume_unique_for_key(hdr.nonce()), Aad::from(&*head), rest)
            .ok()
    }
}
