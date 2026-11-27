//! Who may control the session: the peers that showed the secret while
//! the pairing window was open.

use iroh::EndpointId;
use std::collections::HashSet;

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
    /// The secret was right; this peer is now paired.
    Paired,
    /// A paired peer came back.
    Welcome,
    Refused,
}

/// The pairing rule as a value: no I/O, so the host and tests share it.
/// The secret pairs a new peer only while the window is open (the TUI
/// opens it while the QR code is on screen); a paired peer comes back
/// at any time.
#[derive(Debug)]
pub struct Pairing {
    secret: Secret,
    paired: HashSet<EndpointId>,
    open: bool,
}

impl Pairing {
    /// Open: a start shows the code.
    pub fn new(secret: Secret) -> Self {
        Self {
            secret,
            paired: HashSet::new(),
            open: true,
        }
    }

    pub fn paired(&self) -> usize {
        self.paired.len()
    }

    pub fn set_open(&mut self, open: bool) {
        self.open = open;
    }

    pub fn admit(&mut self, peer: EndpointId, presented: Option<&Secret>) -> Verdict {
        if self.paired.contains(&peer) {
            Verdict::Welcome
        } else if self.open && presented.is_some_and(|s| self.secret.matches(s)) {
            self.paired.insert(peer);
            Verdict::Paired
        } else {
            Verdict::Refused
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
    fn the_secret_pairs_each_peer_while_the_window_is_open() {
        let secret = Secret::generate();
        let mut pairing = Pairing::new(secret);
        let (phone, desktop) = (peer(), peer());
        assert_eq!(pairing.admit(phone, Some(&secret)), Verdict::Paired);
        assert_eq!(pairing.admit(desktop, Some(&secret)), Verdict::Paired);
        assert_eq!(pairing.paired(), 2);
    }

    #[test]
    fn a_closed_window_refuses_new_peers_and_lets_known_ones_back() {
        let secret = Secret::generate();
        let mut pairing = Pairing::new(secret);
        let phone = peer();
        pairing.admit(phone, Some(&secret));
        pairing.set_open(false);
        assert_eq!(pairing.admit(peer(), Some(&secret)), Verdict::Refused);
        assert_eq!(pairing.admit(phone, None), Verdict::Welcome);
        pairing.set_open(true);
        assert_eq!(pairing.admit(peer(), Some(&secret)), Verdict::Paired);
    }

    #[test]
    fn a_wrong_or_missing_secret_is_refused() {
        let mut pairing = Pairing::new(Secret::generate());
        assert_eq!(
            pairing.admit(peer(), Some(&Secret::generate())),
            Verdict::Refused
        );
        assert_eq!(pairing.admit(peer(), None), Verdict::Refused);
        assert_eq!(pairing.paired(), 0);
    }

    #[test]
    fn a_paired_peer_comes_back_without_the_secret() {
        let secret = Secret::generate();
        let mut pairing = Pairing::new(secret);
        let phone = peer();
        pairing.admit(phone, Some(&secret));
        assert_eq!(pairing.admit(phone, None), Verdict::Welcome);
    }
}
