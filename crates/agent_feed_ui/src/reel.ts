const reel = document.querySelector(".reel");
const stage = document.querySelector("#stage");
const eyebrow = document.querySelector("#eyebrow");
const headline = document.querySelector("#headline");
const deck = document.querySelector("#deck");
const chips = document.querySelector("#chips");
const ticker = document.querySelector("#ticker");
const clock = document.querySelector("#clock");
const sourceCount = document.querySelector("#source-count");
const liveState = document.querySelector("#live-state");
const publisher = document.querySelector("#publisher");
const publisherAvatar = document.querySelector("#publisher-avatar");
const publisherLabel = document.querySelector("#publisher-label");
const authAction = document.querySelector("#auth-action");
const stageActions = document.querySelector("#stage-actions");
const headlineImage = document.querySelector("#headline-image");
const headlineImageImg = document.querySelector("#headline-image-img");
const stageProgress = document.querySelector("#stage-progress");
const storyTime = document.querySelector("#story-time");
const timeline = document.querySelector("#timeline");
const modeSwitcher = document.querySelector("#mode-switcher");
const modeDiscovery = document.querySelector("#mode-discovery");
const modeFollowing = document.querySelector("#mode-following");
const modeHistory = document.querySelector("#mode-history");
const footerRev = document.querySelector("#footer-rev");

let bulletins = [];
let activeIndex = 0;
let dwellTimer = undefined;
let controlsTimer = undefined;
let remoteRefreshTimer = undefined;
let remoteRefreshInFlight = false;
let remoteHeadlinesSignature = "";
let remoteFeedCount = undefined;
let localSnapshotSignature = "";
let stageFitFrame = undefined;
let activeStartedAt = 0;
let activeDwellMs = 14000;

const MAX_STAGE_BULLETINS = 12;
const MAX_SEEN_BULLETINS = 512;
const STAGE_HEADLINE_MAX_AGE_MS = 30 * 60 * 1000;
const LATEST_SEEN_HEADLINE_HOLD_MS = 15 * 60 * 1000;
const MIN_QUEUED_ADVANCE_MS = 2500;
const LOCAL_SNAPSHOT_REFRESH_MS = 5000;
const REMOTE_SNAPSHOT_REFRESH_MS = 5000;
const SEEN_BULLETIN_STORAGE_KEY = "feed.seenBulletins.v1";
const FOLLOWING_STORAGE_KEY_V2 = "feed.following.v2";
const FOLLOWING_LEGACY_STORAGE_KEYS = [
  "feed.following",
  "feed.subscriptions",
  "agent_feed.subscriptions",
];
const seenBulletinIds = loadSeenBulletinIds();
const STAGE_FIT_VARIABLES = [
  "--headline-fit",
  "--deck-fit",
  "--stage-gap-fit",
  "--stage-pad-fit",
  "--headline-max-fit",
  "--deck-max-fit",
];

const FEED_COMPATIBILITY = window.FEED_COMPATIBILITY || {};
const FEED_PROTOCOL_VERSION = 1;
const FEED_MODEL_VERSION = Number(FEED_COMPATIBILITY.model_version ?? FEED_COMPATIBILITY.modelVersion ?? 3);
const FEED_MIN_MODEL_VERSION = Number(FEED_COMPATIBILITY.min_model_version ?? FEED_COMPATIBILITY.minModelVersion ?? 3);

const forwardedGithubOauthCallback = forwardRawGithubOauthCallback(window.location);
const githubAuthCallback = forwardedGithubOauthCallback ? undefined : parseGithubAuthCallback(window.location);
const remoteRoute = githubAuthCallback ? undefined : parseRemoteRoute(window.location);

if (footerRev) {
  const rev = String(window.FEED_BUILD_REV || "dev").slice(0, 12) || "dev";
  footerRev.textContent = `rev ${rev}`;
}

if (publisher) {
  publisher.addEventListener("click", (event) => {
    const href = publisher.dataset.href;
    if (!href) {
      return;
    }
    event.preventDefault();
    logInfo("feed.publisher.navigate", { href });
    window.location.assign(href);
  });
  publisher.addEventListener("keydown", (event) => {
    if (!publisher.dataset.href || !["Enter", " "].includes(event.key)) {
      return;
    }
    event.preventDefault();
    publisher.click();
  });
}

function logEvent(level, event, detail = {}) {
  const payload = {
    event,
    ts: new Date().toISOString(),
    ...normalizeLogDetail(detail),
  };
  const logger = console[level] || console.info;
  logger.call(console, `[feed] ${event}`, payload);
}

function normalizeLogDetail(detail) {
  if (!detail) {
    return {};
  }
  if (detail instanceof Error) {
    return {
      error_name: detail.name,
      error_message: detail.message,
      error_stack: detail.stack,
    };
  }
  if (detail instanceof Event) {
    const target = detail.target || {};
    return {
      event_type: detail.type,
      target_type: target.constructor?.name,
      ready_state: target.readyState,
    };
  }
  if (typeof detail === "object") {
    return detail;
  }
  return { value: String(detail) };
}

function logDebug(event, detail) {
  logEvent("debug", event, detail);
}

function logInfo(event, detail) {
  logEvent("info", event, detail);
}

function logError(context, error) {
  logEvent("error", context, error);
}

function logWarn(context, detail) {
  logEvent("warn", context, detail);
}

async function responseErrorMessage(response, fallback) {
  try {
    const body = await response.clone().json();
    return body?.error || body?.message || fallback;
  } catch (_error) {
    return fallback;
  }
}

function setText(node, value) {
  if (node) {
    node.textContent = value || "";
    if (node === headline || node === deck) {
      scheduleStageFit();
    }
  }
}

function scheduleStageFit() {
  if (!stage || !headline || !deck) {
    return;
  }
  if (stageFitFrame !== undefined) {
    window.cancelAnimationFrame(stageFitFrame);
  }
  stageFitFrame = window.requestAnimationFrame(() => {
    stageFitFrame = window.requestAnimationFrame(() => {
      stageFitFrame = undefined;
      fitStageTypography();
    });
  });
}

function fitStageTypography() {
  if (!stage || !headline || !deck || stage.hidden) {
    return;
  }
  clearStageFit();
  const baseHeadline = pixelSize(headline);
  const baseDeck = pixelSize(deck);
  const baseGap = stageGapPx();
  const basePad = stagePadPx();
  let nextHeadline = baseHeadline;
  let nextDeck = baseDeck;
  let nextGap = baseGap;
  let nextPad = basePad;
  let adjusted = false;
  const mobile = window.matchMedia("(max-width: 720px)").matches;

  for (let attempt = 0; attempt < 28 && stageOverflows(); attempt += 1) {
    adjusted = true;
    const pressure = stageOverflowPressure();
    nextHeadline = Math.max(stageMinHeadlinePx(), nextHeadline * (pressure ? 0.88 : 0.94));
    nextDeck = Math.max(stageMinDeckPx(), nextDeck * (pressure ? 0.92 : 0.96));
    nextGap = Math.max(stageMinGapPx(), nextGap * 0.9);
    nextPad = Math.max(stageMinPadPx(), nextPad * 0.93);
    stage.style.setProperty("--headline-fit", `${roundPixel(nextHeadline)}px`);
    stage.style.setProperty("--deck-fit", `${roundPixel(nextDeck)}px`);
    stage.style.setProperty("--stage-gap-fit", `${roundPixel(nextGap)}px`);
    stage.style.setProperty("--stage-pad-fit", `${roundPixel(nextPad)}px`);
    if (!mobile) {
      stage.style.setProperty("--headline-max-fit", `min(100%, ${Math.min(72, 28 + attempt * 2)}ch)`);
      stage.style.setProperty("--deck-max-fit", `min(100%, ${Math.min(96, 38 + attempt * 2)}ch)`);
    }
  }

  if (adjusted) {
    logDebug("feed.stage.typography.fit", {
      headline_px: roundPixel(nextHeadline),
      deck_px: roundPixel(nextDeck),
      gap_px: roundPixel(nextGap),
      pad_px: roundPixel(nextPad),
      stage_scroll_height: stage.scrollHeight,
      stage_client_height: stage.clientHeight,
      headline_scroll_width: headline.scrollWidth,
      headline_client_width: headline.clientWidth,
      headline_scroll_height: headline.scrollHeight,
      headline_client_height: headline.clientHeight,
      deck_scroll_height: deck.scrollHeight,
      deck_client_height: deck.clientHeight,
      overflow_remaining: stageOverflows(),
    });
  }
}

function clearStageFit() {
  if (!stage) {
    return;
  }
  for (const name of STAGE_FIT_VARIABLES) {
    stage.style.removeProperty(name);
  }
}

function stageOverflows() {
  if (!stage || !headline || !deck) {
    return false;
  }
  return (
    (reel && reel.scrollHeight > reel.clientHeight + 1) ||
    stage.scrollHeight > stage.clientHeight + 1 ||
    headline.scrollWidth > headline.clientWidth + 1 ||
    deck.scrollWidth > deck.clientWidth + 1 ||
    headline.scrollHeight > headline.clientHeight + 1 ||
    deck.scrollHeight > deck.clientHeight + 1
  );
}

function stageOverflowPressure() {
  if (!stage || !headline || !deck) {
    return false;
  }
  return (
    stage.scrollHeight > stage.clientHeight + 1 ||
    headline.scrollHeight > headline.clientHeight + 1 ||
    deck.scrollHeight > deck.clientHeight + 1
  );
}

function pixelSize(node) {
  const value = Number.parseFloat(window.getComputedStyle(node).fontSize);
  return Number.isFinite(value) ? value : 16;
}

function stageGapPx() {
  const value = Number.parseFloat(window.getComputedStyle(stage).rowGap);
  return Number.isFinite(value) ? value : 18;
}

function stagePadPx() {
  const value = Number.parseFloat(window.getComputedStyle(stage).paddingTop);
  return Number.isFinite(value) ? value : 28;
}

function stageMinHeadlinePx() {
  return window.matchMedia("(max-width: 720px)").matches ? 16 : 24;
}

function stageMinDeckPx() {
  return window.matchMedia("(max-width: 720px)").matches ? 12 : 15;
}

function stageMinGapPx() {
  return window.matchMedia("(max-width: 720px)").matches ? 6 : 8;
}

function stageMinPadPx() {
  return window.matchMedia("(max-width: 720px)").matches ? 16 : 22;
}

function roundPixel(value) {
  return Math.round(value * 10) / 10;
}

function setAuthAction(url, label = "sign in with github", force = false) {
  if (!authAction) {
    return;
  }
  if (!url || (!force && !isNetworkView())) {
    authAction.hidden = true;
    authAction.removeAttribute("href");
    return;
  }
  authAction.textContent = label;
  authAction.setAttribute("href", url);
  authAction.hidden = false;
}

function clearAuthAction() {
  setAuthAction("");
}

function clearStageActions() {
  if (!stageActions) {
    return;
  }
  stageActions.replaceChildren();
  stageActions.hidden = true;
}

function renderStageActions(bulletin) {
  if (!stageActions) {
    return;
  }
  stageActions.replaceChildren();
  const personTarget = personFollowTargetForBulletin(bulletin);
  const feedTarget = followTargetForBulletin(bulletin);
  if (!p2pEnabled()) {
    stageActions.hidden = true;
    return;
  }
  let added = false;
  const primaryTarget = primaryFollowTargetForTargets(personTarget, feedTarget);
  if (primaryTarget) {
    stageActions.appendChild(
      followButton(primaryTarget, {
        inactive: `follow ${followTargetLabel(primaryTarget)}`,
        active: `following ${followTargetLabel(primaryTarget)}`,
      }),
    );
    added = true;
  }
  if (primaryTarget) {
    const following = document.createElement("a");
    following.className = "feed-action";
    following.href = followingTargetUrl(primaryTarget);
    following.textContent = "open following";
    following.setAttribute("aria-label", `open followed feed ${followTargetLabel(primaryTarget)}`);
    stageActions.appendChild(following);
    added = true;
  }
  if (remoteRoute) {
    stageActions.appendChild(historyAction(remoteRoute));
    added = true;
  }
  stageActions.hidden = !added;
}

function showStage() {
  if (stage) {
    stage.hidden = false;
  }
  if (timeline) {
    timeline.hidden = true;
  }
}

function restartStageProgress(dwellMs = 14000) {
  if (!stageProgress) {
    return;
  }
  const duration = normalizeDwellMs(dwellMs);
  stageProgress.hidden = false;
  stageProgress.style.setProperty("--dwell", `${duration}ms`);
  stageProgress.classList.remove("is-running");
  void stageProgress.offsetWidth;
  stageProgress.classList.add("is-running");
}

function stopStageProgress() {
  window.clearTimeout(dwellTimer);
  dwellTimer = undefined;
  if (stageProgress) {
    stageProgress.classList.remove("is-running");
    stageProgress.style.removeProperty("--dwell");
    stageProgress.hidden = true;
  }
}

function bulletinTimestamp(bulletin) {
  return (
    bulletin?.created_at ||
    bulletin?.createdAt ||
    bulletin?.published_at ||
    bulletin?.publishedAt ||
    bulletin?.story_created_at ||
    bulletin?.storyCreatedAt ||
    bulletin?.capsule?.created_at ||
    bulletin?.capsule?.createdAt ||
    ""
  );
}

function renderStoryTime(bulletin) {
  if (!storyTime) {
    return;
  }
  const timestamp = bulletinTimestamp(bulletin);
  if (!timestamp) {
    clearStoryTime();
    return;
  }
  storyTime.dataset.timestamp = String(timestamp);
  storyTime.textContent = `posted ${relativeTime(timestamp)}`;
  storyTime.hidden = false;
}

function refreshStoryTime() {
  if (!storyTime || storyTime.hidden || !storyTime.dataset.timestamp) {
    return;
  }
  storyTime.textContent = `posted ${relativeTime(storyTime.dataset.timestamp)}`;
}

function clearStoryTime() {
  if (!storyTime) {
    return;
  }
  storyTime.hidden = true;
  storyTime.textContent = "";
  delete storyTime.dataset.timestamp;
}

function renderBulletin(bulletin) {
  if (!bulletin) {
    logWarn("render skipped empty bulletin");
    return;
  }
  const dwellMs = bulletinDwellMs(bulletin);

  logInfo("feed.bulletin.render", {
    bulletin_id: bulletin.id,
    mode: bulletin.mode,
    priority: bulletin.priority,
    dwell_ms: dwellMs,
    publisher: bulletin.publisher?.github_login || bulletin.feed_publisher?.github_login,
  });
  showStage();
  document.body.dataset.mode = bulletin.mode || "dispatch";
  stage?.classList.add("is-changing");
  window.setTimeout(() => {
    setText(eyebrow, bulletin.eyebrow);
    setText(headline, bulletin.headline);
    setText(deck, bulletin.deck);
    renderPublisher(bulletin.publisher || bulletin.feed_publisher);
    renderHeadlineImage(bulletin.image || bulletin.headline_image);
    renderStoryTime(bulletin);
    clearAuthAction();
    renderStageActions(bulletin);
    renderChips(bulletin.chips || []);
    renderTicker(bulletin.ticker || []);
    stage?.classList.remove("is-changing");
    activeStartedAt = Date.now();
    activeDwellMs = dwellMs;
    if (bulletins.length > 1) {
      restartStageProgress(dwellMs);
    } else {
      stopStageProgress();
    }
  }, 180);
}

function renderPublisher(nextPublisher) {
  if (!publisher || !publisherLabel || !publisherAvatar) {
    return;
  }
  if (!nextPublisher) {
    publisher.hidden = true;
    publisher.removeAttribute("data-href");
    publisher.removeAttribute("role");
    publisher.removeAttribute("tabindex");
    publisher.removeAttribute("aria-label");
    publisherAvatar.removeAttribute("src");
    setText(publisherLabel, "");
    return;
  }
  const login = nextPublisher.github_login || nextPublisher.login || nextPublisher.publisher_login;
  const label = login ? `@${login}` : nextPublisher.display_name || "verified peer";
  const avatar = safeAvatarUrl(
    nextPublisher.avatar || nextPublisher.publisher_avatar || nextPublisher.avatar_url,
  );
  setText(publisherLabel, label);
  if (avatar) {
    publisherAvatar.src = avatar;
  } else {
    publisherAvatar.removeAttribute("src");
  }
  const profileUrl = nextPublisher.profile_url || userDiscoveryUrl(login, remoteRoute);
  if (profileUrl) {
    publisher.dataset.href = profileUrl;
    publisher.setAttribute("role", "link");
    publisher.setAttribute("tabindex", "0");
    publisher.setAttribute("aria-label", `open @${login} discovery feed`);
  } else {
    publisher.removeAttribute("data-href");
    publisher.removeAttribute("role");
    publisher.removeAttribute("tabindex");
    publisher.removeAttribute("aria-label");
  }
  publisher.hidden = false;
}

function renderHeadlineImage(nextImage) {
  if (!headlineImage || !headlineImageImg) {
    return;
  }
  if (!nextImage) {
    headlineImage.hidden = true;
    headlineImageImg.removeAttribute("src");
    headlineImageImg.setAttribute("alt", "");
    scheduleStageFit();
    return;
  }
  if (!imagesEnabled()) {
    headlineImage.hidden = true;
    headlineImageImg.removeAttribute("src");
    headlineImageImg.setAttribute("alt", "");
    logWarn("headline image ignored because text-only mode is active", nextImage.source || "");
    scheduleStageFit();
    return;
  }
  const src = safeMediaUrl(nextImage.uri || nextImage.url || nextImage.src);
  if (!src) {
    headlineImage.hidden = true;
    headlineImageImg.removeAttribute("src");
    headlineImageImg.setAttribute("alt", "");
    scheduleStageFit();
    return;
  }
  headlineImageImg.onload = scheduleStageFit;
  headlineImageImg.src = src;
  headlineImageImg.setAttribute("alt", nextImage.alt || "feed generated headline image");
  headlineImage.hidden = false;
  scheduleStageFit();
}

