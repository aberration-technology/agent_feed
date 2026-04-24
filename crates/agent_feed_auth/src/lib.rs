use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Principal {
    pub kind: String,
    pub stable_id: String,
    pub display: String,
}

impl Principal {
    #[must_use]
    pub fn github(id: impl Into<String>, login: impl Into<String>) -> Self {
        let stable_id = id.into();
        let login = login.into();
        Self {
            kind: "github".to_string(),
            stable_id: stable_id.clone(),
            display: format!("github:{login}"),
        }
    }
}
