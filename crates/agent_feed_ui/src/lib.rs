const INDEX_HTML: &str = include_str!("index.html");
const REEL_CSS: &str = include_str!("reel.css");
const REEL_JS: &str = include_str!("reel.ts");
pub const FAVICON_SVG: &str = include_str!("favicon.svg");

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UiConfig {
    pub p2p_enabled: bool,
    pub revision: Option<String>,
}

#[must_use]
pub fn render_index(view: Option<&str>) -> String {
    render_index_with_config(view, &UiConfig::default())
}

#[must_use]
pub fn render_index_with_config(view: Option<&str>, config: &UiConfig) -> String {
    let revision = config
        .revision
        .clone()
        .or_else(default_revision)
        .unwrap_or_else(|| "dev".to_string());
    let config_js = format!(
        "window.FEED_P2P_ENABLED = {};\nwindow.FEED_BUILD_REV = {};\nwindow.FEED_COMPATIBILITY = {};",
        if config.p2p_enabled { "true" } else { "false" },
        js_string(&revision),
        agent_feed_p2p_proto::ProtocolCompatibility::current_json_string()
    );
    INDEX_HTML
        .replace("/*__REEL_CSS__*/", REEL_CSS)
        .replace("/*__REEL_JS__*/", REEL_JS)
        .replace("/*__FEED_CONFIG__*/", &config_js)
        .replace("__REEL_VIEW__", view.unwrap_or("stage"))
}

fn default_revision() -> Option<String> {
    option_env!("GITHUB_SHA")
        .or(option_env!("VERGEN_GIT_SHA"))
        .map(short_revision)
}

fn short_revision(value: &str) -> String {
    value.chars().take(12).collect::<String>()
}

fn js_string(value: &str) -> String {
    let mut output = String::from("\"");
    for ch in value.chars() {
        match ch {
            '\\' => output.push_str("\\\\"),
            '"' => output.push_str("\\\""),
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            '\t' => output.push_str("\\t"),
            ch if ch.is_control() => output.push_str(&format!("\\u{:04x}", ch as u32)),
            ch => output.push(ch),
        }
    }
    output.push('"');
    output
}

#[cfg(test)]
mod tests {
    use super::{UiConfig, render_index_with_config};

    fn config(p2p_enabled: bool) -> UiConfig {
        UiConfig {
            p2p_enabled,
            revision: Some("abc123def456".to_string()),
        }
    }

    #[test]
    fn stage_progress_starts_hidden() {
        let html = render_index_with_config(Some("remote"), &config(true));

        assert!(html.contains("id=\"stage-progress\""));
        assert!(html.contains("aria-hidden=\"true\" hidden"));
    }

    #[test]
    fn stage_story_time_is_subtle_and_story_bound() {
        let html = render_index_with_config(Some("remote"), &config(true));

        assert!(html.contains("class=\"story-time\" id=\"story-time\" hidden"));
        assert!(html.contains(".story-time {\n  position: absolute;"));
        assert!(html.contains("color: rgba(255, 255, 255, 0.16);"));
        assert!(html.contains("function renderStoryTime"));
        assert!(html.contains("storyTime.textContent = `posted ${relativeTime(timestamp)}`;"));
        assert!(html.contains("function clearStoryTime"));
        assert!(html.contains("refreshStoryTime();"));
        assert!(html.contains(".story-time {\n    right: 28px;\n    bottom: 28px;"));
        assert!(!html.contains(".story-time {\n    display: none;"));
    }

    #[test]
    fn favicon_matches_feed_mark() {
        let html = render_index_with_config(Some("stage"), &config(false));

        assert!(html.contains("href=\"./favicon.svg\" type=\"image/svg+xml\""));
        assert!(super::FAVICON_SVG.contains("rx=\"12\" fill=\"#000000\""));
        assert!(super::FAVICON_SVG.contains("fill=\"#d87c7c\""));
        assert!(super::FAVICON_SVG.contains("V30h-7v-8h7v-3"));
    }