function safeAvatarUrl(value) {
  if (!value || typeof value !== "string") {
    return "";
  }
  if (value.startsWith("/") || value.startsWith(window.location.origin)) {
    return value;
  }
  if (edgeBaseUrl() && value.startsWith(edgeBaseUrl())) {
    return value;
  }
  logWarn("ignored non-cached publisher avatar url", value);
  return "";
}

function safeMediaUrl(value) {
  if (!value || typeof value !== "string") {
    return "";
  }
  if (value.startsWith("/") || value.startsWith(window.location.origin)) {
    return value;
  }
  if (edgeBaseUrl() && value.startsWith(edgeBaseUrl())) {
    return value;
  }
  logWarn("ignored non-cached headline image url", value);
  return "";
}

function imagesEnabled() {
  const params = new URLSearchParams(window.location.search);
  if (
    params.get("text") === "only" ||
    params.get("text_only") === "true" ||
    params.get("images") === "off"
  ) {
    return false;
  }
  if (["1", "true", "on"].includes(params.get("images") || "")) {
    return true;
  }
  return (
    window.localStorage.getItem("feed.images") === "enabled" ||
    window.localStorage.getItem("agent_feed.images") === "enabled"
  );
}

function renderRemoteState(route, state, lines = [], nextPublisher = undefined) {
  logInfo("feed.remote.state", {
    state,
    scope: route.kind || "user",
    login: route.login,
    selection: route.selection,
    feed_mode: route.feedMode,
    network: route.network,
    lines,
  });
  showStage();
  document.body.dataset.mode = state === "failed" || state === "version-mismatch" ? "breaking" : "dispatch";
  setText(
    liveState,
    state === "live" ? "live" : state === "version-mismatch" ? "update" : "wait",
  );
  setText(eyebrow, routeEyebrow(route));
  setText(headline, remoteHeadlineForState(state));
  setText(deck, lines.join(" · "));
  renderPublisher(nextPublisher || (route.kind === "global" ? undefined : { login: route.login }));
  renderHeadlineImage(undefined);
  clearStoryTime();
  clearAuthAction();
  clearStageActions();
  renderChips([
    route.feedMode === "following"
      ? "following"
      : routeDisplayMode(route),
    route.network,
    state === "version-mismatch" ? "version" : "redacted",
  ]);
  renderTicker(lines);
  stopStageProgress();
}

function renderQuietConnected(route, classification = {}, nextPublisher = undefined) {
  updateSourceCountFromClassification(route, classification);
  if (renderLatestSeenStory(route, classification, nextPublisher)) {
    return;
  }
  const feedCount = Number(classification.feedCount ?? classification.feeds?.length ?? remoteFeedCount ?? 0);
  const detail = quietDetail(route, classification);
  logInfo("feed.remote.quiet", {
    reason: classification.kind || "quiet",
    scope: route.kind || "user",
    login: route.login,
    selection: route.selection,
    feed_mode: route.feedMode,
    network: route.network,
    feeds: feedCount,
    headlines: classification.headlineCount ?? classification.headlines?.length ?? 0,
    fresh_unseen: classification.freshUnseenCount ?? 0,
  });
  showStage();
  document.body.dataset.mode = "dispatch";
  setText(liveState, route.feedMode === "following" ? "follow" : "quiet");
  setText(eyebrow, routeEyebrow(route));
  setText(headline, "quiet right now");
  setText(deck, detail.join(" · "));
  renderPublisher(nextPublisher || classification.profile || (route.kind === "global" ? undefined : { login: route.login }));
  renderHeadlineImage(undefined);
  clearStoryTime();
  clearAuthAction();
  renderQuietStageActions(route, classification);
  renderChips([
    route.feedMode === "following" ? "following" : routeDisplayMode(route),
    feedCount > 0 ? feedCountLabel(feedCount) : route.network,
    "quiet",
  ]);
  renderTicker(detail);
  stopStageProgress();
}

function renderLatestSeenStory(route, classification = {}, nextPublisher = undefined) {
  updateSourceCountFromClassification(route, classification);
  const latest = classification.latestBulletin;
  if (!latest || !bulletinIsRecentForLatestHold(latest)) {
    return false;
  }
  logInfo("feed.remote.latest_seen", {
    reason: classification.kind || "latest",
    scope: route.kind || "user",
    login: route.login,
    selection: route.selection,
    feed_mode: route.feedMode,
    network: route.network,
    bulletin_id: bulletinId(latest),
    age_ms: Date.now() - bulletinCreatedAtMs(latest),
  });
  showStage();
  document.body.dataset.mode = latest.mode || "dispatch";
  setText(liveState, "latest");
  setText(eyebrow, latest.eyebrow || routeEyebrow(route));
  setText(headline, latest.headline);
  setText(deck, latest.deck);
  renderPublisher(
    latest.publisher ||
      latest.feed_publisher ||
      nextPublisher ||
      classification.profile ||
      (route.kind === "global" ? undefined : { login: route.login }),
  );
  renderHeadlineImage(latest.image || latest.headline_image);
  renderStoryTime(latest);
  clearAuthAction();
  renderStageActions(latest);
  renderChips(latest.chips || []);
  renderTicker(latest.ticker || []);
  stopStageProgress();
  activeStartedAt = 0;
  activeDwellMs = 0;
  return true;
}

function quietDetail(route, classification = {}) {
  const feedCount = Number(classification.feedCount ?? classification.feeds?.length ?? remoteFeedCount ?? 0);
  const parts = [];
  if (route.feedMode === "following") {
    parts.push(`following ${followTargetCountLabel(feedCount || route.followingTargets?.length || 0)}`);
  } else if (route.kind === "global") {
    parts.push(feedCount > 0 ? `${feedCountLabel(feedCount)} connected` : "network connected");
  } else if (route.kind === "org") {
    const stream = routeStreamLabel(route);
    parts.push(stream && stream !== "visible feeds" ? `${route.login} / ${stream}` : `${route.login} connected`);
  } else {
    const stream = routeStreamLabel(route);
    parts.push(stream && stream !== "visible feeds" ? `@${route.login} / ${stream}` : `@${route.login} connected`);
  }
  const lastStory = relativeTimeFromAny(classification.lastStoryAt);
  const lastSeen = relativeTimeFromAny(classification.lastSeenAt);
  if (lastStory) {
    parts.push(`last story ${lastStory}`);
  } else if (lastSeen) {
    parts.push(`last seen ${lastSeen}`);
  }
  parts.push("new settled stories will appear live");
  return uniqueTextParts(parts);
}

function renderQuietStageActions(route, classification = {}) {
  if (!stageActions || !p2pEnabled()) {
    clearStageActions();
    return;
  }
  stageActions.replaceChildren();
  if (classification.historyAvailable !== false) {
    stageActions.appendChild(historyAction(route));
  }
  stageActions.hidden = stageActions.childElementCount === 0;
}

function historyAction(route) {
  const link = document.createElement("a");
  link.className = "feed-action";
  link.href = historyUrl(route);
  link.textContent = "history";
  link.setAttribute("aria-label", "open feed history");
  return link;
}

function routeEyebrow(route) {
  const mode = routeDisplayMode(route);
  if (route.kind === "global") {
    return `feed / ${route.network} / ${mode === "discover" ? "public discovery" : mode}`;
  }
  if (route.kind === "org") {
    return `feed / ${route.network} / org / ${route.org || route.login} / ${mode}`;
  }
  return `feed / ${route.network} / @${route.login} / ${routeStreamLabel(route)}`;
}

function routeDisplayMode(route) {
  return route.feedMode === "discovery" ? "discover" : route.feedMode;
}

function routeStreamLabel(route) {
  if (route.kind === "global") {
    return route.feedMode === "following" ? "followed feeds" : "all public feeds";
  }
  if (route.kind === "org") {
    if (route.feed === "*" || route.feed === "all") {
      return "all org feeds";
    }
    return route.feed || "org feeds";
  }
  if (route.feed === "*") {
    return "all feeds";
  }
  if (route.feed) {
    return route.feed;
  }
  const selection = route.selection || "";
  const prefix = `${route.login}/`;
  if (selection === route.login) {
    return "visible feeds";
  }
  if (selection === `${route.login}/*`) {
    return "all feeds";
  }
  if (selection.startsWith(prefix)) {
    return selection.slice(prefix.length) || "visible feeds";
  }
  return selection && selection !== route.login ? selection : "visible feeds";
}

function remoteHeadlineForState(state) {
  switch (state) {
    case "resolving":
      return "finding feed";
    case "not-found":
      return "github user not found";
    case "auth-required":
      return "sign in required";
    case "no-feeds":
      return "no visible streams";
    case "waiting":
      return "quiet right now";
    case "version-mismatch":
      return "update your peer";
    case "live":
      return "live feed";
    case "failed":
      return "feed unavailable";
    default:
      return "quiet right now";
  }
}

function renderP2pDisabled(route) {
  logInfo("feed.p2p.disabled", {
    scope: route.kind || "user",
    login: route.login,
    selection: route.selection,
    feed_mode: route.feedMode,
    network: route.network,
  });
  showStage();
  hideModeSwitcher();
  document.body.dataset.mode = "dispatch";
  setText(liveState, "local");
  setText(eyebrow, "p2p disabled");
  setText(headline, "local feed only");
  setText(
    deck,
    "network discovery and following are unavailable because p2p is disabled.",
  );
  renderPublisher(undefined);
  renderHeadlineImage(undefined);
  clearStoryTime();
  clearAuthAction();
  clearStageActions();
  renderChips(["p2p off", "privacy on"]);
  renderTicker(["start with --p2p or use the hosted p2p browser shell"]);
  stopStageProgress();
}

function renderAuthRequired(route) {
  renderRemoteState(route, "auth-required", [
    "github sign-in required",
    route.kind === "org" ? "private org feeds need github org authorization" : "private feeds need a signed browser session",
    route.kind === "org" ? `authorize access to ${route.org || route.login}` : "sign in to continue",
  ]);
  setAuthAction(
    browserSignInUrl(window.location.href, route.kind === "org" ? route.org || route.login : undefined),
    route.kind === "org" ? `sign in for ${route.org || route.login}` : "sign in with github",
    true,
  );
}

function renderChips(nextChips) {
  if (!chips) {
    return;
  }
  chips.replaceChildren();
  const seen = new Set();
  nextChips
    .map((chip) => (typeof chip === "string" ? chip : chip.label))
    .filter(Boolean)
    .filter((label) => {
      const key = String(label).toLowerCase();
      if (isHarnessTag(key)) {
        return false;
      }
      if (seen.has(key)) {
        return false;
      }
      seen.add(key);
      return true;
    })
    .slice(0, 4)
    .forEach((label) => {
      const item = document.createElement("span");
      item.textContent = label;
      chips.appendChild(item);
    });
  scheduleStageFit();
}

function isHarnessTag(value) {
  const key = String(value || "")
    .trim()
    .toLowerCase()
    .replace(/[ .]/g, "_");
  return key === "codex" || key === "claude" || key.startsWith("codex_") || key.startsWith("claude_");
}

function renderTicker(items) {
  if (!ticker) {
    return;
  }
  ticker.replaceChildren();
  if (!items.length) {
    return;
  }
  const item = document.createElement("span");
  item.textContent = items.map((entry) => entry.text || entry).join(" · ");
  ticker.appendChild(item);
}

function normalizeDwellMs(dwellMs) {
  return Number.isFinite(Number(dwellMs))
    ? Math.max(Number(dwellMs), 1000)
    : 14000;
}

function bulletinDwellMs(bulletin) {
  return normalizeDwellMs(bulletin?.dwell_ms || bulletin?.dwellMs || 14000);
}

function bulletinId(bulletin) {
  return String(bulletin?.id || bulletin?.capsule_id || bulletin?.capsuleId || "");
}

function activeBulletin() {
  return bulletins[activeIndex];
}

function queueIds() {
  return new Set(bulletins.map(bulletinId).filter(Boolean));
}

function loadSeenBulletinIds() {
  try {
    const parsed = JSON.parse(window.localStorage.getItem(SEEN_BULLETIN_STORAGE_KEY) || "[]");
    if (!Array.isArray(parsed)) {
      return new Set();
    }
    return new Set(parsed.map(String).filter(Boolean).slice(-MAX_SEEN_BULLETINS));
  } catch (error) {
    logWarn("feed.seen.load_failed", error);
    return new Set();
  }
}

function persistSeenBulletinIds() {
  try {
    const ids = Array.from(seenBulletinIds).slice(-MAX_SEEN_BULLETINS);
    window.localStorage.setItem(SEEN_BULLETIN_STORAGE_KEY, JSON.stringify(ids));
  } catch (error) {
    logWarn("feed.seen.persist_failed", error);
  }
}

function markBulletinSeen(bulletin, source = "stage") {
  const id = bulletinId(bulletin);
  if (!id || seenBulletinIds.has(id)) {
    return;
  }
  seenBulletinIds.add(id);
  while (seenBulletinIds.size > MAX_SEEN_BULLETINS) {
    const oldest = seenBulletinIds.values().next().value;
    if (!oldest) {
      break;
    }
    seenBulletinIds.delete(oldest);
  }
  persistSeenBulletinIds();
  logDebug("feed.bulletin.seen", { source, bulletin_id: id, seen: seenBulletinIds.size });
}

function bulletinCreatedAtMs(bulletin) {
  const value = bulletin?.created_at || bulletin?.createdAt || bulletin?.published_at || bulletin?.publishedAt;
  if (!value) {
    return Date.now();
  }
  const timestamp = Date.parse(value);
  return Number.isFinite(timestamp) ? timestamp : Date.now();
}

function bulletinIsFreshForStage(bulletin) {
  const ageMs = Date.now() - bulletinCreatedAtMs(bulletin);
  return ageMs <= STAGE_HEADLINE_MAX_AGE_MS;
}

function bulletinIsRecentForLatestHold(bulletin) {
  const ageMs = Date.now() - bulletinCreatedAtMs(bulletin);
  return ageMs >= 0 && ageMs <= LATEST_SEEN_HEADLINE_HOLD_MS;
}

function stageCandidateBulletins(items, previousIds = new Set()) {
  return uniqueBulletins(items)
    .filter((item) => {
      const id = bulletinId(item);
      if (!id || previousIds.has(id)) {
        return true;
      }
      if (seenBulletinIds.has(id)) {
        return false;
      }
      return bulletinIsFreshForStage(item);
    })
    .slice()
    .reverse();
}

function uniqueBulletins(items) {
  const seen = new Set();
  const output = [];
  for (const item of items || []) {
    const id = bulletinId(item);
    if (!item || !id || seen.has(id)) {
      continue;
    }
    seen.add(id);
    output.push(item);
  }
  return output;
}

function trimBulletinQueue(items, preserveId = "") {
  const next = uniqueBulletins(items);
  if (next.length <= MAX_STAGE_BULLETINS) {
    return next;
  }
  const tail = next.slice(-MAX_STAGE_BULLETINS);
  if (!preserveId || tail.some((item) => bulletinId(item) === preserveId)) {
    return tail;
  }
  const preserved = next.find((item) => bulletinId(item) === preserveId);
  if (!preserved) {
    return tail;
  }
  const tailWithoutPreserved = next
    .filter((item) => bulletinId(item) !== preserveId)
    .slice(-(MAX_STAGE_BULLETINS - 1));
  return [preserved, ...tailWithoutPreserved];
}

function shouldInterruptBulletin(bulletin) {
  const priority = Number(bulletin?.priority || bulletin?.score || 0) || 0;
  const mode = String(bulletin?.mode || bulletin?.visual || "").toLowerCase();
  return priority >= 95 || (priority >= 90 && ["breaking", "incident"].includes(mode));
}

function latestInterruptBulletin(items, previousIds) {
  for (let index = (items || []).length - 1; index >= 0; index -= 1) {
    const item = items[index];
    const id = bulletinId(item);
    if (id && !previousIds.has(id) && shouldInterruptBulletin(item)) {
      return item;
    }
  }
  return undefined;
}

function queuedAdvanceDelay() {
  if (!activeStartedAt) {
    return activeDwellMs;
  }
  const elapsed = Date.now() - activeStartedAt;
  const remaining = Math.max(activeDwellMs - elapsed, MIN_QUEUED_ADVANCE_MS);
  return Math.min(Math.max(remaining, MIN_QUEUED_ADVANCE_MS), activeDwellMs);
}

function ensureQueuedAdvance(source) {
  if (bulletins.length <= 1 || dwellTimer) {
    return;
  }
  const delay = queuedAdvanceDelay();
  restartStageProgress(delay);
  scheduleNext(delay);
  logDebug("feed.bulletin.queue.advance_scheduled", {
    source,
    delay_ms: delay,
    active_id: bulletinId(activeBulletin()),
    queued: bulletins.length,
  });
}

function showQueuedBulletin(bulletin, source, interrupted = false) {
  const id = bulletinId(bulletin);
  const index = bulletins.findIndex((item) => bulletinId(item) === id);
  activeIndex = index >= 0 ? index : Math.max(bulletins.length - 1, 0);
  renderBulletin(bulletins[activeIndex]);
  scheduleNext(bulletinDwellMs(bulletins[activeIndex]));
  logInfo(interrupted ? "feed.bulletin.interrupt" : "feed.bulletin.show", {
    source,
    bulletin_id: bulletinId(bulletins[activeIndex]),
    queued: bulletins.length,
  });
}

