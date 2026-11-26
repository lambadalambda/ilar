//! Who may control the session: the first peer with the secret, only.

use iroh::EndpointId;

pub const SECRET_LEN: usize = 16;

#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Secret(pub [u8; SECRET_LEN]);

impl Secret {
    pub fn generate() -> Self {
        let mut bytes = [0; SECRET_LEN];
        getrandom::fill(&mut bytes).expect("the OS has randomness");
        Self(bytes)
    }

    /// Compares without an early exit, so timing says nothing.
    fn matches(&self, other: &Secret) -> bool {
        self.0
            .iter()
            .zip(other.0)
            .fold(0, |acc, (a, b)| acc | (a ^ b))
            == 0
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret(..)")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// The secret was right; this peer is now the only one.
    Paired,
    /// The pinned peer came back.
    Welcome,
    Refused,
}

/// The pairing rule as a value: no I/O, so the host and tests share it.
#[derive(Debug)]
pub struct Pairing {
    secret: Secret,
    pinned: Option<EndpointId>,
}

impl Pairing {
    pub fn new(secret: Secret) -> Self {
        Self {
            secret,
            pinned: None,
        }
    }

    pub fn pinned(&self) -> Option<EndpointId> {
        self.pinned
    }

    pub fn admit(&mut self, peer: EndpointId, presented: Option<&Secret>) -> Verdict {
        match self.pinned {
            Some(pinned) if pinned == peer => Verdict::Welcome,
            Some(_) => Verdict::Refused,
            None if presented.is_some_and(|s| self.secret.matches(s)) => {
                self.pinned = Some(peer);
                Verdict::Paired
            }
            None => Verdict::Refused,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer() -> EndpointId {
        iroh::SecretKey::generate().public()
    }

    #[test]
    fn the_secret_pairs_the_first_peer() {
        let secret = Secret::generate();
        let mut pairing = Pairing::new(secret);
        let phone = peer();
        assert_eq!(pairing.admit(phone, Some(&secret)), Verdict::Paired);
        assert_eq!(pairing.pinned(), Some(phone));
    }

    #[test]
    fn a_wrong_or_missing_secret_is_refused() {
        let mut pairing = Pairing::new(Secret::generate());
        assert_eq!(
            pairing.admit(peer(), Some(&Secret::generate())),
            Verdict::Refused
        );
        assert_eq!(pairing.admit(peer(), None), Verdict::Refused);
        assert_eq!(pairing.pinned(), None);
    }

    #[test]
    fn the_secret_is_spent_after_pairing() {
        let secret = Secret::generate();
        let mut pairing = Pairing::new(secret);
        pairing.admit(peer(), Some(&secret));
        assert_eq!(pairing.admit(peer(), Some(&secret)), Verdict::Refused);
    }

    #[test]
    fn the_pinned_peer_comes_back_without_the_secret() {
        let secret = Secret::generate();
        let mut pairing = Pairing::new(secret);
        let phone = peer();
        pairing.admit(phone, Some(&secret));
        assert_eq!(pairing.admit(phone, None), Verdict::Welcome);
    }
}