    #[test]
    fn idle_state_avoids_redundant_local_status_chips() {
        let html = render_index_with_config(Some("stage"), &config(false));

        assert!(
            html.contains(
                "<a class=\"brand\" href=\"https://feed.aberration.technology/\">feed</a>"
            )
        );
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
        let html = render_index_with_config(Some("stage"), &config(false));

        assert!(html.contains("--sans: ui-monospace, monospace;"));
        assert!(html.contains("--mono: ui-monospace, monospace;"));
        assert!(html.contains("font-family: ui-monospace, monospace;"));
        assert!(!html.contains("ui-sans-serif"));
        assert!(!html.contains("SFMono-Regular"));
        assert!(html.contains("--secondary: #d87c7c;"));
        assert!(html.contains(".brand {\n  color: var(--secondary);\n  text-decoration: none;\n}"));
        assert!(html.contains(".brand:hover,\n.brand:focus-visible"));
        assert!(html.contains(".footer-links a {\n  color: var(--secondary);"));
        assert!(html.contains("id=\"footer-rev\">rev dev</span>"));
        assert!(html.contains(".footer-rev"));
        assert!(html.contains("window.FEED_BUILD_REV = \"abc123def456\";"));
        assert!(html.contains("href=\"https://aberration.technology/\""));
        assert!(html.contains("href=\"https://github.com/aberration-technology\""));
        assert!(!html.contains("href=\"https://github.com/aberration-technology/agent_feed\""));
        assert!(html.contains("text-decoration: underline;"));
        assert!(!html.contains("text-transform: uppercase;"));
    }

    #[test]
    fn headline_typography_keeps_safe_line_spacing() {
        let html = render_index_with_config(Some("stage"), &config(false));

        assert!(html.contains("--headline: clamp(50px, 6.8vw, 110px);"));
        assert!(html.contains("--headline-leading: 1.16;"));
        assert!(html.contains("--deck: clamp(23px, 2.55vw, 40px);"));
        assert!(html.contains("--deck-leading: 1.16;"));
        assert!(html.contains("h1 {\n  width: 100%;\n  min-width: 0;"));
        assert!(html.contains("--headline-max: min(100%, 28ch);"));
        assert!(html.contains("max-width: var(--headline-max-fit, var(--headline-max));"));
        assert!(html.contains("max-width: var(--deck-max-fit, var(--deck-max));"));
        assert!(html.contains("font-size: var(--headline-fit, var(--headline));"));
        assert!(html.contains("font-size: var(--deck-fit, var(--deck));"));
        assert!(html.contains("line-height: var(--headline-leading);"));
        assert!(html.contains("line-height: var(--deck-leading);"));
        assert!(html.contains("text-wrap: balance;"));
        assert!(html.contains("text-wrap: pretty;"));
        assert!(html.contains("overflow-wrap: break-word;"));
        assert!(html.contains("word-break: normal;"));
        assert!(!html.contains("h1 {\n  max-width: 14ch;\n  margin: 0;\n  color: var(--fg);\n  font-size: var(--headline);\n  font-weight: 620;\n  letter-spacing: 0;\n  line-height: var(--headline-leading);\n  overflow-wrap: anywhere;"));
        assert!(!html.contains("line-height: 0.94;"));
    }

