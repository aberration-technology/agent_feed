const INDEX_HTML: &str = include_str!("index.html");
const REEL_CSS: &str = include_str!("reel.css");
const REEL_JS: &str = include_str!("reel.ts");

#[must_use]
pub fn render_index(view: Option<&str>) -> String {
    INDEX_HTML
        .replace("/*__REEL_CSS__*/", REEL_CSS)
        .replace("/*__REEL_JS__*/", REEL_JS)
        .replace("__REEL_VIEW__", view.unwrap_or("stage"))
}
