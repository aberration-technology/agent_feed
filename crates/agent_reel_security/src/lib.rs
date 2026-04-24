use serde::{Deserialize, Serialize};
use std::net::{IpAddr, SocketAddr};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SecurityConfig {
    pub bind: SocketAddr,
    pub public_bind_requires_token: bool,
    pub display_token: Option<String>,
}

impl Default for SecurityConfig {
    fn default() -> Self {
        Self {
            bind: SocketAddr::from(([127, 0, 0, 1], 7777)),
            public_bind_requires_token: true,
            display_token: None,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SecurityError {
    #[error("non-loopback bind requires --display-token-file or an explicit display token")]
    PublicBindRequiresToken,
}

pub fn validate_bind(config: &SecurityConfig) -> Result<(), SecurityError> {
    if config.public_bind_requires_token
        && !is_loopback(config.bind.ip())
        && config.display_token.is_none()
    {
        return Err(SecurityError::PublicBindRequiresToken);
    }
    Ok(())
}

#[must_use]
pub fn is_loopback(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(addr) => addr.is_loopback(),
        IpAddr::V6(addr) => addr.is_loopback(),
    }
}