    #[test]
    fn mobile_headlines_keep_room_for_large_text_blobs() {
        let html = render_index_with_config(Some("stage"), &config(false));

        let mobile = html
            .split("@media (max-width: 720px)")
            .nth(1)
            .expect("mobile breakpoint is embedded");

        assert!(mobile.contains("--headline: 27px;"));
        assert!(mobile.contains("--headline-leading: 1.28;"));
        assert!(mobile.contains("--deck: 15px;"));
        assert!(mobile.contains("--deck-leading: 1.26;"));
        assert!(html.contains(".reel {\n  width: 100vw;\n  max-width: 100vw;"));
        assert!(mobile.contains("height: 100svh;"));
        assert!(mobile.contains(
            "grid-template-rows: 64px minmax(0, 1fr) calc(58px + env(safe-area-inset-bottom, 0px));"
        ));
        assert!(mobile.contains("@supports (height: 100dvh)"));
        assert!(mobile.contains("min-height: calc(58px + env(safe-area-inset-bottom, 0px));"));
        assert!(mobile.contains("padding-bottom: env(safe-area-inset-bottom, 0px);"));
        assert!(mobile.contains("#ticker {\n    display: none;"));
        assert!(mobile.contains(".footer-rev {\n    display: inline-block;"));
        assert!(mobile.contains("--headline-max: 100%;"));
        assert!(mobile.contains("--deck-max: 100%;"));
        assert!(mobile.contains("display: block;"));
        assert!(mobile.contains("max-height: none;"));
        assert!(mobile.contains("overflow: visible;"));
        assert!(mobile.contains("overflow-wrap: break-word;"));
        assert!(mobile.contains("text-wrap: wrap;"));
        assert!(mobile.contains("white-space: normal;"));
        assert!(!mobile.contains("-webkit-line-clamp"));
        assert!(!mobile.contains("-webkit-box-orient"));
        assert!(mobile.contains(".stage {\n    --stage-gap: 16px;"));
        assert!(mobile.contains("max-width: 100vw;"));
        assert!(mobile.contains("overflow-x: hidden;"));
        assert!(mobile.contains("padding: var(--stage-pad-fit, var(--pad));"));
        assert!(mobile.contains(".stage-actions:not([hidden]) {\n    position: static;"));
        assert!(mobile.contains("body.controls-visible .stage-actions:not([hidden])"));
        assert!(mobile.contains("display: flex;"));
        assert!(mobile.contains("flex-wrap: wrap;"));
        assert!(mobile.contains("overflow-x: hidden;"));
        assert!(mobile.contains(".stage-actions .feed-action {\n    flex: 1 1 calc(50% - 4px);"));
    }

    #[test]
    fn history_uses_browsing_scale_and_continuous_scroll() {
        let html = render_index_with_config(Some("remote"), &config(true));

        assert!(html.contains("--history-headline: clamp(30px, 4.7vw, 72px);"));
        assert!(html.contains("--history-headline-leading: 1.18;"));
        assert!(html.contains("--history-deck: clamp(17px, 2vw, 28px);"));
        assert!(html.contains("--history-deck-leading: 1.32;"));
        assert!(html.contains("overscroll-behavior-y: contain;"));
        assert!(html.contains("-webkit-overflow-scrolling: touch;"));
        assert!(html.contains("scroll-behavior: auto;"));
        assert!(html.contains("min-height: auto;"));
        assert!(html.contains("padding: clamp(40px, 8vh, 96px) var(--pad);"));
        assert!(html.contains(".timeline-card + .timeline-card"));
        assert!(html.contains("font-size: var(--history-headline);"));
        assert!(html.contains("line-height: var(--history-headline-leading);"));
        assert!(html.contains("font-size: var(--history-deck);"));
        assert!(html.contains("line-height: var(--history-deck-leading);"));
        assert!(!html.contains(".timeline-card h2 {\n  width: 100%;\n  min-width: 0;\n  max-width: min(100%, 28ch);\n  margin: 0;\n  color: var(--fg);\n  font-size: var(--headline);"));
        assert!(!html.contains("scroll-snap-type"));
        assert!(!html.contains("scroll-snap-align"));
        assert!(!html.contains("scroll-snap-stop"));
        assert!(!html.contains("content-visibility: auto;"));
    }

