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

#[cfg(test)]
mod tests {
    use super::{UiConfig, render_index_with_config};

    #[test]
    fn stage_progress_starts_hidden() {
        let html = render_index_with_config(Some("remote"), &UiConfig { p2p_enabled: true });

        assert!(html.contains("id=\"stage-progress\""));
        assert!(html.contains("aria-hidden=\"true\" hidden"));
    }

    #[test]
    fn idle_state_avoids_redundant_local_status_chips() {
        let html = render_index_with_config(Some("stage"), &UiConfig { p2p_enabled: false });

        assert!(html.contains("<div class=\"brand\">feed</div>"));
        assert!(html.contains("id=\"eyebrow\">local feed</div>"));
        assert!(html.contains("<span>privacy on</span>"));
        assert!(!html.contains("LOCAL / QUIET / IDLE"));
        assert!(!html.contains(
            "<span>local</span>\n          <span>redacted</span>\n          <span>idle</span>"
        ));
        assert!(html.contains("setText(eyebrow, \"p2p disabled\");"));
        assert!(html.contains("renderChips([\"p2p off\", \"privacy on\"]);"));
    }

    #[test]
    fn chrome_uses_lowercase_accented_site_links() {
        let html = render_index_with_config(Some("stage"), &UiConfig { p2p_enabled: false });

        assert!(html.contains("--secondary: #d87c7c;"));
        assert!(html.contains(".brand {\n  color: var(--secondary);\n}"));
        assert!(html.contains(".footer-links a {\n  color: var(--secondary);"));
        assert!(html.contains("text-decoration: underline;"));
        assert!(!html.contains("text-transform: uppercase;"));
    }

    #[test]
    fn remote_states_stop_dwell_progress() {
        let html = render_index_with_config(Some("remote"), &UiConfig { p2p_enabled: true });
        let remote_state = html
            .split("function renderRemoteState")
            .nth(1)
            .expect("remote state renderer is embedded");

        assert!(remote_state.contains("stopStageProgress();"));
        assert!(!remote_state.contains("restartStageProgress"));
    }

    #[test]
    fn browser_console_logs_feed_lifecycle_events() {
        let html = render_index_with_config(Some("remote"), &UiConfig { p2p_enabled: true });

        assert!(html.contains("function logEvent"));
        assert!(html.contains("feed.remote.route.start"));
        assert!(html.contains("feed.resolver.response"));
        assert!(html.contains("feed.sse.bulletin.incoming"));
    }
}