function renderQueueDrained(source) {
  activeIndex = 0;
  stopStageProgress();
  clearStoryTime();
  if (remoteRoute) {
    renderQuietConnected(remoteRoute, {
      kind: "all_seen",
      feedCount: remoteFeedCount ?? 0,
      historyAvailable: true,
    });
  } else {
    setText(liveState, "wait");
    setText(eyebrow, "feed / waiting");
    setText(headline, "waiting for new story");
    setText(deck, "settled headlines will appear as fresh agent work completes.");
    renderChips(["watching", "story-gated"]);
    renderTicker([]);
  }
  logInfo("feed.bulletin.queue.drained", { source, seen: seenBulletinIds.size });
}

function completeActiveBulletin(source) {
  const current = activeBulletin();
  if (current) {
    markBulletinSeen(current, source);
    const currentId = bulletinId(current);
    bulletins = bulletins.filter((item) => bulletinId(item) !== currentId);
  }
  if (!bulletins.length) {
    renderQueueDrained(source);
    return;
  }
  activeIndex = Math.min(activeIndex, bulletins.length - 1);
  const next = activeBulletin();
  renderBulletin(next);
  scheduleNext(bulletinDwellMs(next));
}

function applyBulletinQueueUpdate(nextBulletins, source) {
  const previousActiveId = bulletinId(activeBulletin());
  const previousIds = queueIds();
  const incoming = stageCandidateBulletins(nextBulletins, previousIds);
  const interrupt = latestInterruptBulletin(incoming, previousIds);

  const existing = bulletins.filter((item) => {
    const id = bulletinId(item);
    return !seenBulletinIds.has(id) && (id === previousActiveId || bulletinIsFreshForStage(item));
  });
  const additions = incoming.filter((item) => !previousIds.has(bulletinId(item)));
  bulletins = trimBulletinQueue([...existing, ...additions], previousActiveId);
  if (interrupt && !bulletins.some((item) => bulletinId(item) === bulletinId(interrupt))) {
    bulletins = trimBulletinQueue([...bulletins, interrupt], previousActiveId);
  }
  if (!bulletins.length) {
    activeIndex = 0;
    stopStageProgress();
    return { active: undefined, rendered: false, preserved: false, interrupted: false };
  }

  const preservedIndex = previousActiveId
    ? bulletins.findIndex((item) => bulletinId(item) === previousActiveId)
    : -1;
  const preserved = preservedIndex >= 0;
  activeIndex = preserved ? preservedIndex : 0;

  if (interrupt) {
    showQueuedBulletin(interrupt, source, true);
    return { active: activeBulletin(), rendered: true, preserved, interrupted: true };
  }
  if (!preserved) {
    showQueuedBulletin(activeBulletin(), source, false);
    return { active: activeBulletin(), rendered: true, preserved: false, interrupted: false };
  }

  ensureQueuedAdvance(source);
  logInfo("feed.bulletin.queued", {
    source,
    preserve_active: preserved,
    active_id: previousActiveId,
    queued: bulletins.length,
    new_items: additions.length,
    skipped_seen_or_stale: uniqueBulletins(nextBulletins).length - incoming.length,
  });
  return { active: activeBulletin(), rendered: false, preserved: true, interrupted: false };
}

function queueIncomingBulletin(bulletin, source) {
  if (!bulletin || !bulletinId(bulletin)) {
    logWarn("incoming bulletin missing id", bulletin);
    return;
  }
  if (seenBulletinIds.has(bulletinId(bulletin))) {
    logDebug("feed.bulletin.seen_ignored", { source, bulletin_id: bulletinId(bulletin) });
    return;
  }
  if (!bulletinIsFreshForStage(bulletin)) {
    logDebug("feed.bulletin.stale_ignored", { source, bulletin_id: bulletinId(bulletin) });
    return;
  }
  const previousActiveId = bulletinId(activeBulletin());
  const previousIds = queueIds();
  const existingIndex = bulletins.findIndex((item) => bulletinId(item) === bulletinId(bulletin));
  if (existingIndex >= 0) {
    bulletins[existingIndex] = bulletin;
  } else {
    bulletins.push(bulletin);
  }
  bulletins = trimBulletinQueue(bulletins, previousActiveId);
  const preservedIndex = previousActiveId
    ? bulletins.findIndex((item) => bulletinId(item) === previousActiveId)
    : -1;
  if (preservedIndex >= 0) {
    activeIndex = preservedIndex;
  }

  const isNew = !previousIds.has(bulletinId(bulletin));
  const shouldRender =
    !previousActiveId ||
    bulletins.length === 1 ||
    (isNew && shouldInterruptBulletin(bulletin));
  if (shouldRender) {
    showQueuedBulletin(bulletin, source, isNew && shouldInterruptBulletin(bulletin));
  } else {
    ensureQueuedAdvance(source);
    logInfo("feed.bulletin.queued", {
      source,
      active_id: previousActiveId,
      queued_id: bulletinId(bulletin),
      queued: bulletins.length,
    });
  }
}

function scheduleNext(dwellMs) {
  window.clearTimeout(dwellTimer);
  dwellTimer = undefined;
  if (!bulletins.length) {
    stopStageProgress();
    return;
  }
  const duration = normalizeDwellMs(dwellMs);
  dwellTimer = window.setTimeout(() => {
    dwellTimer = undefined;
    if (bulletins.length <= 1) {
      completeActiveBulletin("dwell");
      return;
    }
    completeActiveBulletin("dwell");
  }, duration);
}

function applySnapshot(snapshot) {
  remoteFeedCount = undefined;
  if (!snapshot || !Array.isArray(snapshot.bulletins)) {
    logWarn("snapshot payload missing bulletins", snapshot);
  }
  const nextSignature = snapshotSignature(snapshot);
  if (nextSignature && nextSignature === localSnapshotSignature) {
    logDebug("feed.snapshot.unchanged", { signature: nextSignature });
    return;
  }
  localSnapshotSignature = nextSignature;
  const result = applyBulletinQueueUpdate(snapshot.bulletins || [], "local-snapshot");
  if (result.active) {
    logInfo("feed.snapshot.queue.applied", {
      bulletins: bulletins.length,
      rendered: result.rendered,
      preserved: result.preserved,
      interrupted: result.interrupted,
    });
  } else {
    stopStageProgress();
    clearStoryTime();
  }
  updateSourceCount();
}

function snapshotSignature(snapshot) {
  if (!snapshot || !Array.isArray(snapshot.bulletins)) {
    return "";
  }
  const last = snapshot.bulletins[snapshot.bulletins.length - 1];
  return [
    snapshot.bulletins.length,
    last?.id || "",
    last?.created_at || last?.createdAt || "",
  ].join(":");
}

async function hydrate() {
  try {
    logInfo("feed.snapshot.request", { url: "/api/reel/snapshot" });
    const response = await fetch("/api/reel/snapshot", { cache: "no-store" });
    if (!response.ok) {
      throw new Error(`snapshot failed: ${response.status}`);
    }
    applySnapshot(await response.json());
    logInfo("feed.snapshot.applied", { bulletins: bulletins.length });
    await hydrateStatus();
  } catch (error) {
    setText(liveState, "wait");
    stopStageProgress();
    logError("snapshot hydration failed", error);
  }
}

async function hydrateStatus() {
  try {
    const response = await fetch("/api/status", { cache: "no-store" });
    if (!response.ok) {
      throw new Error(`status failed: ${response.status}`);
    }
    const status = await response.json();
    logInfo("feed.status.applied", {
      captured_sources: status.captured_sources?.length || 0,
      capture_watchers: status.capture_watchers?.length || 0,
      stored_events: status.stored_events,
      stored_bulletins: status.stored_bulletins,
      last_event_kind: status.last_event_kind,
    });
    if (bulletins.length) {
      return;
    }
    const captures = status.captured_sources || [];
    const watchers = status.capture_watchers || [];
    if (watchers.length) {
      renderCaptureWatchStatus(status, watchers, captures);
    } else if (captures.length) {
      setText(liveState, "capture");
      setText(sourceCount, `${captures.length} src`);
      setText(eyebrow, "local feed / capture live");
      setText(headline, "waiting for settled story");
      const last = status.last_event_kind ? `last event ${status.last_event_kind}` : "future transcript events are being watched";
      setText(deck, `${last}. headlines publish after completion, test, file-change, or incident signals.`);
      clearStoryTime();
      renderChips(["watching", "story-gated", "redacted"]);
      renderTicker(captures.map((capture) => `${capture.agent} ${capture.adapter}`));
    } else {
      setText(sourceCount, "0 src");
    }
  } catch (error) {
    logError("status hydration failed", error);
  }
}

function renderCaptureWatchStatus(status, watchers, captures = []) {
  const active = watchers.filter((watcher) => watcher.state !== "waiting");
  const latest = latestCaptureWatcher(watchers);
  const sourceTotal = Math.max(active.length, watchers.length, captures.length);
  const imported = latest ? Number(latest.imported_events || latest.importedEvents || 0) : 0;
  const filtered = latest ? Number(latest.filtered_events || latest.filteredEvents || 0) : 0;
  const actor = latest ? `${latest.agent || "agent"} ${latest.adapter || "capture"}` : "agent capture";
  setText(liveState, "watch");
  setText(sourceCount, `${sourceTotal} src`);
  setText(eyebrow, "local feed / capture live");
  if (imported > 0) {
    setText(headline, "agent activity received");
    const filteredLine = filtered > 0 ? ` ${filtered} events were outside the workspace filter.` : "";
    setText(deck, `${actor} imported ${imported} event${imported === 1 ? "" : "s"}.${filteredLine} waiting for a settled story.`);
  } else {
    setText(headline, "watching agent sessions");
    setText(deck, `${watchers.length} transcript watcher${watchers.length === 1 ? "" : "s"} active. headlines publish after completion, tests, edits, or incidents.`);
  }
  clearStoryTime();
  renderChips(["watching", "story-gated", "redacted"]);
  renderTicker(watchers.slice(0, 4).map((watcher) => `${watcher.agent || "agent"} ${watcher.adapter || "capture"}`));
  logInfo("feed.capture.watch.render", {
    watchers: watchers.length,
    latest_agent: latest?.agent,
    latest_adapter: latest?.adapter,
    latest_state: latest?.state,
    latest_imported_events: imported,
  });
}

function latestCaptureWatcher(watchers) {
  return watchers
    .slice()
    .sort((left, right) => {
      const leftTime = Date.parse(left.updated_at || left.updatedAt || "") || 0;
      const rightTime = Date.parse(right.updated_at || right.updatedAt || "") || 0;
      return rightTime - leftTime;
    })[0];
}

function parseRemoteRoute(location) {
  const path = location.pathname.replace(/^\/+|\/+$/g, "");
  if (!path) {
    return rootNetworkRouteRequested(location) ? parseGlobalRoute(location) : undefined;
  }
  if (path === "reel" && rootNetworkRouteRequested(location)) {
    return parseGlobalRoute(location);
  }
  if (
    path === "reel" ||
    path.startsWith("reel/") ||
    path === "network" ||
    path.startsWith("network/") ||
    path.startsWith("api")
  ) {
    return undefined;
  }
  const pathSegments = path.split("/");
  if (pathSegments[0] === "org") {
    if (pathSegments.length !== 2 || pathSegments.some((segment) => !segment || segment.startsWith("."))) {
      return undefined;
    }
    let org = pathSegments[1];
    try {
      org = decodeURIComponent(org);
    } catch (error) {
      logError("org route decode failed", error);
      return undefined;
    }
    if (!/^[A-Za-z0-9](?:[A-Za-z0-9-]{0,37}[A-Za-z0-9])?$/.test(org)) {
      return undefined;
    }
    const params = new URLSearchParams(
      location.hash && location.hash.length > 1
        ? location.hash.slice(1)
        : location.search,
    );
    for (const key of ["redact", "raw", "raw_events", "diffs", "prompts"]) {
      if (params.has(key)) {
        logWarn(`ignored privacy-weakening query param: ${key}`);
      }
    }
    const streamFilter = params.get("streams") || (params.has("all") ? "*" : "");
    return {
      kind: "org",
      org,
      login: org,
      network: params.get("network") || "mainnet",
      feed: streamFilter === "all" ? "*" : streamFilter,
      selection: `org/${org}${streamFilter ? `/${streamFilter}` : ""}`,
      feedMode: routeFeedMode(params, "org"),
      followingTargets: routeFollowingTargets(org, params),
      interactive: routeHistoryRequested(params),
      query: location.search,
    };
  }
  if (pathSegments.length > 2 || pathSegments.some((segment) => !segment || segment.startsWith("."))) {
    return undefined;
  }
  let login = pathSegments[0].startsWith("@") ? pathSegments[0].slice(1) : pathSegments[0];
  try {
    login = decodeURIComponent(login);
  } catch (error) {
    logError("remote route decode failed", error);
    return undefined;
  }
  if (!/^[A-Za-z0-9](?:[A-Za-z0-9-]{0,37}[A-Za-z0-9])?$/.test(login)) {
    return undefined;
  }
  const feedSegment = pathSegments[1] ? decodeFeedSegment(pathSegments[1]) : "";
  if (feedSegment === undefined) {
    return undefined;
  }
  const params = new URLSearchParams(
    location.hash && location.hash.length > 1
      ? location.hash.slice(1)
      : location.search,
  );
  for (const key of ["redact", "raw", "raw_events", "diffs", "prompts"]) {
    if (params.has(key)) {
      logWarn(`ignored privacy-weakening query param: ${key}`);
    }
  }
  return {
    kind: "user",
    login,
    network: params.get("network") || "mainnet",
    feed: feedSegment,
    selection: feedSegment ? `${login}/${feedSegment}` : routeSelection(login, params),
    feedMode: routeFeedMode(params, "user"),
    followingTargets: routeFollowingTargets(login, params),
    interactive: routeHistoryRequested(params),
    query: location.search,
  };
}

function parseGlobalRoute(location) {
  const params = new URLSearchParams(location.search);
  for (const key of ["redact", "raw", "raw_events", "diffs", "prompts"]) {
    if (params.has(key)) {
      logWarn(`ignored privacy-weakening query param: ${key}`);
    }
  }
  return {
    kind: "global",
    login: "",
    network: params.get("network") || "mainnet",
    feed: "*",
    selection: "network/*",
    feedMode: routeFeedMode(params, "global"),
    followingTargets: routeFollowingTargets("", params),
    interactive: routeHistoryRequested(params),
    query: location.search,
  };
}

function rootNetworkRouteRequested(location) {
  const params = new URLSearchParams(location.search);
  return (
    document.body.dataset.view === "remote" ||
    params.has("network") ||
    params.has("feed_mode") ||
    params.has("feedMode") ||
    params.has("subscriptions") ||
    params.has("subscribed") ||
    params.has("following") ||
    params.has("discover") ||
    params.has("discovery") ||
    routeHistoryRequested(params)
  );
}

function routeHistoryRequested(params) {
  const view = String(params.get("view") || "").toLowerCase();
  const mode = String(params.get("mode") || "").toLowerCase();
  return (
    view === "history" ||
    view === "timeline" ||
    mode === "history" ||
    mode === "timeline" ||
    ["1", "true", "on"].includes(params.get("history") || "") ||
    ["1", "true", "on"].includes(params.get("timeline") || "")
  );
}

function routeFeedMode(params, scope = "user") {
  const explicit = (
    params.get("feed_mode") ||
    params.get("feedMode") ||
    params.get("source") ||
    ""
  ).toLowerCase();
  if (["local", "loopback"].includes(explicit)) {
    logInfo("feed.local_link.ignored", {
      message: "hosted feed pages do not link to loopback reels",
    });
  }
  if (["subscribed", "subscriptions", "following"].includes(explicit)) {
    return "following";
  }
  if (scope === "global" && ["discovery", "discover", "hero", "public", ""].includes(explicit)) {
    return "discovery";
  }
  if (
    params.has("subscriptions") ||
    params.has("subscribed") ||
    ["1", "true", "on"].includes(params.get("following") || "")
  ) {
    return "following";
  }
  if (scope === "global") {
    return "discovery";
  }
  if (["discovery", "discover", "hero", "public"].includes(explicit)) {
    logInfo("feed.user.discovery_alias", {
      message: "per-user discovery is represented by user/* wildcard routes",
    });
  }
  return "discovery";
}

function routeFollowingTargets(login, params) {
  const raw =
    params.get("subscriptions") ||
    params.get("subscribed") ||
    params.get("following") ||
    "";
  const explicit = raw
    .split(",")
    .map((target) => target.trim())
    .filter(Boolean)
    .filter(isSafeFollowTarget);
  if (explicit.length) {
    return dedupeTargets(explicit.map(normalizeFollowTarget).filter(Boolean));
  }
  const stored = storedFollowingTargets().filter(isSafeFollowTarget);
  if (!login) {
    return stored;
  }
  return stored.filter((target) => followTargetMatchesLogin(target, login));
}

function storedFollowingTargets() {
  return storedFollowEntries().map(followEntryToTarget).filter(Boolean);
}

function storedFollowEntries() {
  try {
    const rawV2 = window.localStorage.getItem(FOLLOWING_STORAGE_KEY_V2) || "";
    if (rawV2) {
      const entries = parseFollowEntries(rawV2);
      if (entries.length) {
        return entries;
      }
    }
    for (const key of FOLLOWING_LEGACY_STORAGE_KEYS) {
      const raw = window.localStorage.getItem(key) || "";
      if (!raw) {
        continue;
      }
      const entries = parseFollowEntries(raw);
      if (entries.length) {
        saveFollowEntries(entries);
        return entries;
      }
    }
  } catch (error) {
    logError("following list read failed", error);
  }
  return [];
}