    #[test]
    fn stage_typography_fits_overflowing_desktop_headlines() {
        let html = render_index_with_config(Some("stage"), &config(false));

        assert!(html.contains("let stageFitFrame = undefined;"));
        assert!(html.contains("function scheduleStageFit()"));
        assert!(html.contains("function fitStageTypography()"));
        assert!(html.contains("function stageOverflows()"));
        assert!(html.contains("const STAGE_FIT_VARIABLES = ["));
        assert!(html.contains("function clearStageFit()"));
        assert!(html.contains("height: 100dvh;"));
        assert!(
            html.contains(
                "grid-template-rows: minmax(56px, 8vh) minmax(0, 1fr) minmax(48px, 8vh);"
            )
        );
        assert!(html.contains(".reel {\n  width: 100vw;"));
        assert!(html.contains("height: 100vh;"));
        assert!(html.contains("overflow: hidden;"));
        assert!(html.contains("stage.scrollHeight > stage.clientHeight + 1"));
        assert!(html.contains("headline.scrollWidth > headline.clientWidth + 1"));
        assert!(html.contains("headline.scrollHeight > headline.clientHeight + 1"));
        assert!(html.contains("deck.scrollHeight > deck.clientHeight + 1"));
        assert!(html.contains("stage.style.setProperty(\"--headline-fit\""));
        assert!(html.contains("stage.style.setProperty(\"--deck-fit\""));
        assert!(html.contains("stage.style.setProperty(\"--stage-gap-fit\""));
        assert!(html.contains("stage.style.setProperty(\"--stage-pad-fit\""));
        assert!(html.contains("stage.style.setProperty(\"--headline-max-fit\""));
        assert!(html.contains("function stageMinHeadlinePx()"));
        assert!(
            html.contains("return window.matchMedia(\"(max-width: 720px)\").matches ? 16 : 24;")
        );
        assert!(html.contains("function stageMinGapPx()"));
        assert!(html.contains("function stageMinPadPx()"));
        assert!(html.contains("headlineImageImg.onload = scheduleStageFit;"));
        assert!(html.contains("window.addEventListener(\"resize\", scheduleStageFit);"));
    }

    #[test]
    fn remote_states_stop_dwell_progress() {
        let html = render_index_with_config(Some("remote"), &config(true));
        let remote_state = html
            .split("function renderRemoteState")
            .nth(1)
            .expect("remote state renderer is embedded")
            .split("function routeEyebrow")
            .next()
            .expect("remote state renderer has a following function");

        assert!(remote_state.contains("stopStageProgress();"));
        assert!(!remote_state.contains("restartStageProgress"));
    }

    #[test]
    fn remote_user_route_shows_identity_once() {
        let html = render_index_with_config(Some("remote"), &config(true));

        assert!(html.contains("function routeStreamLabel"));
        assert!(html.contains("function routeEyebrow"));
        assert!(html.contains("function remoteHeadlineForState"));
        assert!(html.contains("setText(eyebrow, routeEyebrow(route));"));
        assert!(html.contains("setText(headline, remoteHeadlineForState(state));"));
        assert!(html
            .contains("renderPublisher(nextPublisher || (route.kind === \"global\" ? undefined : { login: route.login }));"));
        assert!(!html.contains("`@${route.login} / ${route.selection} / ${route.feedMode}`"));
        assert!(!html.contains("setText(headline, `@${route.login}`);"));
    }

    #[test]
    fn history_uses_feed_labels_without_repeating_login() {
        let html = render_index_with_config(Some("remote"), &config(true));

        assert!(html.contains("feedLink(route.login, \"*\", \"all feeds\""));
        assert!(html.contains("feed.publisher_avatar ||"));
        assert!(html.contains("meta.textContent = feedLabel;"));
        assert!(html.contains("function timelineMetaText"));
        assert!(html.contains("function primaryProjectTag"));
        assert!(html.contains("history · ${routeStreamLabel(route)}"));
        assert!(!html.contains("feedLink(route.login, \"*\", `${route.login}/*`"));
        assert!(
            !html.contains("meta.textContent = `${publisherText(feed, ticket)} / ${feedLabel}`;")
        );
    }

    #[test]
    fn discovery_publishers_link_to_user_discovery_feed() {
        let html = render_index_with_config(Some("remote"), &config(true));

        assert!(html.contains("function userDiscoveryUrl"));
        assert!(html.contains("return `/${encodeURIComponent(login)}/*"));
        assert!(html.contains("publisher.dataset.href = profileUrl;"));
        assert!(html.contains("open @${login} discovery feed"));
        assert!(html.contains("timelinePublisher(item, { profile: {} }, route)"));
        assert!(
            html.contains("const node = document.createElement(profileUrl ? \"a\" : \"div\");")
        );
        assert!(html.contains(".publisher[data-href]"));
    }

