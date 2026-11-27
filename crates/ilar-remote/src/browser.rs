//! Relays as a browser can dial them. iroh names n0's relays with a
//! root dot (`euc1-1.relay.n0.iroh.link.`); Safari's WebSocket fails on
//! such a host (close 1006, seen on iOS 26.5) where Chrome does not.
//! The same relays without the dot work in both.

use iroh::{RelayMap, RelayUrl};

/// `url` without a root dot on its host.
pub fn relay_url(url: &RelayUrl) -> RelayUrl {
    match url.host_str() {
        Some(host) if host.len() > 1 && host.ends_with('.') => url
            .as_str()
            .replacen(host, host.trim_end_matches('.'), 1)
            .parse()
            .unwrap_or_else(|_| url.clone()),
        _ => url.clone(),
    }
}

/// n0's public relays, as [`relay_url`] writes them.
pub fn relay_map() -> RelayMap {
    iroh::defaults::prod::default_relay_map()
        .relays::<Vec<_>>()
        .into_iter()
        .map(|relay| {
            let mut relay = (*relay).clone();
            relay.url = relay_url(&relay.url);
            relay
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(text: &str) -> RelayUrl {
        text.parse().unwrap()
    }

    #[test]
    fn a_relay_url_loses_its_root_dot_and_keeps_the_rest() {
        assert_eq!(
            relay_url(&url("https://euc1-1.relay.n0.iroh.link./")),
            url("https://euc1-1.relay.n0.iroh.link/")
        );
        assert_eq!(
            relay_url(&url("https://relay.example.:8443/x")),
            url("https://relay.example:8443/x")
        );
        assert_eq!(
            relay_url(&url("https://relay.example/")),
            url("https://relay.example/")
        );
    }

    #[test]
    fn the_browser_relay_map_has_n0_s_relays_without_root_dots() {
        let urls: Vec<RelayUrl> = relay_map().urls();
        assert_eq!(urls.len(), iroh::defaults::prod::default_relay_map().len());
        assert!(urls.iter().all(|u| !u.host_str().unwrap().ends_with('.')));
        assert!(urls.contains(&url("https://euc1-1.relay.n0.iroh.link/")));
    }
}