function parseFollowEntries(raw) {
  try {
    const value = JSON.parse(raw);
    if (Array.isArray(value)) {
      return dedupeFollowEntries(value.map(normalizeFollowEntry).filter(Boolean));
    }
    if (typeof value === "string") {
      return parseFollowEntriesFromCsv(value);
    }
  } catch (_error) {
    return parseFollowEntriesFromCsv(raw);
  }
  return [];
}

function parseFollowEntriesFromCsv(value) {
  return dedupeFollowEntries(
    String(value || "")
      .split(",")
      .map((target) => normalizeFollowEntry(target.trim()))
      .filter(Boolean),
  );
}

function normalizeFollowEntry(value) {
  if (typeof value === "string") {
    return followEntryFromTarget(normalizeFollowTarget(value));
  }
  if (!value || typeof value !== "object") {
    return undefined;
  }
  const kind = String(value.kind || "").toLowerCase();
  if (kind === "tag") {
    const tag = normalizeTag(value.tag || value.value || value.target || "");
    return tag ? { kind: "tag", tag, created_at: value.created_at || value.createdAt || new Date().toISOString() } : undefined;
  }
  const login = normalizeGithubLogin(value.login || value.github_login || value.githubLogin || "");
  if (!login) {
    return undefined;
  }
  const feed = normalizeFeedFollowLabel(value.feed || value.feed_label || value.feedLabel || value.stream || "*");
  if (kind === "person" || feed === "*") {
    return { kind: "person", login, created_at: value.created_at || value.createdAt || new Date().toISOString() };
  }
  return { kind: "feed", login, feed, created_at: value.created_at || value.createdAt || new Date().toISOString() };
}

function followEntryFromTarget(target) {
  if (!target) {
    return undefined;
  }
  if (isTagFollowTarget(target)) {
    return { kind: "tag", tag: normalizeTag(target), created_at: new Date().toISOString() };
  }
  const [login, feed = "*"] = target.split("/");
  if (!login) {
    return undefined;
  }
  return feed === "*"
    ? { kind: "person", login, created_at: new Date().toISOString() }
    : { kind: "feed", login, feed, created_at: new Date().toISOString() };
}

function followEntryToTarget(entry) {
  const clean = normalizeFollowEntry(entry);
  if (!clean) {
    return "";
  }
  if (clean.kind === "tag") {
    return `#${clean.tag}`;
  }
  if (clean.kind === "person") {
    return `${clean.login}/*`;
  }
  return `${clean.login}/${clean.feed}`;
}

function dedupeFollowEntries(entries) {
  const seen = new Set();
  const output = [];
  for (const entry of entries || []) {
    const clean = normalizeFollowEntry(entry);
    const key = followEntryToTarget(clean);
    if (!key || seen.has(key)) {
      continue;
    }
    seen.add(key);
    output.push(clean);
  }
  return output;
}

function saveFollowingTargets(targets) {
  const clean = dedupeFollowEntries(targets.map((target) => followEntryFromTarget(normalizeFollowTarget(target))).filter(Boolean));
  return saveFollowEntries(clean).map(followEntryToTarget).filter(Boolean);
}

function saveFollowEntries(entries) {
  const clean = dedupeFollowEntries(entries);
  const targets = clean.map(followEntryToTarget).filter(Boolean);
  window.localStorage.setItem(FOLLOWING_STORAGE_KEY_V2, JSON.stringify(clean));
  window.localStorage.setItem("feed.following", JSON.stringify(targets));
  logInfo("feed.following.saved", { targets: clean });
  return clean;
}

function dedupeTargets(targets) {
  return [...new Set(targets)];
}

function normalizeFollowTarget(value) {
  const raw = String(value || "").trim();
  if (raw.startsWith("#")) {
    const tag = normalizeTag(raw);
    return tag ? `#${tag}` : "";
  }
  const clean = raw.replace(/^@/, "");
  if (!isSafeFollowTarget(clean)) {
    return "";
  }
  const [login, feed = "*"] = clean.split("/");
  const normalizedLogin = normalizeGithubLogin(login);
  const normalizedFeed = normalizeFeedFollowLabel(feed || "*");
  return normalizedLogin && normalizedFeed ? `${normalizedLogin}/${normalizedFeed}` : "";
}

function isFollowingTarget(target) {
  const clean = normalizeFollowTarget(target);
  if (!clean) {
    return false;
  }
  return storedFollowingTargets().includes(clean);
}

function toggleFollowTarget(target) {
  const clean = normalizeFollowTarget(target);
  if (!clean) {
    return [];
  }
  const current = storedFollowingTargets();
  const next = current.includes(clean)
    ? current.filter((value) => value !== clean)
    : [...current, clean];
  return saveFollowingTargets(next);
}

function isSafeSubscriptionTarget(value) {
  return isSafeFollowTarget(value);
}

function isSafeFollowTarget(value) {
  const clean = String(value || "").trim();
  if (clean.startsWith("#")) {
    return /^#[A-Za-z0-9][A-Za-z0-9_.-]{0,63}$/.test(clean);
  }
  return /^@?[A-Za-z0-9](?:[A-Za-z0-9-]{0,37}[A-Za-z0-9])?(?:\/(?:\*|[A-Za-z0-9][A-Za-z0-9_.-]{0,63}))?$/.test(
    value,
  );
}

function isTagFollowTarget(target) {
  return String(target || "").trim().startsWith("#");
}

function normalizeGithubLogin(value) {
  const clean = String(value || "").trim().replace(/^@/, "");
  if (!/^[A-Za-z0-9](?:[A-Za-z0-9-]{0,37}[A-Za-z0-9])?$/.test(clean)) {
    return "";
  }
  return clean;
}

function normalizeFeedFollowLabel(value) {
  const clean = String(value || "").trim() || "*";
  if (clean === "*") {
    return "*";
  }
  return /^[A-Za-z0-9][A-Za-z0-9_.-]{0,63}$/.test(clean) ? clean : "";
}

function followTargetMatchesLogin(target, login) {
  if (isTagFollowTarget(target)) {
    return false;
  }
  return normalizeRouteLogin(target.split("/")[0]) === normalizeRouteLogin(login);
}

function followTargetLabel(target) {
  const clean = normalizeFollowTarget(target);
  if (!clean) {
    return "feed";
  }
  if (isTagFollowTarget(clean)) {
    return clean;
  }
  const [login, feed = "*"] = clean.split("/");
  return feed === "*" ? `@${login}` : `@${login}/${feed}`;
}

function decodeFeedSegment(segment) {
  if (segment === "*") {
    return "*";
  }
  let decoded = "";
  try {
    decoded = decodeURIComponent(segment);
  } catch (error) {
    logError("feed segment decode failed", error);
    return undefined;
  }
  if (!/^[A-Za-z0-9][A-Za-z0-9_.-]{0,63}$/.test(decoded)) {
    return undefined;
  }
  return decoded;
}

function routeSelection(login, params) {
  const streams = params.get("streams");
  const explicit = (
    params.get("feed_mode") ||
    params.get("feedMode") ||
    params.get("source") ||
    ""
  ).toLowerCase();
  if (
    params.has("all") ||
    streams === "all" ||
    streams === "*" ||
    streams === `${login}/*` ||
    ["discovery", "discover", "hero", "public"].includes(explicit)
  ) {
    return `${login}/*`;
  }
  if (streams && !streams.includes(",")) {
    return `${login}/${streams}`;
  }
  return `${login}/*`;
}

function p2pEnabled() {
  return window.FEED_P2P_ENABLED === true || window.FEED_P2P_ENABLED === "true";
}

function setupModeSwitcher(route) {
  if (!modeSwitcher || !modeDiscovery || !modeFollowing || !modeHistory || !p2pEnabled()) {
    hideModeSwitcher();
    return;
  }
  modeDiscovery.textContent = route.kind === "global" ? "discover" : route.kind === "org" ? "org feeds" : "feeds";
  modeFollowing.textContent = route.kind === "user" ? `following @${route.login}` : "following";
  modeHistory.textContent = "history";
  modeDiscovery.setAttribute("href", modeUrl(route, "discovery"));
  modeFollowing.setAttribute("href", modeUrl(route, "following"));
  modeHistory.setAttribute("href", historyUrl(route));
  modeDiscovery.toggleAttribute(
    "aria-current",
    !route.interactive &&
      ((route.kind === "global" && route.feedMode === "discovery") ||
        ((route.kind === "user" || route.kind === "org") && route.feedMode !== "following")),
  );
  modeFollowing.toggleAttribute("aria-current", !route.interactive && route.feedMode === "following");
  modeHistory.toggleAttribute("aria-current", route.interactive);
  modeSwitcher.hidden = false;
  const reveal = () => revealControls();
  window.addEventListener("pointermove", reveal, { passive: true });
  window.addEventListener("pointerdown", reveal, { passive: true });
  window.addEventListener("touchstart", reveal, { passive: true });
  window.addEventListener("keydown", reveal);
  modeSwitcher.addEventListener("focusin", reveal);
}

function hideModeSwitcher() {
  if (modeSwitcher) {
    modeSwitcher.hidden = true;
  }
  document.body.classList.remove("controls-visible");
}

function revealControls() {
  if (!modeSwitcher || modeSwitcher.hidden) {
    return;
  }
  document.body.classList.add("controls-visible");
  window.clearTimeout(controlsTimer);
  controlsTimer = window.setTimeout(() => {
    if (!modeSwitcher.matches(":focus-within")) {
      document.body.classList.remove("controls-visible");
    }
  }, 3600);
}

function modeUrl(route, mode) {
  const params = new URLSearchParams(route.query);
  params.set("feed_mode", mode);
  params.delete("history");
  params.delete("timeline");
  params.delete("view");
  if (params.get("mode") === "history" || params.get("mode") === "timeline") {
    params.delete("mode");
  }
  if (route.network && route.network !== "mainnet") {
    params.set("network", route.network);
  }
  if (mode === "discovery") {
    params.delete("subscriptions");
    params.delete("subscribed");
    params.delete("following");
    params.delete("streams");
  }
  if (mode === "following") {
    params.delete("all");
    params.delete("streams");
  }
  if (route.kind === "global") {
    const query = params.toString();
    return query ? `/?${query}` : "/";
  }
  if (route.kind === "org") {
    if (mode === "discovery") {
      params.delete("feed_mode");
      params.set("all", "true");
    }
    const query = params.toString();
    return `/org/${encodeURIComponent(route.org || route.login)}${query ? `?${query}` : ""}`;
  }
  if (mode === "discovery") {
    params.delete("feed_mode");
    params.set("all", "true");
  }
  const userFeed =
    mode === "discovery"
      ? "*"
      : route.feed && route.feed !== "*"
        ? route.feed
        : route.followingTargets.length === 1 &&
            route.followingTargets[0].replace(/^@/, "").startsWith(`${route.login}/`)
          ? route.followingTargets[0].replace(/^@/, "").split("/")[1] || "*"
          : route.feed === "*"
            ? "*"
            : "";
  const nextParams = new URLSearchParams(params);
  const nextQuery = nextParams.toString();
  const path =
    userFeed && userFeed !== "*"
      ? `/${encodeURIComponent(route.login)}/${encodeURIComponent(userFeed)}`
      : `/${encodeURIComponent(route.login)}${userFeed === "*" ? "/*" : ""}`;
  return nextQuery ? `${path}?${nextQuery}` : path;
}

function historyUrl(route) {
  const params = new URLSearchParams(route.query || "");
  params.set("view", "history");
  params.delete("history");
  params.delete("timeline");
  if (params.get("mode") === "history" || params.get("mode") === "timeline") {
    params.delete("mode");
  }
  for (const key of ["redact", "raw", "raw_events", "diffs", "prompts"]) {
    params.delete(key);
  }
  const query = params.toString();
  return `${routePath(route)}${query ? `?${query}` : ""}`;
}

function parseGithubAuthCallback(location) {
  if (location.pathname !== "/callback/github") {
    return undefined;
  }
  const params = githubCallbackParams(location);
  if (params.has("code") && !params.has("login") && !params.has("github_user_id")) {
    return undefined;
  }
  return {
    state: params.get("state") || "",
    login: params.get("login") || "",
    github_user_id: params.get("github_user_id") || params.get("id") || "",
    name: params.get("name") || "",
    avatar_url: params.get("avatar_url") || params.get("avatar") || "",
    session_token:
      params.get("session") || params.get("session_token") || params.get("grant") || "",
    scopes: params.get("scopes") || "",
    github_orgs: params.get("github_orgs") || params.get("orgs") || "",
    expires_at: params.get("expires_at") || "",
    return_to: params.get("return_to") || "/network",
  };
}

function githubCallbackParams(location) {
  const hash = location.hash && location.hash.startsWith("#") ? location.hash.slice(1) : "";
  const query = location.search && location.search.startsWith("?") ? location.search.slice(1) : "";
  return new URLSearchParams(hash || query);
}

function forwardRawGithubOauthCallback(location) {
  if (location.pathname !== "/callback/github") {
    return false;
  }
  const params = new URLSearchParams(location.search);
  if (!params.has("code") || !params.has("state")) {
    return false;
  }
  const edge = edgeBaseUrl();
  if (!edge) {
    logError("github raw oauth callback cannot be forwarded without edge base url");
    return false;
  }
  const target = `${edge.replace(/\/$/, "")}/callback/github${location.search}`;
  logInfo("feed.github.callback.forward_edge", { edge });
  window.location.replace(target);
  return true;
}

function edgeBaseUrl() {
  if (window.FEED_EDGE_BASE_URL) {
    return window.FEED_EDGE_BASE_URL;
  }
  if (window.AGENT_FEED_EDGE_BASE_URL) {
    return window.AGENT_FEED_EDGE_BASE_URL;
  }
  if (window.location.hostname === "feed.aberration.technology") {
    return "https://api.feed.aberration.technology";
  }
  return "";
}

function isNetworkView() {
  return window.location.pathname === "/network" || document.body.dataset.view === "network";
}

function storedGithubSession() {
  try {
    const raw =
      window.localStorage.getItem("feed.github.session") ||
      window.localStorage.getItem("agent_feed.github.session");
    return raw ? JSON.parse(raw) : undefined;
  } catch (error) {
    logError("github session read failed", error);
    return undefined;
  }
}

function githubAuthHeaders() {
  const session = storedGithubSession();
  if (!session?.session_token) {
    return {};
  }
  return { authorization: `Bearer ${session.session_token}` };
}

function browserSignInUrl(returnTo = window.location.href, githubOrg = undefined) {
  const state = randomState();
  window.localStorage.setItem("feed.github.auth_state", state);
  const params = new URLSearchParams();
  params.set("client", "feed-browser");
  params.set("return_to", returnTo);
  params.set("state", state);
  if (githubOrg) {
    params.set("org", githubOrg);
    params.set("scope", "read:user read:org");
  }
  return `${edgeBaseUrl()}/auth/github?${params.toString()}`;
}

function randomState() {
  const bytes = new Uint8Array(16);
  if (window.crypto?.getRandomValues) {
    window.crypto.getRandomValues(bytes);
  } else {
    for (let index = 0; index < bytes.length; index += 1) {
      bytes[index] = Math.floor(Math.random() * 256);
    }
    logWarn("browser crypto unavailable; github auth state used weak fallback");
  }
  return Array.from(bytes, (byte) => byte.toString(16).padStart(2, "0")).join("");
}

function handleGithubAuthCallback(callback) {
  setText(liveState, "auth");
  setText(eyebrow, "github / sign-in / callback");
  setText(headline, "github sign-in");
  renderPublisher(undefined);
  renderHeadlineImage(undefined);
  clearStageActions();
  renderChips(["github", "verified", "browser", "session"]);
  stopStageProgress();
  const expectedState =
    window.localStorage.getItem("feed.github.auth_state") ||
    window.localStorage.getItem("agent_feed.github.auth_state") ||
    "";
  if (!callback.state || callback.state !== expectedState) {
    setText(deck, "sign-in state did not match. start again from /network.");
    logError("github browser callback state mismatch", {
      has_state: Boolean(callback.state),
      expected_state_present: Boolean(expectedState),
    });
    return;
  }
  if (!callback.login || !callback.github_user_id) {
    setText(deck, "sign-in callback was missing github identity.");
    logError("github browser callback missing identity", {
      has_login: Boolean(callback.login),
      has_github_user_id: Boolean(callback.github_user_id),
    });
    return;
  }
  const session = {
    provider: "github",
    login: callback.login,
    github_user_id: callback.github_user_id,
    name: callback.name || undefined,
    avatar_url: callback.avatar_url || undefined,
    session_token: callback.session_token || undefined,
    scopes: callback.scopes ? callback.scopes.split(" ").filter(Boolean) : [],
    github_orgs: callback.github_orgs ? callback.github_orgs.split(",").filter(Boolean) : [],
    expires_at: callback.expires_at || undefined,
    edge_base_url: edgeBaseUrl(),
  };
  window.localStorage.setItem("feed.github.session", JSON.stringify(session));
  window.localStorage.removeItem("feed.github.auth_state");
  window.localStorage.removeItem("agent_feed.github.auth_state");
  setText(deck, `signed in as @${callback.login}. returning to network.`);
  logInfo("feed.github.signin.complete", {
    login: callback.login,
    github_user_id: callback.github_user_id,
  });
  window.setTimeout(() => {
    window.location.replace(callback.return_to || "/network");
  }, 700);
}

