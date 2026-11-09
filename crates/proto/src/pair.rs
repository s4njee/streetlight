//! PIN pairing: SPAKE2 over the PIN, then each side MACs both TLS certificate
//! fingerprints with the shared key. A MITM can't complete the exchange without
//! the PIN, and the MACs bind the exact certificates seen on the wire.

use ring::hmac;
use spake2::{Ed25519Group, Identity, Password, Spake2};

pub const CLIENT_ID: &[u8] = b"streetlight-client";
pub const HOST_ID: &[u8] = b"streetlight-host";

pub fn fingerprint(cert_der: &[u8]) -> [u8; 32] {
    let d = ring::digest::digest(&ring::digest::SHA256, cert_der);
    d.as_ref().try_into().unwrap()
}

pub fn fingerprint_hex(fp: &[u8; 32]) -> String {
    fp.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn random_pin() -> String {
    format!("{:04}", rand::random_range(0..10000u32))
}

pub struct PairSide {
    state: Spake2<Ed25519Group>,
}

impl PairSide {
    pub fn client(pin: &str) -> (Self, Vec<u8>) {
        let (s, msg) = Spake2::<Ed25519Group>::start_a(
            &Password::new(pin.as_bytes()),
            &Identity::new(CLIENT_ID),
            &Identity::new(HOST_ID),
        );
        (Self { state: s }, msg)
    }

    pub fn host(pin: &str) -> (Self, Vec<u8>) {
        let (s, msg) = Spake2::<Ed25519Group>::start_b(
            &Password::new(pin.as_bytes()),
            &Identity::new(CLIENT_ID),
            &Identity::new(HOST_ID),
        );
        (Self { state: s }, msg)
    }

    pub fn finish(self, peer_msg: &[u8]) -> Option<PairKey> {
        self.state.finish(peer_msg).ok().map(|k| PairKey(hmac::Key::new(hmac::HMAC_SHA256, &k)))
    }
}

pub struct PairKey(hmac::Key);

impl PairKey {
    /// `role` distinguishes the two confirmations so one can't be reflected.
    pub fn mac(&self, role: &[u8], client_fp: &[u8; 32], host_fp: &[u8; 32]) -> Vec<u8> {
        let mut ctx = hmac::Context::with_key(&self.0);
        ctx.update(role);
        ctx.update(client_fp);
        ctx.update(host_fp);
        ctx.sign().as_ref().to_vec()
    }

    pub fn verify(&self, role: &[u8], client_fp: &[u8; 32], host_fp: &[u8; 32], mac: &[u8]) -> bool {
        let mut msg = Vec::with_capacity(role.len() + 64);
        msg.extend_from_slice(role);
        msg.extend_from_slice(client_fp);
        msg.extend_from_slice(host_fp);
        hmac::verify(&self.0, &msg, mac).is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pin_match_and_mismatch() {
        let (c, cm) = PairSide::client("1234");
        let (h, hm) = PairSide::host("1234");
        let (ck, hk) = (c.finish(&hm).unwrap(), h.finish(&cm).unwrap());
        let (cf, hf) = ([1u8; 32], [2u8; 32]);
        assert!(hk.verify(CLIENT_ID, &cf, &hf, &ck.mac(CLIENT_ID, &cf, &hf)));

        let (c, cm) = PairSide::client("1234");
        let (h, hm) = PairSide::host("9999");
        let (ck, hk) = (c.finish(&hm).unwrap(), h.finish(&cm).unwrap());
        assert!(!hk.verify(CLIENT_ID, &cf, &hf, &ck.mac(CLIENT_ID, &cf, &hf)));
    }
}
