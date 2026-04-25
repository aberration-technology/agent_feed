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

#[must_use]
pub fn requires_display_token(config: &SecurityConfig) -> bool {
    !is_loopback(config.bind.ip()) && config.display_token.is_some()
}

#[must_use]
pub fn token_matches(expected: &str, actual: &str) -> bool {
    let left = expected.as_bytes();
    let right = actual.as_bytes();
    if left.len() != right.len() {
        return false;
    }
    left.iter()
        .zip(right)
        .fold(0u8, |acc, (left, right)| acc | (left ^ right))
        == 0
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

    #[test]
    fn display_token_matching_is_constant_time_shape_and_exact() {
        assert!(token_matches("display-token", "display-token"));
        assert!(!token_matches("display-token", "display-t0ken"));
        assert!(!token_matches("display-token", "display-token-extra"));
    }
}