function startNetworkView() {
  showStage();
  const session = storedGithubSession();
  logInfo("feed.network.view", {
    signed_in: Boolean(session),
    login: session?.login,
    edge_base_url: edgeBaseUrl(),
  });
  document.body.dataset.mode = "dispatch";
  setText(liveState, session ? "auth" : "wait");
  setText(eyebrow, "feed / network / github");
  if (session) {
    setText(headline, `@${session.login}`);
    setText(deck, "github session is available for browser feed discovery and private stream grants.");
    renderPublisher({
      github_login: session.login,
      display_name: session.name,
      avatar: session.avatar_url,
    });
    renderHeadlineImage(undefined);
    setAuthAction(browserSignInUrl(`${window.location.origin}/network`), "refresh github sign-in");
    clearStageActions();
    renderChips(["github", "signed-in", "browser", "story-only"]);
  } else {
    setText(headline, "github sign-in");
    setText(deck, "sign in to request private feed grants and receive signed browser bootstrap material.");
    renderPublisher(undefined);
    renderHeadlineImage(undefined);
    setAuthAction(browserSignInUrl(`${window.location.origin}/network`));
    clearStageActions();
    renderChips(["github", "browser", "private feeds", "redacted"]);
  }
  renderTicker(["auth stays on the edge", "projection remains story-only"]);
  stopStageProgress();
}

async function startRemoteRoute(route) {
  logInfo("feed.remote.route.start", {
    scope: route.kind || "user",
    login: route.login,
    selection: route.selection,
    feed_mode: route.feedMode,
    network: route.network,
    interactive: route.interactive,
    p2p_enabled: p2pEnabled(),
  });
  if (!p2pEnabled()) {
    renderP2pDisabled(route);
    return;
  }
  setupModeSwitcher(route);
  if (route.feedMode === "following") {
    await startFollowingRoute(route);
    scheduleRemoteRefresh(route, "initial-following");
    return;
  }
  if (route.kind === "global") {
    await startGlobalDiscoveryRoute(route);
    scheduleRemoteRefresh(route, "initial-global");
    return;
  }
  if (route.kind === "org") {
    await startOrgRoute(route);
    scheduleRemoteRefresh(route, "initial-org");
    return;
  }
  await startUserRoute(route);
  scheduleRemoteRefresh(route, "initial-user");
}

function scheduleRemoteRefresh(route, reason = "timer") {
  window.clearTimeout(remoteRefreshTimer);
  if (!p2pEnabled()) {
    return;
  }
  remoteRefreshTimer = window.setTimeout(() => {
    refreshRemoteRoute(route, reason);
  }, REMOTE_SNAPSHOT_REFRESH_MS);
  logDebug("feed.remote.refresh.scheduled", {
    reason,
    interval_ms: REMOTE_SNAPSHOT_REFRESH_MS,
    scope: route.kind || "user",
    selection: route.selection,
  });
}

async function refreshRemoteRoute(route, reason = "timer") {
  if (remoteRefreshInFlight) {
    scheduleRemoteRefresh(route, "busy");
    return;
  }
  remoteRefreshInFlight = true;
  try {
    logInfo("feed.remote.refresh", {
      reason,
      scope: route.kind || "user",
      selection: route.selection,
      feed_mode: route.feedMode,
      interactive: route.interactive,
    });
    if (route.feedMode === "following") {
      await startFollowingRoute(route, true);
    } else if (route.kind === "global") {
      await startGlobalDiscoveryRoute(route, true);
    } else if (route.kind === "org") {
      await startOrgRoute(route, true);
    } else {
      await startUserRoute(route, true);
    }
  } finally {
    remoteRefreshInFlight = false;
    scheduleRemoteRefresh(route, "interval");
  }
}

async function startGlobalDiscoveryRoute(route, refresh = false) {
  if (!refresh) {
    renderRemoteState(route, "resolving", [
      "joining p2p network",
      `searching ${route.network}`,
      "requesting accessible story snapshots",
      "waiting for settled story capsules",
    ]);
  }
  const endpoint = `${edgeBaseUrl()}/network/snapshot${networkDiscoveryQuery(route)}`;
  try {
    logInfo("feed.network.discovery.request", {
      endpoint,
      network: route.network,
      interactive: route.interactive,
    });
    const response = await fetch(endpoint, {
      cache: "no-store",
      headers: githubAuthHeaders(),
    });
    logInfo("feed.network.discovery.response", {
      status: response.status,
      ok: response.ok,
    });
    if (response.status === 401 || response.status === 403) {
      renderAuthRequired(route);
      return;
    }
    if (response.status === 426) {
      const message = await responseErrorMessage(response, "update your peer to the latest version");
      renderRemoteState(route, "version-mismatch", [
        "feed protocol network or data model changed",
        message,
        "update your peer or choose the matching network",
      ]);
      logWarn("feed.network.discovery.upgrade_required", { message });
      return;
    }
    if (!response.ok) {
      throw new Error(`network snapshot failed: ${response.status}`);
    }
    const snapshot = await response.json();
    const snapshotStatus = compatibilityStatus(snapshot.compatibility || snapshot.browser_seed?.compatibility);
    if (!snapshotStatus.compatible) {
      renderRemoteState(route, "version-mismatch", [
        "feed protocol or data model changed",
        snapshotStatus.message,
        "update your peer to the latest version",
      ]);
      logWarn("feed.network.discovery.version_mismatch", {
        compatibility: snapshot.compatibility,
        message: snapshotStatus.message,
      });
      return;
    }
    const snapshotNetworkStatus = networkCompatibilityStatus(route, snapshot);
    if (!snapshotNetworkStatus.compatible) {
      renderRemoteState(route, "version-mismatch", [
        "feed network changed",
        snapshotNetworkStatus.message,
        "update your peer or choose the matching network",
      ]);
      logWarn("feed.network.discovery.network_mismatch", {
        expected: snapshotNetworkStatus.expected,
        actual: snapshotNetworkStatus.actual,
      });
      return;
    }
    const feeds = snapshotFeeds(snapshot);
    const allHeadlines = snapshotHeadlines(snapshot);
    const headlines = allHeadlines.filter((item) => compatibilityStatus(item.compatibility).compatible);
    const incompatibleHeadlines = allHeadlines.length - headlines.length;
    if (incompatibleHeadlines > 0) {
      logWarn("feed.network.discovery.incompatible_headlines_ignored", {
        incompatible_headlines: incompatibleHeadlines,
      });
    }
    logInfo("feed.network.discovery.snapshot", {
      network_id: snapshot.network_id,
      feeds: feeds.length,
      headlines: headlines.length,
      incompatible_headlines: incompatibleHeadlines,
    });
    if (route.interactive) {
      renderGlobalTimeline(route, snapshot);
      return;
    }
    const classification = classifyRemoteSnapshot(route, feeds, headlines);
    logInfo("feed.network.discovery.classified", {
      classification: classification.kind,
      feeds: classification.feedCount,
      headlines: classification.headlineCount,
      fresh_unseen: classification.freshUnseenCount,
    });
    if (classification.kind === "fresh_unseen") {
      updateSourceCountFromFeeds(feeds, headlines);
      applyRemoteHeadlines(route, headlines, classification);
      return;
    }
    if (classification.historyAvailable) {
      renderQuietConnected(route, classification);
      return;
    }
    renderRemoteState(route, "no-feeds", [
      "no visible settled story streams",
      "network fabric may still be routing peers",
      "waiting for published headlines",
    ]);
    updateSourceCountFromFeeds([]);
  } catch (error) {
    renderRemoteState(route, "failed", [
      "edge snapshot mode unavailable",
      "waiting for p2p live path",
    ]);
    logError("network discovery failed", error);
  }
}

async function startUserRoute(route, refresh = false) {
  if (!refresh) {
    renderRemoteState(route, "resolving", [
      "resolving github identity",
      `finding feeds on ${route.network}`,
      "dialing p2p peers",
      "waiting for story capsules",
    ]);
  }
  const endpoint = `${edgeBaseUrl()}/resolve/github/${encodeURIComponent(route.login)}${resolverQuery(route)}`;
  try {
    logInfo("feed.resolver.request", {
      login: route.login,
      selection: route.selection,
      endpoint,
    });
    const response = await fetch(endpoint, {
      cache: "no-store",
      headers: githubAuthHeaders(),
    });
    logInfo("feed.resolver.response", {
      login: route.login,
      status: response.status,
      ok: response.ok,
    });
    let ticket;
    if (response.status === 404) {
      ticket = await fetchDirectoryTicketForUser(route);
      if (!ticket) {
        renderRemoteState(route, "not-found", ["github user not found"]);
        return;
      }
      logWarn("feed.resolver.directory_fallback", {
        login: route.login,
        feeds: ticketFeeds(ticket).length,
        headlines: snapshotHeadlines(ticket).length,
      });
    }
    if (response.status === 401 || response.status === 403) {
      renderAuthRequired(route);
      logInfo("feed.remote.auth_required", {
        login: route.login,
        status: response.status,
      });
      return;
    }
    if (response.status === 426) {
      const message = await responseErrorMessage(response, "update your peer to the latest version");
      renderRemoteState(route, "version-mismatch", [
        "feed protocol network or data model changed",
        message,
        "update your peer or choose the matching network",
      ]);
      logWarn("feed.user.upgrade_required", { login: route.login, message });
      return;
    }
    if (!response.ok && !ticket) {
      throw new Error(`resolver failed: ${response.status}`);
    }
    if (!ticket) {
      ticket = await response.json();
    }
    const ticketStatus = compatibilityStatus(ticket.compatibility || ticket.browser_seed?.compatibility);
    if (!ticketStatus.compatible) {
      renderRemoteState(route, "version-mismatch", [
        "feed protocol or data model changed",
        ticketStatus.message,
        "update your peer to the latest version",
      ], ticket.profile);
      logWarn("feed.user.version_mismatch", {
        login: ticket.profile?.login || route.login,
        compatibility: ticket.compatibility,
        message: ticketStatus.message,
      });
      return;
    }
    const ticketNetworkStatus = networkCompatibilityStatus(route, ticket);
    if (!ticketNetworkStatus.compatible) {
      renderRemoteState(route, "version-mismatch", [
        "feed network changed",
        ticketNetworkStatus.message,
        "update your peer or choose the matching network",
      ], ticket.profile);
      logWarn("feed.user.network_mismatch", {
        login: ticket.profile?.login || route.login,
        expected: ticketNetworkStatus.expected,
        actual: ticketNetworkStatus.actual,
      });
      return;
    }
    const allFeeds = ticketFeeds(ticket);
    const compatibleFeeds = allFeeds.filter((feed) => compatibilityStatus(feed.compatibility).compatible);
    const incompatibleFeeds = allFeeds.length - compatibleFeeds.length;
    if (incompatibleFeeds > 0) {
      logWarn("feed.user.incompatible_feeds_ignored", {
        login: ticket.profile?.login || route.login,
        incompatible_feeds: incompatibleFeeds,
      });
    }
    if (ticket.feeds) {
      ticket.feeds = compatibleFeeds;
    }
    if (ticket.candidate_feeds) {
      ticket.candidate_feeds = compatibleFeeds;
    }
    const feedCount = compatibleFeeds.length;
    const allHeadlines = snapshotHeadlines(ticket);
    const headlines = allHeadlines
      .filter((item) => compatibilityStatus(item.compatibility || ticket.compatibility).compatible)
      .filter((item) => headlineMatchesRoute(item, route))
      .map((item) => enrichHeadlineFromTicket(item, ticket));
    const incompatibleHeadlines = allHeadlines.length - allHeadlines.filter((item) => compatibilityStatus(item.compatibility || ticket.compatibility).compatible).length;
    logInfo("feed.user.ticket", {
      login: ticket.profile?.login || route.login,
      github_user_id: ticket.github_user_id || ticket.resolved_github_id,
      feeds: feedCount,
      headlines: headlines.length,
      incompatible_feeds: incompatibleFeeds,
      incompatible_headlines: incompatibleHeadlines,
      expires_at: ticket.expires_at,
    });
    if (allFeeds.length > 0 && feedCount === 0) {
      const firstStatus = compatibilityStatus(allFeeds[0].compatibility);
      renderRemoteState(route, "version-mismatch", [
        "visible feeds use a different data model",
        firstStatus.message,
        "update your peer to the latest version",
      ], ticket.profile);
      return;
    }
    const classification = classifyRemoteSnapshot(route, compatibleFeeds, headlines, {
      profile: ticket.profile,
    });
    logInfo("feed.user.classified", {
      login: ticket.profile?.login || route.login,
      classification: classification.kind,
      feeds: classification.feedCount,
      headlines: classification.headlineCount,
      fresh_unseen: classification.freshUnseenCount,
    });
    if (classification.kind === "fresh_unseen" && !route.interactive) {
      updateSourceCountFromFeeds(compatibleFeeds, headlines);
      applyRemoteHeadlines(route, headlines, classification);
      return;
    }
    if (feedCount === 0 && headlines.length === 0) {
      renderRemoteState(route, "no-feeds", [
        "github identity found",
        "no visible settled story streams",
      ], ticket.profile);
      updateSourceCountFromFeeds([]);
      logInfo("feed.user.no_visible_streams", {
        login: ticket.profile?.login || route.login,
        github_user_id: ticket.github_user_id || ticket.resolved_github_id,
      });
      return;
    }
    if (route.interactive) {
      renderTimeline(route, ticket);
      logInfo("feed.history.ready", {
        selection: route.selection,
        feeds: feedCount,
      });
      return;
    }
    renderQuietConnected(route, classification, ticket.profile);
  } catch (error) {
    renderRemoteState(route, "failed", [
      "edge snapshot mode unavailable",
      "waiting for p2p live path",
    ]);
    logError("remote route resolution failed", error);
  }
}

async function startOrgRoute(route, refresh = false) {
  if (!refresh) {
    renderRemoteState(route, "resolving", [
      `authorizing ${route.org || route.login}`,
      `finding private org feeds on ${route.network}`,
      "waiting for settled story capsules",
    ]);
  }
  const org = route.org || route.login;
  const endpoint = `${edgeBaseUrl()}/resolve/github-org/${encodeURIComponent(org)}${resolverQuery(route)}`;
  try {
    logInfo("feed.org.resolver.request", {
      org,
      selection: route.selection,
      endpoint,
    });
    const response = await fetch(endpoint, {
      cache: "no-store",
      headers: githubAuthHeaders(),
    });
    logInfo("feed.org.resolver.response", {
      org,
      status: response.status,
      ok: response.ok,
    });
    if (response.status === 401 || response.status === 403) {
      renderAuthRequired(route);
      return;
    }
    if (response.status === 426) {
      const message = await responseErrorMessage(response, "update your peer to the latest version");
      renderRemoteState(route, "version-mismatch", [
        "feed protocol network or data model changed",
        message,
        "update your peer or choose the matching network",
      ]);
      return;
    }
    if (!response.ok) {
      throw new Error(`org resolver failed: ${response.status}`);
    }
    const ticket = await response.json();
    const ticketStatus = compatibilityStatus(ticket.compatibility || ticket.browser_seed?.compatibility);
    if (!ticketStatus.compatible) {
      renderRemoteState(route, "version-mismatch", [
        "feed protocol or data model changed",
        ticketStatus.message,
        "update your peer to the latest version",
      ]);
      return;
    }
    const feeds = ticketFeeds(ticket).filter((feed) => compatibilityStatus(feed.compatibility).compatible);
    const headlines = snapshotHeadlines(ticket)
      .filter((item) => compatibilityStatus(item.compatibility || ticket.compatibility).compatible)
      .filter((item) => headlineMatchesRoute(item, route));
    const classification = classifyRemoteSnapshot(route, feeds, headlines, {
      profile: { login: org, display_name: org },
    });
    if (classification.kind === "fresh_unseen" && !route.interactive) {
      updateSourceCountFromFeeds(feeds, headlines);
      applyRemoteHeadlines(route, headlines, classification);
      return;
    }
    if (feeds.length === 0 && headlines.length === 0) {
      renderRemoteState(route, "no-feeds", [
        "github org authorization accepted",
        "no private org story streams are visible",
      ], { login: org });
      updateSourceCountFromFeeds([]);
      return;
    }
    if (route.interactive) {
      renderTimeline(route, ticket);
      return;
    }
    renderQuietConnected(route, classification, { login: org });
  } catch (error) {
    renderRemoteState(route, "failed", [
      "org feed snapshot unavailable",
      "waiting for p2p live path",
    ]);
    logError("org route resolution failed", error);
  }
}

function ticketFeeds(ticket) {
  return ticket.feeds || ticket.candidate_feeds || [];
}

function snapshotFeeds(snapshot) {
  return snapshot.feeds || snapshot.candidate_feeds || snapshot.directory || [];
}

function snapshotHeadlines(snapshot) {
  return snapshot.headlines || snapshot.stories || snapshot.capsules || [];
}

function enrichHeadlineFromTicket(item, ticket) {
  if (!ticket?.profile) {
    return item;
  }
  return {
    ...item,
    publisher_login: item.publisher_login || item.github_login || ticket.profile.login,
    publisher_avatar: item.publisher_avatar || item.avatar || ticket.profile.avatar,
  };
}

function compatibilityStatus(remote) {
  if (!remote) {
    return {
      compatible: false,
      message: "compatibility metadata unavailable; update your peer to the latest version",
    };
  }
  const protocolVersion = Number(remote.protocol_version ?? remote.protocolVersion ?? 0);
  const modelVersion = Number(remote.model_version ?? remote.modelVersion ?? 0);
  const minModelVersion = Number(remote.min_model_version ?? remote.minModelVersion ?? 0);
  let message = "compatible";
  let compatible =
    protocolVersion === FEED_PROTOCOL_VERSION &&
    FEED_MODEL_VERSION >= minModelVersion &&
    modelVersion >= FEED_MIN_MODEL_VERSION;
  if (protocolVersion !== FEED_PROTOCOL_VERSION) {
    message = "protocol changed; update your peer to the latest version";
  } else if (minModelVersion > FEED_MODEL_VERSION) {
    message = "remote feed requires a newer data model; update your peer to the latest version";
  } else if (modelVersion < FEED_MIN_MODEL_VERSION) {
    message = "remote peer is using an older data model; ask the publisher to update";
  }
  return {
    compatible,
    message,
    protocolVersion,
    modelVersion,
    minModelVersion,
  };
}

