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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_bind_is_allowed_without_display_token() {
        let config = SecurityConfig {
            bind: SocketAddr::from(([127, 0, 0, 1], 7777)),
            display_token: None,
            ..SecurityConfig::default()
        };

        validate_bind(&config).expect("loopback bind is local-only");
    }

    #[test]
    fn public_bind_requires_display_token_by_default() {
        let config = SecurityConfig {
            bind: SocketAddr::from(([0, 0, 0, 0], 7777)),
            display_token: None,
            ..SecurityConfig::default()
        };

        assert!(matches!(
            validate_bind(&config),
            Err(SecurityError::PublicBindRequiresToken)
        ));
    }

    #[test]
    fn public_bind_is_allowed_with_explicit_display_token() {
        let config = SecurityConfig {
            bind: SocketAddr::from(([0, 0, 0, 0], 7777)),
            display_token: Some("display-token".to_string()),
            ..SecurityConfig::default()
        };

        validate_bind(&config).expect("token gates non-loopback display");
    }
}
