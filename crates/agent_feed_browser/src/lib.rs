#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BrowserSeed {
    pub network_id: String,
    pub edge_base_url: String,
    pub bootstrap_peers: Vec<String>,
}

impl BrowserSeed {
    #[must_use]
    pub fn is_usable(&self) -> bool {
        !self.network_id.is_empty()
            && !self.edge_base_url.is_empty()
            && !self.bootstrap_peers.is_empty()
    }
}