function expectedNetworkId(route) {
  const network = String(route?.network || "mainnet").trim();
  return !network || network === "mainnet" ? "agent-feed-mainnet" : network;
}

function networkCompatibilityStatus(route, payload) {
  const expected = expectedNetworkId(route);
  const actual = String(payload?.network_id || payload?.networkId || "").trim();
  if (!actual) {
    return {
      compatible: false,
      expected,
      actual: "",
      message: "network metadata unavailable; update your peer to the latest version",
    };
  }
  if (actual === expected) {
    return {
      compatible: true,
      expected,
      actual,
      message: "compatible",
    };
  }
  return {
    compatible: false,
    expected,
    actual,
    message: `network mismatch; expected ${expected}, got ${actual}`,
  };
}

async function startFollowingRoute(route, refresh = false) {
  const targets = route.followingTargets.map(normalizeFollowTarget).filter(Boolean);
  const streamTargets = targets.filter((target) => !isTagFollowTarget(target));
  const tagTargets = targets.filter(isTagFollowTarget);
  logInfo("feed.following.selected", {
    network: route.network,
    targets,
    interactive: route.interactive,
    refresh,
  });
  if (!targets.length) {
    renderFollowingEmpty(route);
    return;
  }

  if (!refresh) {
    renderRemoteState(route, "resolving", [
      `checking ${followTargetCountLabel(targets.length)}`,
      "requesting accessible story snapshots",
      "showing settled story streams",
    ]);
  }
  const results = [
    ...(await Promise.all(streamTargets.map((target) => fetchFollowingTarget(route, target)))),
    ...(await fetchFollowingTagTargets(route, tagTargets)),
  ];
  const tickets = results.filter((result) => result.ticket).map((result) => result.ticket);
  const headlines = dedupeHeadlines(results.flatMap((result) => result.headlines))
    .sort((a, b) => Date.parse(b.created_at || b.createdAt || 0) - Date.parse(a.created_at || a.createdAt || 0));
  const feeds = tickets.flatMap(ticketFeeds);
  logInfo("feed.following.snapshot", {
    targets: targets.length,
    tickets: tickets.length,
    feeds: feeds.length,
    headlines: headlines.length,
    failures: results.filter((result) => result.error).length,
  });

  if (route.interactive) {
    renderFollowingTimeline(route, targets, results);
    return;
  }
  const classification = classifyRemoteSnapshot(route, feeds, headlines, { targets });
  logInfo("feed.following.classified", {
    classification: classification.kind,
    targets: targets.length,
    feeds: classification.feedCount,
    headlines: classification.headlineCount,
    fresh_unseen: classification.freshUnseenCount,
  });
  if (classification.kind === "fresh_unseen") {
    remoteFeedCount = targets.length;
    applyRemoteHeadlines(route, headlines, classification);
    return;
  }
  renderQuietConnected(route, { ...classification, feedCount: targets.length });
  setText(sourceCount, `${targets.length} following`);
}

async function fetchFollowingTagTargets(route, tagTargets) {
  if (!tagTargets.length) {
    return [];
  }
  const endpoint = `${edgeBaseUrl()}/network/snapshot${followingTagSnapshotQuery(route)}`;
  try {
    logInfo("feed.following.tags.request", { targets: tagTargets, endpoint });
    const response = await fetch(endpoint, {
      cache: "no-store",
      headers: githubAuthHeaders(),
    });
    logInfo("feed.following.tags.response", {
      status: response.status,
      ok: response.ok,
    });
    if (!response.ok) {
      if (response.status === 426) {
        const message = await responseErrorMessage(response, "update your peer to the latest version");
        return tagTargets.map((target) => ({ target, ticket: undefined, headlines: [], error: message }));
      }
      throw new Error(`following tag snapshot failed: ${response.status}`);
    }
    const snapshot = await response.json();
    const status = compatibilityStatus(snapshot.compatibility || snapshot.browser_seed?.compatibility);
    if (!status.compatible) {
      logWarn("feed.following.tags.version_mismatch", { message: status.message });
      return tagTargets.map((target) => ({ target, ticket: snapshot, headlines: [], error: status.message }));
    }
    const networkStatus = networkCompatibilityStatus(route, snapshot);
    if (!networkStatus.compatible) {
      logWarn("feed.following.tags.network_mismatch", {
        expected: networkStatus.expected,
        actual: networkStatus.actual,
      });
      return tagTargets.map((target) => ({ target, ticket: snapshot, headlines: [], error: networkStatus.message }));
    }
    const allHeadlines = snapshotHeadlines(snapshot)
      .filter((item) => compatibilityStatus(item.compatibility || snapshot.compatibility).compatible);
    return tagTargets.map((target) => ({
      target,
      ticket: snapshot,
      headlines: allHeadlines.filter((item) => headlineMatchesFollowTarget(item, target)),
      error: undefined,
    }));
  } catch (error) {
    logError("following tag snapshot failed", error);
    return tagTargets.map((target) => ({ target, ticket: undefined, headlines: [], error: String(error) }));
  }
}

async function fetchFollowingTarget(route, target) {
  const clean = normalizeFollowTarget(target);
  const [login, feed = "*"] = clean.split("/");
  const targetRoute = {
    ...route,
    kind: "user",
    login,
    feed,
    selection: feed === "*" ? `${login}/*` : `${login}/${feed}`,
  };
  const endpoint = `${edgeBaseUrl()}/resolve/github/${encodeURIComponent(login)}${followingResolverQuery(route, feed)}`;
  try {
    logInfo("feed.following.target.request", { target: clean, endpoint });
    const response = await fetch(endpoint, {
      cache: "no-store",
      headers: githubAuthHeaders(),
    });
    logInfo("feed.following.target.response", {
      target: clean,
      status: response.status,
      ok: response.ok,
    });
    if (!response.ok) {
      if (response.status === 404) {
        const fallbackTicket = await fetchDirectoryTicketForUser(targetRoute);
        if (fallbackTicket) {
          logWarn("feed.following.target.directory_fallback", {
            target: clean,
            feeds: ticketFeeds(fallbackTicket).length,
          });
          return {
            target: clean,
            ticket: fallbackTicket,
            headlines: snapshotHeadlines(fallbackTicket)
              .filter((item) => compatibilityStatus(item.compatibility || fallbackTicket.compatibility).compatible)
              .filter((item) => headlineMatchesRoute(item, targetRoute))
              .map((item) => enrichHeadlineFromTicket(item, fallbackTicket)),
            error: undefined,
          };
        }
      }
      if (response.status === 426) {
        const message = await responseErrorMessage(response, "update your peer to the latest version");
        return { target: clean, ticket: undefined, headlines: [], error: message };
      }
      throw new Error(`following target failed: ${response.status}`);
    }
    const ticket = await response.json();
    const status = compatibilityStatus(ticket.compatibility || ticket.browser_seed?.compatibility);
    if (!status.compatible) {
      logWarn("feed.following.target.version_mismatch", { target: clean, message: status.message });
      return { target: clean, ticket, headlines: [], error: status.message };
    }
    const networkStatus = networkCompatibilityStatus(route, ticket);
    if (!networkStatus.compatible) {
      logWarn("feed.following.target.network_mismatch", {
        target: clean,
        expected: networkStatus.expected,
        actual: networkStatus.actual,
      });
      return { target: clean, ticket, headlines: [], error: networkStatus.message };
    }
    return {
      target: clean,
      ticket,
      headlines: snapshotHeadlines(ticket)
        .filter((item) => compatibilityStatus(item.compatibility || ticket.compatibility).compatible)
        .filter((item) => headlineMatchesRoute(item, targetRoute))
        .map((item) => enrichHeadlineFromTicket(item, ticket)),
      error: undefined,
    };
  } catch (error) {
    logError("following target resolution failed", error);
    return { target: clean, ticket: undefined, headlines: [], error: String(error) };
  }
}

function followingTagSnapshotQuery(route) {
  const params = new URLSearchParams();
  params.set("network", route.network || "mainnet");
  params.set("feed_mode", "discovery");
  params.set("story_only", "true");
  params.set("settled_only", "true");
  copyReelFilterParams(route, params);
  const query = params.toString();
  return query ? `?${query}` : "";
}

function dedupeHeadlines(headlines) {
  const seen = new Set();
  const output = [];
  for (const item of headlines || []) {
    const key = item?.capsule_id || item?.id || `${item?.publisher_login || item?.github_login || ""}:${item?.headline || item?.title || ""}:${item?.created_at || item?.createdAt || ""}`;
    if (!item || !key || seen.has(key)) {
      continue;
    }
    seen.add(key);
    output.push(item);
  }
  return output;
}

function followTargetCountLabel(count) {
  return `${count} ${count === 1 ? "target" : "targets"}`;
}

async function fetchDirectoryTicketForUser(route) {
  const endpoint = `${edgeBaseUrl()}/network/snapshot${directoryFallbackQuery(route)}`;
  const expectedLogin = normalizeRouteLogin(route.login);
  try {
    logInfo("feed.directory.user_fallback.request", {
      login: route.login,
      endpoint,
    });
    const response = await fetch(endpoint, {
      cache: "no-store",
      headers: githubAuthHeaders(),
    });
    logInfo("feed.directory.user_fallback.response", {
      login: route.login,
      status: response.status,
      ok: response.ok,
    });
    if (!response.ok) {
      return undefined;
    }
    const snapshot = await response.json();
    const feeds = snapshotFeeds(snapshot).filter(
      (feed) => normalizeRouteLogin(publisherLoginForProfile(feed, { profile: {} })) === expectedLogin,
    );
    const fallbackRoute = {
      ...route,
      kind: "user",
      login: route.login,
      feed: route.feed || "*",
      selection: route.selection || `${route.login}/${route.feed || "*"}`,
    };
    const baseProfileTicket = directoryProfileTicket(route, feeds, [], snapshot);
    const headlines = snapshotHeadlines(snapshot)
      .filter((item) => headlineMatchesRoute(item, fallbackRoute))
      .map((item) => enrichHeadlineFromTicket(item, baseProfileTicket));
    if (!feeds.length && !headlines.length) {
      return undefined;
    }
    const profileTicket = directoryProfileTicket(route, feeds, headlines, snapshot);
    return {
      ...snapshot,
      requested_login: route.login,
      resolved_github_id: profileTicket.resolved_github_id,
      github_user_id: profileTicket.github_user_id,
      profile: profileTicket.profile,
      feeds,
      candidate_feeds: feeds,
      headlines,
    };
  } catch (error) {
    logError("directory user fallback failed", error);
    return undefined;
  }
}

function directoryFallbackQuery(route) {
  const params = new URLSearchParams();
  params.set("network", route.network || "mainnet");
  params.set("feed_mode", "discovery");
  params.set("story_only", "true");
  params.set("settled_only", "true");
  copyReelFilterParams(route, params);
  return `?${params.toString()}`;
}

function directoryProfileTicket(route, feeds, headlines, snapshot) {
  const source = feeds[0] || headlines[0] || {};
  const githubId =
    source.publisher_github_user_id ||
    source.github_user_id ||
    source.publisher?.github_user_id ||
    source.owner?.github_user_id ||
    "";
  const login =
    publisherLoginForProfile(source, { profile: {} }) ||
    publisherLoginFromHeadline(source) ||
    route.login;
  const avatar =
    source.publisher_avatar ||
    source.avatar ||
    source.publisher?.avatar ||
    source.owner?.avatar?.url ||
    source.owner?.avatar_url ||
    "";
  const displayName =
    source.publisher_display_name ||
    source.display_name ||
    source.publisher?.display_name ||
    source.owner?.display_name ||
    login;
  return {
    compatibility: snapshot.compatibility,
    browser_seed: snapshot.browser_seed,
    github_user_id: githubId,
    resolved_github_id: githubId,
    profile: {
      login,
      name: displayName,
      avatar,
    },
  };
}

function followingResolverQuery(route, feed) {
  const params = new URLSearchParams();
  params.set("network", route.network || "mainnet");
  params.set("story_only", "true");
  params.set("settled_only", "true");
  copyReelFilterParams(route, params);
  if (feed === "*") {
    params.set("streams", "all");
  } else if (feed) {
    params.set("streams", feed);
  }
  return `?${params.toString()}`;
}

function copyReelFilterParams(route, params) {
  const source = new URLSearchParams(route.query || "");
  for (const key of ["agents", "kinds", "projects", "project", "min_score"]) {
    const value = source.get(key);
    if (value) {
      params.set(key, value);
    }
  }
}

function networkDiscoveryQuery(route) {
  const params = new URLSearchParams(route.query);
  params.set("network", route.network || "mainnet");
  params.set("feed_mode", "discovery");
  params.set("story_only", "true");
  params.set("settled_only", "true");
  for (const key of ["redact", "raw", "raw_events", "diffs", "prompts"]) {
    params.delete(key);
  }
  const query = params.toString();
  return query ? `?${query}` : "";
}

function resolverQuery(route) {
  const params = new URLSearchParams(route.query);
  if (route.feed && !params.has("streams") && !params.has("all")) {
    if (route.feed === "*") {
      params.set("streams", "all");
    } else {
      params.set("streams", route.feed);
    }
  }
  const query = params.toString();
  return query ? `?${query}` : "";
}

function classifyRemoteSnapshot(route, feeds = [], headlines = [], context = {}) {
  const routeHeadlines = (headlines || []).filter((item) => headlineMatchesRoute(item, route));
  const feedList = Array.isArray(feeds) ? feeds : [];
  const stageBulletins = routeHeadlines
    .map((item, index) => remoteHeadlineToBulletin(route, item, index))
    .filter(Boolean);
  const latestBulletin = latestBulletinByTime(stageBulletins);
  const freshUnseen = stageBulletins.filter(
    (item) => !seenBulletinIds.has(bulletinId(item)) && bulletinIsFreshForStage(item),
  );
  const allSeen =
    stageBulletins.length > 0 &&
    stageBulletins.every((item) => seenBulletinIds.has(bulletinId(item)));
  const allStale =
    stageBulletins.length > 0 &&
    stageBulletins.every((item) => !bulletinIsFreshForStage(item));
  const kind = freshUnseen.length
    ? "fresh_unseen"
    : allSeen
      ? "all_seen"
      : allStale
        ? "all_stale"
        : routeHeadlines.length
          ? "seen_or_stale_available"
          : feedList.length
            ? "feeds_only"
            : "nothing_visible";
  return {
    kind,
    feeds: feedList,
    feedCount: logicalFeedCount(feedList, routeHeadlines),
    headlines: routeHeadlines,
    headlineCount: routeHeadlines.length,
    latestBulletin,
    freshBulletins: freshUnseen,
    freshUnseenCount: freshUnseen.length,
    historyAvailable: routeHeadlines.length > 0 || feedList.length > 0,
    lastStoryAt: latestTimeValue(routeHeadlines, ["created_at", "createdAt", "published_at", "publishedAt"]),
    lastSeenAt: latestTimeValue(feedList, ["last_seen_at", "lastSeenAt", "updated_at", "updatedAt"]),
    profile: context.profile,
    targets: context.targets || [],
  };
}

function latestBulletinByTime(items) {
  let latest = undefined;
  let latestTime = 0;
  for (const item of items || []) {
    const timestamp = bulletinCreatedAtMs(item);
    if (!latest || timestamp > latestTime) {
      latest = item;
      latestTime = timestamp;
    }
  }
  return latest;
}

function latestTimeValue(items, keys) {
  let latest = 0;
  for (const item of items || []) {
    for (const key of keys) {
      const value = item?.[key];
      const timestamp = Date.parse(value || "");
      if (Number.isFinite(timestamp) && timestamp > latest) {
        latest = timestamp;
      }
    }
  }
  return latest > 0 ? new Date(latest).toISOString() : "";
}

function applyRemoteHeadlines(route, headlines, classification = undefined) {
  const state = classification || classifyRemoteSnapshot(route, [], headlines);
  updateSourceCountFromClassification(route, state);
  const nextHeadlines = state.headlines || headlines || [];
  const nextSignature = remoteSnapshotSignature(nextHeadlines);
  if (nextSignature && nextSignature === remoteHeadlinesSignature && bulletins.length) {
    logDebug("feed.network.discovery.headlines.unchanged", {
      signature: nextSignature,
      headlines: nextHeadlines.length,
    });
    return;
  }
  remoteHeadlinesSignature = nextSignature;
  const nextBulletins = Array.isArray(state.freshBulletins)
    ? state.freshBulletins
    : nextHeadlines
      .map((item, index) => remoteHeadlineToBulletin(route, item, index))
      .filter(Boolean);
  const result = applyBulletinQueueUpdate(nextBulletins, "remote-snapshot");
  if (!bulletins.length) {
    renderQuietConnected(route, state, state.profile);
    return;
  }
  updateSourceCount();
  logInfo("feed.network.discovery.headlines.render", {
    classification: state.kind,
    headlines: bulletins.length,
    rendered: result.rendered,
    preserved: result.preserved,
    interrupted: result.interrupted,
  });
}

function remoteSnapshotSignature(headlines) {
  return (headlines || [])
    .map((item) => [
      item.capsule_id || item.id || "",
      item.created_at || item.createdAt || item.published_at || item.publishedAt || "",
      item.headline || item.title || "",
    ].join(":"))
    .join("|");
}