    #[test]
    fn browser_user_route_filters_headlines_without_throwing() {
        let html = render_index_with_config(Some("remote"), &config(true));

        assert!(html.contains("function headlineMatchesRoute"));
        assert!(html.contains("const requestedFeeds = requestedFeedLabels(route);"));
        assert!(html.contains("function headlineMatchesReelFilters"));
        assert!(html.contains(
            "created_at: item.created_at || item.createdAt || item.published_at || item.publishedAt"
        ));
        assert!(html.contains("requestedCsvParams(params, [\"projects\", \"project\"])"));
        assert!(html.contains("function headlineTagTerms"));
        assert!(html.contains("function copyReelFilterParams"));
        assert!(html.contains("function requestedFeedLabels"));
        assert!(html.contains("function headlineFeedLabels"));
        assert!(html.contains("clean === `${String(login || \"\").replace(/^@/, \"\")}/*`"));
        assert!(html.contains(".filter((item) => headlineMatchesRoute(item, route))"));
        assert!(html.contains("params.set(\"streams\", \"all\");"));
    }

    #[test]
    fn browser_network_source_count_uses_feed_identity_not_chips() {
        let html = render_index_with_config(Some("remote"), &config(true));

        assert!(html.contains(r#"<span id="source-count">0 feeds</span>"#));
        assert!(html.contains("let remoteFeedCount = undefined;"));
        assert!(html.contains("function updateSourceCountFromFeeds(feeds)"));
        assert!(html.contains("setText(sourceCount, feedCountLabel(remoteFeedCount));"));
        assert!(html.contains("if (remoteRoute && remoteFeedCount !== undefined)"));
        assert!(html.contains("source_key:"));
        assert!(html.contains("function bulletinSourceKey"));
        assert!(html.contains("bulletin.feed_id || bulletin.feedId"));
        assert!(html.contains("bulletin.lower_third || bulletin.lowerThird"));
        assert!(!html.contains("0 stories"));
        assert!(!html.contains("const firstChip = bulletin.chips?.[0];\n    const label"));
    }

    #[test]
    fn browser_console_logs_feed_lifecycle_events() {
        let html = render_index_with_config(Some("remote"), &config(true));

        assert!(html.contains("function logEvent"));
        assert!(html.contains("feed.remote.route.start"));
        assert!(html.contains("feed.resolver.response"));
        assert!(html.contains("feed.network.discovery.snapshot"));
        assert!(html.contains("feed.sse.bulletin.incoming"));
    }

    #[test]
    fn browser_remote_route_reports_version_mismatch() {
        let html = render_index_with_config(Some("remote"), &config(true));

        assert!(html.contains("const FEED_PROTOCOL_VERSION = 1;"));
        assert!(html.contains("window.FEED_COMPATIBILITY = {"));
        assert!(html.contains("\"model_version\":3"));
        assert!(html.contains("const FEED_MODEL_VERSION = Number"));
        assert!(html.contains("function compatibilityStatus"));
        assert!(html.contains("function networkCompatibilityStatus"));
        assert!(html.contains("version-mismatch"));
        assert!(html.contains("update your peer to the latest version"));
        assert!(html.contains("feed.user.incompatible_feeds_ignored"));
        assert!(html.contains("feed.network.discovery.network_mismatch"));
    }

    #[test]
    fn root_page_supports_global_network_discovery() {
        let html = render_index_with_config(Some("remote"), &config(true));

        assert!(html.contains("function parseGlobalRoute"));
        assert!(html.contains("function startGlobalDiscoveryRoute"));
        assert!(html.contains("/network/snapshot"));
        assert!(html.contains("function classifyRemoteSnapshot"));
        assert!(html.contains("function renderQuietConnected"));
        assert!(html.contains("quiet right now"));
        assert!(html.contains("new settled stories will appear live"));
        assert!(
            html.contains(
                "renderPublisher(nextPublisher || (route.kind === \"global\" ? undefined"
            )
        );
        assert!(html.contains("function timelineModeLink"));
        assert!(html.contains("body[data-view=\"history\"] .mode-switcher"));
        assert!(html.contains("body[data-view=\"timeline\"] .mode-switcher"));
        assert!(html.contains(".timeline-toolbar > span"));
        assert!(html.contains("nav.appendChild(timelineModeLink(route, \"discovery\""));
        assert!(html.contains("params.set(\"feed_mode\", mode);"));
        assert!(html.contains("per-user discovery is represented by user/* wildcard routes"));
    }

    #[test]
    fn following_mode_uses_local_follow_list_and_resolves_targets() {
        let html = render_index_with_config(Some("remote"), &config(true));

        assert!(html.contains("function startFollowingRoute"));
        assert!(html.contains("const FOLLOWING_STORAGE_KEY_V2 = \"feed.following.v2\";"));
        assert!(html.contains("function storedFollowEntries"));
        assert!(html.contains("function normalizeFollowEntry"));
        assert!(html.contains("window.localStorage.getItem(FOLLOWING_STORAGE_KEY_V2)"));
        assert!(html.contains("window.localStorage.setItem(FOLLOWING_STORAGE_KEY_V2"));
        assert!(html.contains("window.localStorage.setItem(\"feed.following\""));
        assert!(html.contains("function fetchFollowingTarget"));
        assert!(html.contains("function fetchFollowingTagTargets"));
        assert!(html.contains("function headlineMatchesFollowTarget"));
        assert!(html.contains("function renderFollowingTimeline"));
        assert!(html.contains("function toolbarFollowButton"));
        assert!(html.contains("inactive: `follow ${followTargetLabel(target)}`"));
        assert!(html.contains("button.dataset.kind = \"follow\";"));
        assert!(html.contains("button.dataset.followTarget = normalizeFollowTarget(target);"));
        assert!(html.contains("function refreshAfterFollowingChange"));
        assert!(html.contains("const follow = toolbarFollowButton(route);"));
        assert!(html.contains("feed.following.target.directory_fallback"));
        assert!(html.contains("copyReelFilterParams(route, params);"));
        assert!(html.contains("nothing followed yet"));
        assert!(!html.contains("no subscriptions selected"));
    }

    #[test]
    fn history_exposes_project_filter_actions() {
        let html = render_index_with_config(Some("remote"), &config(true));

        assert!(html.contains("function projectFilterLink"));
        assert!(html.contains("function projectFilterUrl"));
        assert!(html.contains("function tagFollowButton"));
        assert!(html.contains("params.set(\"projects\", project);"));
        assert!(html.contains("filter history to project ${project}"));
        assert!(html.contains("params.set(\"view\", \"history\");"));
        assert!(html.contains("actions.appendChild(projectFilterLink(project, route));"));
        assert!(html.contains("actions.appendChild(tagFollowButton(project));"));
    }

    #[test]
    fn discovery_stage_has_follow_flow_without_default_ticker_copy() {
        let html = render_index_with_config(Some("remote"), &config(true));

        assert!(html.contains("id=\"stage-actions\" hidden"));
        assert!(html.contains("function renderStageActions"));
        assert!(html.contains("function followTargetForBulletin"));
        assert!(html.contains("function personFollowTargetForBulletin"));
        assert!(html.contains(
            "const primaryTarget = primaryFollowTargetForTargets(personTarget, feedTarget);"
        ));
        assert!(html.contains("followButton(primaryTarget"));
        assert!(html.contains("following.textContent = \"open following\";"));
        assert!(html.contains("stageActions.appendChild(historyAction(remoteRoute));"));
        assert!(html.contains("link.textContent = \"history\";"));
        assert!(html.contains("body.controls-visible .stage-actions"));
        assert!(html.contains("button.setAttribute(\"aria-label\", `${active ? \"unfollow\" : \"follow\"} ${followTargetLabel(target)}`);"));
        assert!(!html.contains("inactive: \"follow feed\""));
        assert!(!html.contains("activity is reduced before display"));
    }

    #[test]
    fn mode_switcher_preserves_user_scope_without_loopback_link() {
        let html = render_index_with_config(Some("remote"), &config(true));

        assert!(!html.contains("id=\"mode-local\""));
        assert!(!html.contains("http://127.0.0.1:7777/reel"));
        assert!(html.contains(
            "modeDiscovery.textContent = route.kind === \"global\" ? \"discover\" : route.kind === \"org\" ? \"org feeds\" : \"feeds\";"
        ));
        assert!(html.contains("modeHistory.textContent = \"history\";"));
        assert!(html.contains("modeHistory.setAttribute(\"href\", historyUrl(route));"));
        assert!(html.contains("function routeHistoryRequested"));
        assert!(html.contains("view === \"history\""));
        assert!(html.contains("params.set(\"view\", \"history\");"));
        assert!(
            html.contains("window.addEventListener(\"pointerdown\", reveal, { passive: true });")
        );
        assert!(
            html.contains("window.addEventListener(\"touchstart\", reveal, { passive: true });")
        );
        assert!(html.contains("params.set(\"all\", \"true\");"));
        assert!(!html.contains("params.set(\"following\", route.followingTargets.join(\",\"));"));
        assert!(
            html.contains("[\"discovery\", \"discover\", \"hero\", \"public\"].includes(explicit)")
        );
        assert!(html.contains("hosted feed pages do not link to loopback reels"));
        assert!(html.contains("modeFollowing.textContent = route.kind === \"user\" ? `following @${route.login}` : \"following\";"));
        assert!(html.contains("timelineModeLink(route, \"following\", \"following\""));
        assert!(html.contains("return `${routePath(route)}${query ? `?${query}` : \"\"}`;"));
        assert!(html.contains("return `${login}/*`;"));
        assert!(html.contains("link.dataset.kind = \"mode\";"));
        assert!(html.contains("link.dataset.kind = \"feed\";"));
        assert!(html.contains("function fetchDirectoryTicketForUser"));
        assert!(html.contains("feed.resolver.directory_fallback"));
        assert!(html.contains("/network/snapshot${directoryFallbackQuery(route)}"));
        assert!(html.contains(".timeline-feeds a[data-kind=\"mode\"]"));
        assert!(html.contains(".timeline-feeds .feed-action[data-kind=\"follow\"]"));
    }

    #[test]
    fn browser_supports_private_org_feed_routes() {
        let html = render_index_with_config(Some("remote"), &config(true));

        assert!(html.contains("pathSegments[0] === \"org\""));
        assert!(html.contains("/resolve/github-org/${encodeURIComponent(org)}"));
        assert!(html.contains("private org feeds need github org authorization"));
        assert!(html.contains("params.set(\"scope\", \"read:user read:org\");"));
        assert!(html.contains("function startOrgRoute"));
        assert!(html.contains("route.kind === \"org\" ? `sign in for ${route.org || route.login}` : \"sign in with github\""));
        assert!(html.contains("true,\n  );"));
    }

    #[test]
    fn github_callback_handles_fragment_and_raw_oauth_forward() {
        let html = render_index_with_config(Some("remote"), &config(true));

        assert!(html.contains(
            "const forwardedGithubOauthCallback = forwardRawGithubOauthCallback(window.location);"
        ));
        assert!(html.contains("function githubCallbackParams(location)"));
        assert!(html.contains("return new URLSearchParams(hash || query);"));
        assert!(html.contains("function forwardRawGithubOauthCallback(location)"));
        assert!(html.contains("!params.has(\"code\") || !params.has(\"state\")"));
        assert!(html.contains("window.location.replace(target);"));
        assert!(html.contains("feed.github.callback.forward_edge"));
    }

    #[test]
    fn following_is_person_first_with_feed_and_tag_refinements() {
        let html = render_index_with_config(Some("remote"), &config(true));

        assert!(html.contains("return feed === \"*\""));
        assert!(html.contains("return `${clean.login}/*`;"));
        assert!(html.contains("return `${clean.login}/${clean.feed}`;"));
        assert!(html.contains("return tag ? { kind: \"tag\""));
        assert!(html.contains("return feed === \"*\" ? `@${login}` : `@${login}/${feed}`;"));
        assert!(html.contains(
            "const streamTargets = targets.filter((target) => !isTagFollowTarget(target));"
        ));
        assert!(html.contains("const tagTargets = targets.filter(isTagFollowTarget);"));
        assert!(html.contains("followTargetMatchesLogin(target, login)"));
        assert!(html.contains("function primaryFollowTargetForTargets"));
        assert!(html.contains(
            "const primaryTarget = primaryFollowTargetForTargets(personTarget, target, route);"
        ));
        assert!(html.contains("followButton(primaryTarget"));
        assert!(html.contains("privateFeedPill(visibility)"));
        assert!(!html.contains("following feed"));
        assert!(!html.contains("request access"));
        assert!(!html.contains("access pending"));
    }

    #[test]
    fn browser_refreshes_local_and_remote_snapshots_without_page_reload() {
        let html = render_index_with_config(Some("remote"), &config(true));

        assert!(html.contains("const LOCAL_SNAPSHOT_REFRESH_MS = 5000;"));
        assert!(html.contains("const REMOTE_SNAPSHOT_REFRESH_MS = 5000;"));
        assert!(html.contains("function scheduleRemoteRefresh"));
        assert!(html.contains("function refreshRemoteRoute"));
        assert!(html.contains("await startGlobalDiscoveryRoute(route, true);"));
        assert!(html.contains("await startUserRoute(route, true);"));
        assert!(html.contains("await startFollowingRoute(route, true);"));
        assert!(html.contains("window.setInterval(hydrate, LOCAL_SNAPSHOT_REFRESH_MS);"));
        assert!(html.contains("feed.network.discovery.headlines.unchanged"));
        assert!(html.contains("feed.network.discovery.classified"));
        assert!(html.contains("feed.user.classified"));
        assert!(html.contains("feed.following.classified"));
    }

    #[test]
    fn browser_queues_new_stories_instead_of_immediate_switching() {
        let html = render_index_with_config(Some("remote"), &config(true));

        assert!(html.contains("const MAX_STAGE_BULLETINS = 12;"));
        assert!(html.contains("const MAX_SEEN_BULLETINS = 512;"));
        assert!(html.contains("const STAGE_HEADLINE_MAX_AGE_MS = 30 * 60 * 1000;"));
        assert!(html.contains("const LATEST_SEEN_HEADLINE_HOLD_MS = 15 * 60 * 1000;"));
        assert!(html.contains("const SEEN_BULLETIN_STORAGE_KEY = \"feed.seenBulletins.v1\";"));
        assert!(html.contains("const MIN_QUEUED_ADVANCE_MS = 2500;"));
        assert!(html.contains("function applyBulletinQueueUpdate"));
        assert!(html.contains("function queueIncomingBulletin"));
        assert!(html.contains("function completeActiveBulletin"));
        assert!(html.contains("function markBulletinSeen"));
        assert!(html.contains("function renderLatestSeenStory"));
        assert!(html.contains("function bulletinIsRecentForLatestHold"));
        assert!(html.contains("setText(liveState, \"latest\");"));
        assert!(html.contains("feed.remote.latest_seen"));
        assert!(html.contains("latestBulletin,"));
        assert!(html.contains("function shouldInterruptBulletin"));
        assert!(html.contains(
            "priority >= 95 || (priority >= 90 && [\"breaking\", \"incident\"].includes(mode))"
        ));
        assert!(html.contains("feed.bulletin.queued"));
        assert!(html.contains("feed.bulletin.queue.advance_scheduled"));
        assert!(html.contains("feed.bulletin.queue.drained"));
        assert!(html.contains("renderQuietConnected(remoteRoute"));
        assert!(html.contains("completeActiveBulletin(\"dwell\");"));
        assert!(html.contains("queueIncomingBulletin(bulletin, \"sse\");"));
        assert!(!html.contains("activeIndex = (activeIndex + 1) % bulletins.length;"));
        assert!(
            !html.contains("activeIndex = bulletins.length - 1;\n      renderBulletin(bulletin);")
        );
    }

    #[test]
    fn local_status_renders_capture_watchers_without_fake_story() {
        let html = render_index_with_config(Some("stage"), &config(false));

        assert!(html.contains("capture_watchers: status.capture_watchers?.length || 0"));
        assert!(html.contains("function renderCaptureWatchStatus"));
        assert!(html.contains("function latestCaptureWatcher"));
        assert!(html.contains("watching agent sessions"));
        assert!(html.contains("agent activity received"));
        assert!(html.contains("feed.capture.watch.render"));
        assert!(html.contains("renderChips([\"watching\", \"story-gated\", \"redacted\"]);"));
        assert!(!html.contains("0 captures"));
        assert!(!html.contains("raw events unavailable"));
    }
}
