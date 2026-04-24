const INDEX_HTML: &str = include_str!("index.html");
const REEL_CSS: &str = include_str!("reel.css");
const REEL_JS: &str = include_str!("reel.ts");

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UiConfig {
    pub p2p_enabled: bool,
}

#[must_use]
pub fn render_index(view: Option<&str>) -> String {
    render_index_with_config(view, &UiConfig::default())
}

#[must_use]
pub fn render_index_with_config(view: Option<&str>, config: &UiConfig) -> String {
    let config_js = format!(
        "window.FEED_P2P_ENABLED = {};",
        if config.p2p_enabled { "true" } else { "false" }
    );
    INDEX_HTML
        .replace("/*__REEL_CSS__*/", REEL_CSS)
        .replace("/*__REEL_JS__*/", REEL_JS)
        .replace("/*__FEED_CONFIG__*/", &config_js)
        .replace("__REEL_VIEW__", view.unwrap_or("stage"))
}