function remoteHeadlineToBulletin(route, item, index) {
  const headlineText = item.headline || item.title;
  if (!headlineText) {
    return undefined;
  }
  const publisherLogin = publisherLoginFromHeadline(item);
  return {
    id: item.capsule_id || item.id || `network-headline-${index}`,
    mode: item.mode || "dispatch",
    priority: item.priority || item.score || 75,
    dwell_ms: item.dwell_ms || item.dwellMs || 14000,
    source_key:
      item.feed_id ||
      `${publisherLogin || route.login || "network"}/${item.feed_label || item.label || route.feed || "*"}`,
    feed_label: item.feed_label || item.label || route.feed || "",
    created_at: item.created_at || item.createdAt || item.published_at || item.publishedAt,
    eyebrow: routeEyebrow(route),
    headline: headlineText,
    deck: item.deck || item.summary || "settled story capsule published to the network.",
    lower_third: item.lower_third || item.lowerThird || item.publisher_label || "verified peer",
    chips: item.chips || ["verified", "story-only", route.network, "redacted"],
    ticker: item.ticker || [],
    image: item.image || item.headline_image,
    publisher: publisherLogin || item.publisher_avatar
      ? {
          github_login: publisherLogin,
          avatar: item.publisher_avatar || item.avatar,
        }
      : undefined,
  };
}

function headlineMatchesRoute(item, route) {
  if (!item || !route) {
    return false;
  }
  if (route.kind !== "global" && route.kind !== "org") {
    const expectedLogin = normalizeRouteLogin(route.login);
    const publisherLogin = normalizeRouteLogin(publisherLoginFromHeadline(item));
    if (expectedLogin && publisherLogin && expectedLogin !== publisherLogin) {
      return false;
    }
  }
  const requestedFeeds = requestedFeedLabels(route);
  if (requestedFeeds.length) {
    const headlineFeeds = headlineFeedLabels(item);
    if (!requestedFeeds.some((feed) => headlineFeeds.includes(feed))) {
      return false;
    }
  }
  return headlineMatchesReelFilters(item, route);
}

function headlineMatchesFollowTarget(item, target) {
  const clean = normalizeFollowTarget(target);
  if (!clean) {
    return false;
  }
  if (isTagFollowTarget(clean)) {
    return headlineTagTerms(item).includes(normalizeTag(clean));
  }
  const [login, feed = "*"] = clean.split("/");
  const publisherLogin = normalizeRouteLogin(publisherLoginFromHeadline(item));
  if (normalizeRouteLogin(login) !== publisherLogin) {
    return false;
  }
  if (feed === "*") {
    return true;
  }
  return headlineFeedLabels(item).includes(normalizeFeedLabel(feed));
}

function headlineMatchesReelFilters(item, route) {
  const params = new URLSearchParams(route.query || "");
  const score = headlineScore(item);
  const minScore = Number(params.get("min_score") || "0") || 0;
  if (minScore > 0 && score > 0 && score < minScore) {
    return false;
  }
  const terms = headlineTagTerms(item);
  return (
    requestedTagsMatch(requestedCsvParams(params, ["agents"]), terms) &&
    requestedTagsMatch(requestedCsvParams(params, ["kinds"]), terms) &&
    requestedTagsMatch(requestedCsvParams(params, ["projects", "project"]), terms)
  );
}

function requestedCsvParams(params, keys) {
  for (const key of keys) {
    const value = params.get(key);
    if (value) {
      return value
        .split(",")
        .map(normalizeTag)
        .filter(Boolean);
    }
  }
  return [];
}

function requestedTagsMatch(requested, terms) {
  return !requested.length || requested.some((value) => terms.includes(value));
}

function headlineTagTerms(item) {
  const values = [
    item.agent,
    item.agent_kind,
    item.kind,
    item.story_kind,
    item.project,
    item.project_tag,
    item.feed_label,
    item.label,
    ...(item.chips || []),
    ...String(item.lower_third || item.lowerThird || "")
      .split(/[\/·,|]/)
      .map((value) => value.trim()),
  ];
  return [...new Set(values.map(normalizeTag).filter(Boolean))];
}

function headlineScore(item) {
  return Number(item.score || item.priority || item.score_hint || item.scoreHint || 0) || 0;
}

function normalizeTag(value) {
  return String(value || "")
    .trim()
    .replace(/^@/, "")
    .toLowerCase()
    .replace(/[^a-z0-9]+/g, "_")
    .replace(/^_+|_+$/g, "");
}

function normalizeRouteLogin(value) {
  return String(value || "").replace(/^@/, "").toLowerCase();
}

function requestedFeedLabels(route) {
  if (route.feed && route.feed !== "*") {
    return [normalizeFeedLabel(route.feed)];
  }
  const params = new URLSearchParams(route.query || "");
  const streams = params.get("streams") || "";
  if (!streams || matchesWildcardStream(streams, route.login)) {
    return [];
  }
  return streams
    .split(",")
    .map((value) => value.trim().replace(/^@/, ""))
    .map((value) => (value.includes("/") ? value.split("/").pop() : value))
    .map(normalizeFeedLabel)
    .filter(Boolean);
}

function matchesWildcardStream(value, login) {
  const clean = String(value || "").trim().replace(/^@/, "");
  return (
    clean === "all" ||
    clean === "*" ||
    clean === `${String(login || "").replace(/^@/, "")}/*`
  );
}

function headlineFeedLabels(item) {
  const labels = [
    item.feed_label,
    item.label,
    item.stream_label,
    item.stream_id,
    item.feed?.label,
  ];
  const feedId = String(item.feed_id || item.feedId || "");
  if (feedId.includes(":")) {
    labels.push(feedId.split(":").pop());
  }
  return labels.map(normalizeFeedLabel).filter(Boolean);
}

function normalizeFeedLabel(value) {
  return String(value || "").trim().toLowerCase();
}

function timelineMetaText(item, fallback = "feed") {
  const project = primaryProjectTag(item);
  const feed = item.feed_label || item.label || item.stream_label || "";
  const when = item.created_at || item.createdAt || item.published_at || item.publishedAt;
  return uniqueTextParts([project, feed, when ? relativeTime(when) : ""]).join(" · ") || fallback;
}

function primaryProjectTag(item) {
  if (item.project || item.project_tag) {
    return item.project || item.project_tag;
  }
  const reserved = new Set([
    "codex",
    "claude",
    "verified",
    "story_only",
    "redacted",
    "score",
    "turn",
    "plan",
    "command",
    "mcp",
    "recap",
    "idle",
    "incident",
    "permission",
    "file_change",
    "test",
    "tests",
    "pass",
    "fail",
    "failed",
    "publish",
    "edge",
    "local",
    "network",
  ]);
  const feed = normalizeTag(item.feed_label || item.label || "");
  return (
    (item.chips || []).find((chip) => {
      const value = normalizeTag(chip);
      return value && value !== feed && !reserved.has(value) && !value.startsWith("score_");
    }) || ""
  );
}

function uniqueTextParts(parts) {
  const seen = new Set();
  return parts
    .map((part) => String(part || "").trim())
    .filter((part) => {
      const key = part.toLowerCase();
      if (!key || seen.has(key)) {
        return false;
      }
      seen.add(key);
      return true;
    });
}

function publisherLoginFromHeadline(item) {
  const value =
    item.publisher_login ||
    item.github_login ||
    item.publisher?.github_login ||
    item.publisher_label ||
    "";
  return String(value).replace(/^@/, "").split(/\s|\//)[0] || "";
}

function renderGlobalTimeline(route, snapshot) {
  if (!timeline) {
    renderRemoteState(route, "failed", ["history surface unavailable"]);
    return;
  }
  const feeds = snapshotFeeds(snapshot);
  const headlines = snapshotHeadlines(snapshot).filter((item) => headlineMatchesRoute(item, route));
  logInfo("feed.network.history.render", {
    network: route.network,
    feeds: feeds.length,
    headlines: headlines.length,
  });
  if (stage) {
    stage.hidden = true;
  }
  stopStageProgress();
  clearStoryTime();
  timeline.hidden = false;
  document.body.dataset.view = "history";
  document.body.dataset.mode = "dispatch";
  setText(liveState, "browse");
  updateSourceCountFromFeeds(feeds, headlines);
  timeline.replaceChildren();

  const toolbar = document.createElement("div");
  toolbar.className = "timeline-toolbar";
  const label = document.createElement("span");
  label.textContent = `network discovery / ${route.network}`;
  toolbar.appendChild(label);
  const nav = document.createElement("nav");
  nav.className = "timeline-feeds";
  nav.appendChild(timelineModeLink(route, "discovery", "all public feeds", true));
  nav.appendChild(timelineModeLink(route, "following", "following", false));
  toolbar.appendChild(nav);
  timeline.appendChild(toolbar);

  if (headlines.length) {
    for (const item of headlines) {
      const card = document.createElement("article");
      card.className = "timeline-card";
      card.tabIndex = 0;
      card.appendChild(timelinePublisher(item, { profile: {} }, route));
      const meta = document.createElement("div");
      meta.className = "timeline-meta";
      meta.textContent = timelineMetaText(item, "network headline");
      const title = document.createElement("h2");
      title.textContent = item.headline || item.title || "settled story";
      const copy = document.createElement("p");
      copy.textContent = item.deck || item.summary || "story-only capsule";
      card.append(meta, title, copy, timelineActions(item, route));
      timeline.appendChild(card);
    }
  } else {
    for (const feed of feeds) {
      const card = document.createElement("article");
      card.className = "timeline-card";
      card.tabIndex = 0;
      card.appendChild(timelinePublisher(feed, { profile: {} }, route));
      const meta = document.createElement("div");
      meta.className = "timeline-meta";
      meta.textContent = feed.label || feed.feed_label || "feed";
      const title = document.createElement("h2");
      title.textContent = "waiting for published headline";
      const copy = document.createElement("p");
      copy.textContent = "this feed is visible on the network. settled stories will appear as they publish.";
      card.append(meta, title, copy, timelineStatus(feed, route), timelineActions(feed, route));
      timeline.appendChild(card);
    }
  }
  if (timeline.children.length === 1) {
    const card = document.createElement("article");
    card.className = "timeline-card timeline-empty";
    card.tabIndex = 0;
    const meta = document.createElement("div");
    meta.className = "timeline-meta";
    meta.textContent = "network discovery";
    const title = document.createElement("h2");
    title.textContent = "no visible feeds";
    const copy = document.createElement("p");
    copy.textContent = "network peers may still be routing without publishing accessible feeds.";
    card.append(meta, title, copy);
    timeline.appendChild(card);
  }
  renderTicker(["history · network discovery", "followed feeds stay explicit"]);
}

function timelineModeLink(route, mode, label, current = false) {
  const link = document.createElement("a");
  link.href = modeUrl(route, mode);
  link.textContent = label;
  link.dataset.kind = "mode";
  if (current) {
    link.setAttribute("aria-current", "page");
  }
  return link;
}

function userDiscoveryUrl(login, route = undefined) {
  if (!login || !/^[A-Za-z0-9](?:[A-Za-z0-9-]{0,37}[A-Za-z0-9])?$/.test(login)) {
    return "";
  }
  const params = new URLSearchParams();
  params.set("feed_mode", "discovery");
  if (route?.network && route.network !== "mainnet") {
    params.set("network", route.network);
  }
  if (
    route?.interactive ||
    ["history", "timeline"].includes(new URLSearchParams(window.location.search).get("view") || "")
  ) {
    params.set("view", "history");
  }
  const query = params.toString();
  return `/${encodeURIComponent(login)}/*${query ? `?${query}` : ""}`;
}

function renderTimeline(route, ticket) {
  if (!timeline) {
    renderRemoteState(route, "failed", ["history surface unavailable"]);
    return;
  }
  const feeds = ticket.feeds || ticket.candidate_feeds || [];
  const headlines = snapshotHeadlines(ticket)
    .filter((item) => headlineMatchesRoute(item, route))
    .map((item) => enrichHeadlineFromTicket(item, ticket));
  logInfo("feed.history.render", {
    login: ticket.profile?.login || route.login,
    selection: route.selection,
    feeds: feeds.length,
    headlines: headlines.length,
    wildcard: route.feed === "*" || !route.feed,
  });
  if (stage) {
    stage.hidden = true;
  }
  stopStageProgress();
  clearStoryTime();
  timeline.hidden = false;
  document.body.dataset.view = "history";
  document.body.dataset.mode = "dispatch";
  setText(liveState, "browse");
  updateSourceCountFromFeeds(feeds, headlines);
  timeline.replaceChildren();

  const toolbar = document.createElement("div");
  toolbar.className = "timeline-toolbar";
  const label = document.createElement("span");
  label.textContent = `@${ticket.profile?.login || route.login} / ${routeStreamLabel(route)}`;
  toolbar.appendChild(label);
  const nav = document.createElement("nav");
  nav.className = "timeline-feeds";
  nav.appendChild(
    feedLink(route.login, "*", "all feeds", !route.feed || route.feed === "*"),
  );
  for (const feed of feeds) {
    const feedLabel = feed.label || feed.feed_label || "feed";
    nav.appendChild(feedLink(route.login, feedLabel, feedLabel, route.feed === feedLabel));
  }
  const follow = toolbarFollowButton(route);
  if (follow) {
    nav.appendChild(follow);
  }
  nav.appendChild(timelineModeLink(route, "following", "following", false));
  toolbar.appendChild(nav);
  timeline.appendChild(toolbar);

  for (const item of headlines) {
    const card = document.createElement("article");
    card.className = "timeline-card";
    card.tabIndex = 0;
    const meta = document.createElement("div");
    meta.className = "timeline-meta";
    meta.textContent = timelineMetaText(item, item.lower_third || "feed");
    const title = document.createElement("h2");
    title.textContent = item.headline || item.title || "settled story";
    const copy = document.createElement("p");
    copy.textContent = item.deck || item.summary || "story-only capsule";
    card.append(timelinePublisher(item, ticket, route), meta, title, copy, timelineActions(item, route));
    timeline.appendChild(card);
  }

  for (const feed of feeds) {
    const feedLabel = feed.label || feed.feed_label || "feed";
    if (route.feed && route.feed !== "*" && route.feed !== feedLabel) {
      continue;
    }
    const card = document.createElement("article");
    card.className = "timeline-card";
    card.tabIndex = 0;
    card.appendChild(timelinePublisher(feed, ticket, route));
    const meta = document.createElement("div");
    meta.className = "timeline-meta";
    meta.textContent = feedLabel;
    const title = document.createElement("h2");
    title.textContent = `waiting for ${feedLabel}`;
    const copy = document.createElement("p");
    copy.textContent = "settled stories will appear here as a vertical feed.";
    card.append(meta, title, copy, timelineStatus(feed, route), timelineActions(feed, route));
    timeline.appendChild(card);
  }
  if (timeline.children.length === 1) {
    const card = document.createElement("article");
    card.className = "timeline-card timeline-empty";
    card.tabIndex = 0;
    const meta = document.createElement("div");
    meta.className = "timeline-meta";
    meta.textContent = `@${route.login} / ${routeStreamLabel(route)}`;
    const title = document.createElement("h2");
    title.textContent = "no matching feed";
    const copy = document.createElement("p");
    copy.textContent = "the selected logical feed is not visible from this session.";
    card.append(meta, title, copy);
    timeline.appendChild(card);
  }
  renderTicker([`history · ${routeStreamLabel(route)}`, "projection mode remains automatic"]);
}

function renderFollowingEmpty(route) {
  showStage();
  document.body.dataset.mode = "dispatch";
  setText(liveState, "browse");
  setText(eyebrow, `feed / ${route.network} / following`);
  setText(headline, "nothing followed yet");
  setText(
    deck,
    "open discovery and follow a person. feed and tag follows can narrow the view later.",
  );
  renderPublisher(undefined);
  renderHeadlineImage(undefined);
  clearAuthAction();
  clearStageActions();
  renderChips(["following", "local filter", route.network, "story-only"]);
  renderTicker(["discovery finds people", "following filters the projection"]);
  stopStageProgress();
  if (route.interactive) {
    renderFollowingTimeline(route, [], []);
  }
}

function renderFollowingTimeline(route, targets, results) {
  if (!timeline) {
    renderRemoteState(route, "failed", ["history surface unavailable"]);
    return;
  }
  logInfo("feed.following.history.render", {
    network: route.network,
    targets,
  });
  if (stage) {
    stage.hidden = true;
  }
  stopStageProgress();
  clearStoryTime();
  timeline.hidden = false;
  document.body.dataset.view = "history";
  document.body.dataset.mode = "dispatch";
  setText(liveState, "follow");
  setText(sourceCount, `${targets.length} following`);
  timeline.replaceChildren();

  const toolbar = document.createElement("div");
  toolbar.className = "timeline-toolbar";
  const label = document.createElement("span");
  label.textContent = route.kind === "user"
    ? `following @${route.login} / ${route.network}`
    : `following / ${route.network}`;
  toolbar.appendChild(label);
  const nav = document.createElement("nav");
  nav.className = "timeline-feeds";
  nav.appendChild(timelineModeLink(route, "following", "following", true));
  nav.appendChild(timelineModeLink(route, "discovery", route.kind === "user" ? "feeds" : "discover", false));
  for (const target of targets) {
    const link = document.createElement("a");
    link.href = followingTargetUrl(target);
    link.textContent = followTargetLabel(target);
    nav.appendChild(link);
  }
  toolbar.appendChild(nav);
  timeline.appendChild(toolbar);

  for (const result of results) {
    for (const item of result.headlines || []) {
      const card = document.createElement("article");
      card.className = "timeline-card";
      card.tabIndex = 0;
      const meta = document.createElement("div");
      meta.className = "timeline-meta";
      meta.textContent = timelineMetaText(item, item.lower_third || `${result.target} / following`);
      const title = document.createElement("h2");
      title.textContent = item.headline || item.title || "settled story";
      const copy = document.createElement("p");
      copy.textContent = item.deck || item.summary || "story-only capsule";
      card.append(timelinePublisher(item, { profile: {} }, route), meta, title, copy, timelineActions(item, route));
      timeline.appendChild(card);
    }
  }

  for (const target of targets) {
    const hasHeadline = results.some((result) => result.target === target && result.headlines?.length);
    if (hasHeadline) {
      continue;
    }
    const card = document.createElement("article");
    card.className = "timeline-card";
    card.tabIndex = 0;
    const meta = document.createElement("div");
    meta.className = "timeline-meta";
    meta.textContent = `${followTargetLabel(target)} / following`;
    const title = document.createElement("h2");
    title.textContent = `waiting for ${followTargetLabel(target)}`;
    const copy = document.createElement("p");
    copy.textContent =
      "only followed settled story capsules appear here. discovery results are not mixed in.";
    const status = document.createElement("div");
    status.className = "timeline-status";
    status.append(statusItem("target", followTargetLabel(target)), statusItem("mode", "following only"));
    card.append(meta, title, copy, status, timelineActions(followPlaceholderItem(target), route));
    timeline.appendChild(card);
  }
  if (!targets.length) {
    const card = document.createElement("article");
    card.className = "timeline-card timeline-empty";
    card.tabIndex = 0;
    const meta = document.createElement("div");
    meta.className = "timeline-meta";
    meta.textContent = "following";
    const title = document.createElement("h2");
    title.textContent = "nothing followed yet";
    const copy = document.createElement("p");
    copy.textContent =
      "use discovery to follow people, feeds, or tags. private feed access is handled separately.";
    card.append(meta, title, copy);
    timeline.appendChild(card);
  }
  renderTicker(["history · following", "tap or move pointer for controls"]);
}

function followingTargetUrl(target) {
  const clean = normalizeFollowTarget(target);
  if (isTagFollowTarget(clean)) {
    const params = new URLSearchParams();
    params.set("feed_mode", "following");
    params.set("view", "history");
    params.set("following", clean);
    return `/?${params.toString()}`;
  }
  const [login, feed = "*"] = clean.split("/");
  return `/${encodeURIComponent(login)}/${encodeURIComponent(feed)}?feed_mode=following&view=history`;
}

function userFeedPath(login, label) {
  const cleanLogin = encodeURIComponent(login);
  if (!label || label === "*") {
    return `/${cleanLogin}/*`;
  }
  return `/${cleanLogin}/${encodeURIComponent(label)}`;
}

function toolbarFollowButton(route) {
  if (!route || route.kind !== "user") {
    return undefined;
  }
  const feed = route.feed && route.feed !== "*" ? route.feed : "*";
  const target = normalizeFollowTarget(`${route.login}/${feed}`);
  if (!target) {
    return undefined;
  }
  const button = followButton(target, {
    inactive: `follow ${followTargetLabel(target)}`,
    active: `following ${followTargetLabel(target)}`,
  });
  button.dataset.kind = "follow";
  return button;
}

function timelineActions(feed, route) {
  const actions = document.createElement("div");
  actions.className = "timeline-actions";
  const project = primaryProjectTag(feed);
  if (project) {
    actions.appendChild(projectFilterLink(project, route));
    actions.appendChild(tagFollowButton(project));
  }
  const personTarget = personFollowTargetFor(feed, route);
  const target = followTargetFor(feed, route);
  const primaryTarget = primaryFollowTargetForTargets(personTarget, target, route);
  if (primaryTarget) {
    actions.appendChild(
      followButton(primaryTarget, {
        inactive: `follow ${followTargetLabel(primaryTarget)}`,
        active: `following ${followTargetLabel(primaryTarget)}`,
      }),
    );
  }
  const visibility = String(feed.visibility || "").toLowerCase();
  if (visibility && visibility !== "public") {
    actions.appendChild(privateFeedPill(visibility));
  }
  return actions;
}

function tagFollowButton(project) {
  return followButton(`#${normalizeTag(project)}`, {
    inactive: `follow #${normalizeTag(project)}`,
    active: `following #${normalizeTag(project)}`,
  });
}

function projectFilterLink(project, route) {
  const link = document.createElement("a");
  link.className = "feed-action";
  link.href = projectFilterUrl(project, route);
  link.textContent = normalizeTag(project) === normalizeTag(routeProjectFilter(route))
    ? project
    : `project ${project}`;
  link.setAttribute("aria-label", `filter history to project ${project}`);
  return link;
}

function projectFilterUrl(project, route) {
  const params = new URLSearchParams(route.query || "");
  params.set("projects", project);
  params.delete("project");
  params.set("view", "history");
  for (const key of ["redact", "raw", "raw_events", "diffs", "prompts"]) {
    params.delete(key);
  }
  const query = params.toString();
  return `${routePath(route)}${query ? `?${query}` : ""}`;
}

function routeProjectFilter(route) {
  const params = new URLSearchParams(route.query || "");
  return params.get("projects") || params.get("project") || "";
}

function routePath(route) {
  if (!route || route.kind === "global") {
    return "/";
  }
  if (route.kind === "org") {
    return `/org/${encodeURIComponent(route.org || route.login)}`;
  }
  const login = route.login ? encodeURIComponent(route.login) : "";
  if (!login) {
    return "/";
  }
  if (route.feed) {
    return `/${login}/${encodeURIComponent(route.feed)}`;
  }
  return `/${login}`;
}

function followTargetFor(feed, route) {
  const login =
    publisherLoginForProfile(feed, { profile: { login: route.login } }) ||
    route.login ||
    "";
  if (!login) {
    return "";
  }
  const label = feed.feed_label || feed.label || route.feed || "*";
  return normalizeFollowTarget(`${login}/${label || "*"}`);
}

function personFollowTargetFor(feed, route) {
  const login =
    publisherLoginForProfile(feed, { profile: { login: route.login } }) ||
    route.login ||
    "";
  return login ? normalizeFollowTarget(`${login}/*`) : "";
}

function primaryFollowTargetForTargets(personTarget, feedTarget, route = remoteRoute) {
  const normalizedPerson = normalizeFollowTarget(personTarget);
  const normalizedFeed = normalizeFollowTarget(feedTarget);
  const routeFeed = route?.feed && route.feed !== "*" ? normalizeFeedLabel(route.feed) : "";
  if (routeFeed && normalizedFeed && !normalizedFeed.endsWith("/*")) {
    return normalizedFeed;
  }
  return normalizedPerson || normalizedFeed;
}

function followTargetForBulletin(bulletin) {
  const login = publisherLoginFromHeadline(bulletin);
  if (!login) {
    return "";
  }
  const label = bulletinFeedLabel(bulletin);
  return normalizeFollowTarget(`${login}/${label || "*"}`);
}

function personFollowTargetForBulletin(bulletin) {
  const login = publisherLoginFromHeadline(bulletin);
  return login ? normalizeFollowTarget(`${login}/*`) : "";
}

function followPlaceholderItem(target) {
  const clean = normalizeFollowTarget(target);
  if (isTagFollowTarget(clean)) {
    return { chips: [clean], project_tag: clean.replace(/^#/, "") };
  }
  const [login, feed = "*"] = clean.split("/");
  return { feed_label: feed, publisher_login: login };
}

function bulletinFeedLabel(bulletin) {
  const direct =
    bulletin?.feed_label ||
    bulletin?.feedLabel ||
    bulletin?.label ||
    bulletin?.stream_label ||
    bulletin?.streamLabel ||
    "";
  if (direct) {
    return direct;
  }
  const sourceKey = String(bulletin?.source_key || bulletin?.sourceKey || "");
  if (sourceKey.includes("/")) {
    return sourceKey.split("/").pop() || "*";
  }
  return remoteRoute?.feed || "*";
}

function followButton(target, labels = {}) {
  const button = document.createElement("button");
  button.type = "button";
  button.className = "feed-action";
  button.dataset.followTarget = normalizeFollowTarget(target);
  const applyState = () => {
    const active = isFollowingTarget(target);
    button.textContent = active
      ? labels.active || `following ${followTargetLabel(target)}`
      : labels.inactive || `follow ${followTargetLabel(target)}`;
    button.setAttribute("aria-pressed", active ? "true" : "false");
    button.setAttribute("aria-label", `${active ? "unfollow" : "follow"} ${followTargetLabel(target)}`);
  };
  applyState();
  button.addEventListener("click", () => {
    const next = toggleFollowTarget(target);
    refreshFollowButtons();
    refreshAfterFollowingChange();
    logInfo("feed.following.toggle", { target, following: next });
  });
  return button;
}

function refreshFollowButtons() {
  for (const button of document.querySelectorAll("[data-follow-target]")) {
    const target = button.dataset.followTarget || "";
    const active = isFollowingTarget(target);
    button.setAttribute("aria-pressed", active ? "true" : "false");
    button.setAttribute("aria-label", `${active ? "unfollow" : "follow"} ${followTargetLabel(target)}`);
    if (button.textContent?.startsWith("follow")) {
      button.textContent = active ? `following ${followTargetLabel(target)}` : `follow ${followTargetLabel(target)}`;
    }
  }
}

function refreshAfterFollowingChange() {
  if (!remoteRoute || !p2pEnabled()) {
    return;
  }
  const params = new URLSearchParams(
    window.location.hash && window.location.hash.length > 1
      ? window.location.hash.slice(1)
      : window.location.search,
  );
  remoteRoute.followingTargets = routeFollowingTargets(remoteRoute.login, params);
  if (remoteRoute.feedMode === "following") {
    window.clearTimeout(remoteRefreshTimer);
    refreshRemoteRoute(remoteRoute, "following-toggle");
  }
}

function privateFeedPill(visibility) {
  const pill = document.createElement("span");
  pill.className = "feed-action feed-action-muted";
  pill.textContent = `${visibility || "private"} · grant required`;
  pill.setAttribute("aria-disabled", "true");
  return pill;
}

function feedLink(login, label, text, current = false) {
  const link = document.createElement("a");
  link.href = `${userFeedPath(login, label)}?view=history`;
  link.textContent = text;
  link.dataset.kind = "feed";
  if (current) {
    link.setAttribute("aria-current", "page");
  }
  return link;
}

function timelinePublisher(feed, ticket, route = undefined) {
  const login = publisherLoginForProfile(feed, ticket);
  const profileUrl = userDiscoveryUrl(login, route);
  const node = document.createElement(profileUrl ? "a" : "div");
  node.className = "publisher";
  if (profileUrl) {
    node.href = profileUrl;
    node.setAttribute("aria-label", `open @${login} discovery feed`);
  }
  const img = document.createElement("img");
  img.alt = "";
  img.loading = "lazy";
  img.decoding = "async";
  img.referrerPolicy = "no-referrer";
  const avatar = safeAvatarUrl(
    feed.publisher_avatar ||
      feed.avatar ||
      feed.owner?.avatar?.url ||
      feed.owner?.avatar_url ||
      ticket.profile?.avatar,
  );
  if (avatar) {
    img.src = avatar;
  }
  const label = document.createElement("span");
  label.textContent = login ? `@${login}` : "verified";
  node.append(img, label);
  return node;
}

function timelineStatus(feed, route) {
  const status = document.createElement("div");
  status.className = "timeline-status";
  const visibility = feed.visibility || "visible";
  const lastSeen = feed.last_seen_at ? relativeTime(feed.last_seen_at) : "waiting";
  status.append(
    statusItem("visibility", visibility),
    statusItem("last seen", lastSeen),
    statusItem("network", route.network),
  );
  return status;
}

function statusItem(label, value) {
  const item = document.createElement("span");
  item.textContent = `${label}: ${value}`;
  return item;
}

function relativeTime(value) {
  const timestamp = Date.parse(value);
  if (!Number.isFinite(timestamp)) {
    return String(value);
  }
  const seconds = Math.max(0, Math.round((Date.now() - timestamp) / 1000));
  if (seconds < 60) {
    return `${seconds}s ago`;
  }
  const minutes = Math.round(seconds / 60);
  if (minutes < 60) {
    return `${minutes}m ago`;
  }
  const hours = Math.round(minutes / 60);
  if (hours < 24) {
    return `${hours}h ago`;
  }
  return `${Math.round(hours / 24)}d ago`;
}

function relativeTimeFromAny(value) {
  if (!value) {
    return "";
  }
  return relativeTime(value);
}

function publisherLoginForProfile(feed, ticket) {
  return (
    feed.publisher_login ||
    feed.github_login ||
    feed.publisher?.github_login ||
    feed.owner?.current_login ||
    ticket.profile?.login ||
    ""
  )
    .replace(/^@/, "")
    .split(/\s|\//)[0];
}

function updateSourceCountFromClassification(route, classification = {}) {
  if (!remoteRoute && route) {
    return;
  }
  if (classification.feedCount !== undefined) {
    remoteFeedCount = Number(classification.feedCount) || 0;
    updateSourceCount();
  }
}

function updateSourceCountFromFeeds(feeds, headlines = []) {
  remoteFeedCount = logicalFeedCount(feeds, headlines);
  setText(sourceCount, feedCountLabel(remoteFeedCount));
}

function logicalFeedCount(feeds = [], headlines = []) {
  const keys = new Set();
  for (const feed of feeds || []) {
    const key = logicalFeedKeyFromFeed(feed);
    if (key) {
      keys.add(key);
    }
  }
  for (const item of headlines || []) {
    const key = logicalFeedKeyFromHeadline(item);
    if (key) {
      keys.add(key);
    }
  }
  return keys.size;
}

function logicalFeedKeyFromFeed(feed) {
  if (!feed) {
    return "";
  }
  const owner =
    feed.publisher_github_user_id ||
    feed.github_user_id ||
    feed.owner?.github_user_id ||
    normalizeRouteLogin(feed.publisher_login || feed.github_login || feed.owner?.current_login || "");
  const label = normalizeRouteLogin(feed.label || feed.feed_label || "feed");
  if (owner && label) {
    return `${owner}/${label}`;
  }
  return String(feed.feed_id || feed.id || "");
}

function logicalFeedKeyFromHeadline(item) {
  if (!item) {
    return "";
  }
  const owner =
    item.publisher_github_user_id ||
    item.github_user_id ||
    item.publisher?.github_user_id ||
    normalizeRouteLogin(publisherLoginFromHeadline(item));
  const label = normalizeRouteLogin(item.feed_label || item.label || item.feed || "feed");
  if (owner && label) {
    return `${owner}/${label}`;
  }
  return String(item.feed_id || item.id || item.capsule_id || "");
}

function feedCountLabel(count) {
  return `${count} ${count === 1 ? "feed" : "feeds"}`;
}

function connectSse() {
  logInfo("feed.sse.connecting", { url: "/events.sse" });
  const source = new EventSource("/events.sse");
  source.addEventListener("open", () => {
    setText(liveState, "live");
    logInfo("feed.sse.open", { ready_state: source.readyState });
  });
  source.addEventListener("error", (event) => {
    setText(liveState, "retry");
    logError("sse connection error", event);
  });
  source.addEventListener("bulletin", (event) => {
    try {
      const envelope = JSON.parse(event.data);
      const bulletin = envelope.bulletin || envelope;
      if (!bulletin || !bulletin.id) {
        logWarn("sse bulletin missing id", envelope);
      }
      logInfo("feed.sse.bulletin.incoming", {
        bulletin_id: bulletin.id,
        mode: bulletin.mode,
        priority: bulletin.priority,
        dwell_ms: bulletin.dwell_ms,
        publisher: bulletin.publisher?.github_login || bulletin.feed_publisher?.github_login,
      });
      queueIncomingBulletin(bulletin, "sse");
      updateSourceCount();
    } catch (error) {
      logError("sse bulletin parse/render failed", error);
    }
  });
}

function updateClock() {
  const now = new Date();
  setText(
    clock,
    now.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" }),
  );
  refreshStoryTime();
}

function updateSourceCount() {
  if (remoteRoute && remoteFeedCount !== undefined) {
    if (remoteRoute.feedMode === "following") {
      setText(sourceCount, followTargetCountLabel(remoteFeedCount));
    } else {
      setText(sourceCount, feedCountLabel(remoteFeedCount));
    }
    return;
  }
  const sources = new Set();
  for (const bulletin of bulletins) {
    const source = bulletinSourceKey(bulletin);
    if (source) {
      sources.add(source);
    }
  }
  if (sources.size > 0) {
    setText(sourceCount, `${sources.size} ${sources.size === 1 ? "story source" : "story sources"}`);
  } else {
    setText(sourceCount, "0 src");
  }
}

function bulletinSourceKey(bulletin) {
  if (bulletin.source_key || bulletin.sourceKey) {
    return String(bulletin.source_key || bulletin.sourceKey);
  }
  if (bulletin.feed_id || bulletin.feedId) {
    return String(bulletin.feed_id || bulletin.feedId);
  }
  const publisherLogin = bulletin.publisher?.github_login || bulletin.publisher?.login || "";
  if (publisherLogin || bulletin.feed_label) {
    return `${publisherLogin || "feed"}/${bulletin.feed_label || "*"}`;
  }
  if (bulletin.lower_third || bulletin.lowerThird) {
    return String(bulletin.lower_third || bulletin.lowerThird);
  }
  const firstChip = bulletin.chips?.[0];
  return typeof firstChip === "string" ? firstChip : firstChip?.label || "";
}

updateClock();
scheduleStageFit();
window.addEventListener("resize", scheduleStageFit);
window.setInterval(updateClock, 1000);
if (githubAuthCallback) {
  handleGithubAuthCallback(githubAuthCallback);
} else if (isNetworkView()) {
  startNetworkView();
} else if (remoteRoute) {
  startRemoteRoute(remoteRoute);
} else {
  hydrate();
  connectSse();
  window.setInterval(hydrate, LOCAL_SNAPSHOT_REFRESH_MS);
}
