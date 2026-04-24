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
const headlineImage = document.querySelector("#headline-image");
const headlineImageImg = document.querySelector("#headline-image-img");
const stageProgress = document.querySelector("#stage-progress");
const timeline = document.querySelector("#timeline");
const modeSwitcher = document.querySelector("#mode-switcher");
const modeDiscovery = document.querySelector("#mode-discovery");
const modeSubscribed = document.querySelector("#mode-subscribed");
const modeLocal = document.querySelector("#mode-local");

let bulletins = [];
let activeIndex = 0;
let dwellTimer = undefined;
let controlsTimer = undefined;

const FEED_PROTOCOL_VERSION = 1;
const FEED_MODEL_VERSION = 1;
const FEED_MIN_MODEL_VERSION = 1;

const githubAuthCallback = parseGithubAuthCallback(window.location);
const remoteRoute = githubAuthCallback ? undefined : parseRemoteRoute(window.location);

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

function setText(node, value) {
  if (node) {
    node.textContent = value || "";
  }
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
  const duration = Number.isFinite(Number(dwellMs))
    ? Math.max(Number(dwellMs), 1000)
    : 14000;
  stageProgress.hidden = false;
  stageProgress.style.setProperty("--dwell", `${duration}ms`);
  stageProgress.classList.remove("is-running");
  void stageProgress.offsetWidth;
  stageProgress.classList.add("is-running");
}

function stopStageProgress() {
  if (stageProgress) {
    stageProgress.classList.remove("is-running");
    stageProgress.style.removeProperty("--dwell");
    stageProgress.hidden = true;
  }
}

function renderBulletin(bulletin) {
  if (!bulletin) {
    logWarn("render skipped empty bulletin");
    return;
  }

  logInfo("feed.bulletin.render", {
    bulletin_id: bulletin.id,
    mode: bulletin.mode,
    priority: bulletin.priority,
    dwell_ms: bulletin.dwell_ms || bulletin.dwellMs,
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
    clearAuthAction();
    renderChips(bulletin.chips || []);
    renderTicker(bulletin.ticker || []);
    stage?.classList.remove("is-changing");
    if (bulletins.length > 1) {
      restartStageProgress(bulletin.dwell_ms || bulletin.dwellMs || 14000);
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
    return;
  }
  if (!imagesEnabled()) {
    headlineImage.hidden = true;
    headlineImageImg.removeAttribute("src");
    headlineImageImg.setAttribute("alt", "");
    logWarn("headline image ignored because text-only mode is active", nextImage.source || "");
    return;
  }
  const src = safeMediaUrl(nextImage.uri || nextImage.url || nextImage.src);
  if (!src) {
    headlineImage.hidden = true;
    headlineImageImg.removeAttribute("src");
    headlineImageImg.setAttribute("alt", "");
    return;
  }
  headlineImageImg.src = src;
  headlineImageImg.setAttribute("alt", nextImage.alt || "feed generated headline image");
  headlineImage.hidden = false;
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
  clearAuthAction();
  renderChips([
    route.feedMode === "subscribed"
      ? "subscribed"
      : route.kind === "global"
        ? "discovery"
        : "selected",
    route.network,
    state === "version-mismatch" ? "version" : "redacted",
  ]);
  renderTicker(lines);
  stopStageProgress();
}

function routeEyebrow(route) {
  if (route.kind === "global") {
    return `feed / ${route.network} / ${route.feedMode}`;
  }
  return `${route.network} / ${route.feedMode} / ${routeStreamLabel(route)}`;
}

function routeStreamLabel(route) {
  if (route.kind === "global") {
    return route.feedMode === "subscribed" ? "explicit subscriptions" : "all public feeds";
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
      return "waiting for stories";
    case "version-mismatch":
      return "update your peer";
    case "live":
      return "live feed";
    case "failed":
      return "feed unavailable";
    default:
      return "waiting for stories";
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
    "network discovery and subscribed remote feeds are unavailable because p2p is disabled.",
  );
  renderPublisher(undefined);
  renderHeadlineImage(undefined);
  clearAuthAction();
  renderChips(["p2p off", "privacy on"]);
  renderTicker(["start with --p2p or use the hosted p2p browser shell"]);
  stopStageProgress();
}

function renderLocalLoopbackRoute(route) {
  logInfo("feed.local.loopback.selected", {
    scope: route.kind || "global",
    login: route.login,
    network: route.network,
    local_reel_url: localReelUrl(),
  });
  showStage();
  document.body.dataset.mode = "dispatch";
  setText(liveState, "local");
  setText(eyebrow, "feed / local / loopback");
  setText(headline, "open local reel");
  setText(
    deck,
    "the hosted shell does not read loopback streams by default · use the daemon-served reel for local agent activity",
  );
  renderPublisher(undefined);
  renderHeadlineImage(undefined);
  setAuthAction(localReelUrl(), "open local reel", true);
  renderChips(["local", "loopback", "opt-in", "redacted"]);
  renderTicker(["agent-feed serve publishes local streams at 127.0.0.1:7777/reel"]);
  stopStageProgress();
}

function renderAuthRequired(route) {
  renderRemoteState(route, "auth-required", [
    "github sign-in required",
    "private feeds need a signed browser session",
    "open /network to sign in",
  ]);
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
}

function renderTicker(items) {
  if (!ticker) {
    return;
  }
  ticker.replaceChildren();
  const item = document.createElement("span");
  item.textContent = items.length
    ? items.map((entry) => entry.text || entry).join(" · ")
    : "activity is reduced before display";
  ticker.appendChild(item);
}

function scheduleNext(dwellMs) {
  window.clearTimeout(dwellTimer);
  dwellTimer = window.setTimeout(() => {
    if (bulletins.length <= 1) {
      stopStageProgress();
      scheduleNext(dwellMs);
      return;
    }
    activeIndex = (activeIndex + 1) % bulletins.length;
    const next = bulletins[activeIndex];
    renderBulletin(next);
    scheduleNext(next.dwell_ms || 14000);
  }, dwellMs || 14000);
}

function applySnapshot(snapshot) {
  if (!snapshot || !Array.isArray(snapshot.bulletins)) {
    logWarn("snapshot payload missing bulletins", snapshot);
  }
  bulletins = snapshot.bulletins || [];
  if (snapshot.active) {
    const index = bulletins.findIndex((item) => item.id === snapshot.active.id);
    activeIndex = index >= 0 ? index : bulletins.length - 1;
    renderBulletin(snapshot.active);
    scheduleNext(snapshot.active.dwell_ms || 14000);
  } else {
    stopStageProgress();
  }
  updateSourceCount();
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
  } catch (error) {
    setText(liveState, "wait");
    stopStageProgress();
    logError("snapshot hydration failed", error);
  }
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
  const params = new URLSearchParams(location.search);
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
    subscriptionTargets: routeSubscriptionTargets(login, feedSegment, params),
    interactive:
      params.get("view") === "timeline" ||
      params.get("mode") === "timeline" ||
      ["1", "true", "on"].includes(params.get("timeline") || ""),
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
    subscriptionTargets: routeSubscriptionTargets("", "*", params),
    interactive:
      params.get("view") === "timeline" ||
      params.get("mode") === "timeline" ||
      ["1", "true", "on"].includes(params.get("timeline") || ""),
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
    params.has("discovery")
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
    return "local";
  }
  if (["subscribed", "subscriptions", "following"].includes(explicit)) {
    return "subscribed";
  }
  if (scope === "global" && ["discovery", "discover", "hero", "public", ""].includes(explicit)) {
    return "discovery";
  }
  if (
    params.has("subscriptions") ||
    params.has("subscribed") ||
    ["1", "true", "on"].includes(params.get("following") || "")
  ) {
    return "subscribed";
  }
  if (scope === "global") {
    return "discovery";
  }
  if (["discovery", "discover", "hero", "public"].includes(explicit)) {
    logInfo("feed.user.discovery_alias", {
      message: "per-user discovery is represented by user/* wildcard routes",
    });
  }
  return "selected";
}

function routeSubscriptionTargets(login, feedSegment, params) {
  const raw =
    params.get("subscriptions") ||
    params.get("subscribed") ||
    params.get("following") ||
    "";
  const explicit = raw
    .split(",")
    .map((target) => target.trim())
    .filter(Boolean)
    .filter(isSafeSubscriptionTarget);
  if (explicit.length) {
    return explicit;
  }
  const stored = storedSubscriptions().filter(isSafeSubscriptionTarget);
  if (stored.length) {
    return stored;
  }
  if (!login) {
    return [];
  }
  if (feedSegment) {
    return [`${login}/${feedSegment}`];
  }
  return [`${login}/*`];
}

function storedSubscriptions() {
  try {
    const raw =
      window.localStorage.getItem("feed.subscriptions") ||
      window.localStorage.getItem("agent_feed.subscriptions") ||
      "";
    if (!raw) {
      return [];
    }
    const value = JSON.parse(raw);
    if (Array.isArray(value)) {
      return value.map(String);
    }
    if (typeof value === "string") {
      return value.split(",");
    }
  } catch (error) {
    logError("subscription list read failed", error);
  }
  return [];
}

function isSafeSubscriptionTarget(value) {
  return /^@?[A-Za-z0-9](?:[A-Za-z0-9-]{0,37}[A-Za-z0-9])?(?:\/(?:\*|[A-Za-z0-9][A-Za-z0-9_.-]{0,63}))?$/.test(
    value,
  );
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
  return login;
}

function p2pEnabled() {
  return window.FEED_P2P_ENABLED === true || window.FEED_P2P_ENABLED === "true";
}

function setupModeSwitcher(route) {
  if (!modeSwitcher || !modeDiscovery || !modeSubscribed || !modeLocal || !p2pEnabled()) {
    hideModeSwitcher();
    return;
  }
  modeDiscovery.textContent = route.kind === "global" ? "discovery" : `${route.login}/*`;
  modeSubscribed.textContent = route.kind === "global" ? "subscribed" : "subscribed";
  modeDiscovery.setAttribute("href", modeUrl(route, "discovery"));
  modeSubscribed.setAttribute("href", modeUrl(route, "subscribed"));
  modeLocal.setAttribute("href", localReelUrl());
  modeDiscovery.toggleAttribute(
    "aria-current",
    (route.kind === "global" && route.feedMode === "discovery") ||
      (route.kind === "user" && route.feedMode !== "subscribed" && route.feedMode !== "local"),
  );
  modeSubscribed.toggleAttribute("aria-current", route.feedMode === "subscribed");
  modeLocal.toggleAttribute("aria-current", route.feedMode === "local");
  modeSwitcher.hidden = false;
  const reveal = () => revealControls();
  window.addEventListener("pointermove", reveal, { passive: true });
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
  if (mode === "local") {
    return localReelUrl();
  }
  const params = new URLSearchParams(route.query);
  params.set("feed_mode", mode);
  if (route.network && route.network !== "mainnet") {
    params.set("network", route.network);
  }
  if (mode === "subscribed" && !params.has("subscriptions") && route.subscriptionTargets.length) {
    params.set("subscriptions", route.subscriptionTargets.join(","));
  }
  if (mode === "discovery") {
    params.delete("subscriptions");
    params.delete("subscribed");
    params.delete("following");
    params.delete("streams");
  }
  if (mode === "subscribed") {
    params.delete("all");
    params.delete("streams");
  }
  if (route.kind === "global") {
    const query = params.toString();
    return query ? `/?${query}` : "/";
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
        : route.subscriptionTargets.length === 1 &&
            route.subscriptionTargets[0].replace(/^@/, "").startsWith(`${route.login}/`)
          ? route.subscriptionTargets[0].replace(/^@/, "").split("/")[1] || "*"
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

function localReelUrl() {
  return "http://127.0.0.1:7777/reel";
}

function parseGithubAuthCallback(location) {
  if (location.pathname !== "/callback/github") {
    return undefined;
  }
  const params = new URLSearchParams(location.search);
  return {
    state: params.get("state") || "",
    login: params.get("login") || "",
    github_user_id: params.get("github_user_id") || params.get("id") || "",
    name: params.get("name") || "",
    avatar_url: params.get("avatar_url") || params.get("avatar") || "",
    session_token:
      params.get("session") || params.get("session_token") || params.get("grant") || "",
    expires_at: params.get("expires_at") || "",
    return_to: params.get("return_to") || "/network",
  };
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

function browserSignInUrl(returnTo = window.location.href) {
  const state = randomState();
  window.localStorage.setItem("feed.github.auth_state", state);
  const params = new URLSearchParams();
  params.set("client", "feed-browser");
  params.set("return_to", returnTo);
  params.set("state", state);
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
    renderChips(["github", "signed-in", "browser", "story-only"]);
  } else {
    setText(headline, "github sign-in");
    setText(deck, "sign in to request private feed grants and receive signed browser bootstrap material.");
    renderPublisher(undefined);
    renderHeadlineImage(undefined);
    setAuthAction(browserSignInUrl(`${window.location.origin}/network`));
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
  if (route.feedMode === "local") {
    setupModeSwitcher(route);
    renderLocalLoopbackRoute(route);
    return;
  }
  if (!p2pEnabled()) {
    renderP2pDisabled(route);
    return;
  }
  setupModeSwitcher(route);
  if (route.feedMode === "subscribed") {
    startSubscribedRoute(route);
    return;
  }
  if (route.kind === "global") {
    await startGlobalDiscoveryRoute(route);
    return;
  }
  await startUserRoute(route);
}

async function startGlobalDiscoveryRoute(route) {
  renderRemoteState(route, "resolving", [
    "joining p2p network",
    `searching ${route.network}`,
    "requesting accessible story snapshots",
    "waiting for settled story capsules",
  ]);
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
    const feeds = snapshotFeeds(snapshot);
    const headlines = snapshotHeadlines(snapshot);
    logInfo("feed.network.discovery.snapshot", {
      network_id: snapshot.network_id,
      feeds: feeds.length,
      headlines: headlines.length,
    });
    if (route.interactive) {
      renderGlobalTimeline(route, snapshot);
      return;
    }
    if (headlines.length) {
      applyRemoteHeadlines(route, headlines);
      return;
    }
    if (feeds.length) {
      renderRemoteState(route, "waiting", [
        `network directory found ${feeds.length} feeds`,
        "connected · waiting for first story",
        "raw events unavailable",
      ]);
      updateSourceCountFromFeeds(feeds);
      return;
    }
    renderRemoteState(route, "no-feeds", [
      "no visible settled story streams",
      "network fabric may still be routing peers",
      "waiting for published headlines",
    ]);
  } catch (error) {
    renderRemoteState(route, "failed", [
      "edge snapshot mode unavailable",
      "waiting for p2p live path",
    ]);
    logError("network discovery failed", error);
  }
}

async function startUserRoute(route) {
  renderRemoteState(route, "resolving", [
    "resolving github identity",
    `finding feeds on ${route.network}`,
    "dialing p2p peers",
    "waiting for story capsules",
  ]);
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
    if (response.status === 404) {
      renderRemoteState(route, "not-found", ["github user not found"]);
      return;
    }
    if (response.status === 401 || response.status === 403) {
      renderAuthRequired(route);
      logInfo("feed.remote.auth_required", {
        login: route.login,
        status: response.status,
      });
      return;
    }
    if (!response.ok) {
      throw new Error(`resolver failed: ${response.status}`);
    }
    const ticket = await response.json();
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
    logInfo("feed.user.ticket", {
      login: ticket.profile?.login || route.login,
      github_user_id: ticket.github_user_id || ticket.resolved_github_id,
      feeds: feedCount,
      incompatible_feeds: incompatibleFeeds,
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
    if (feedCount === 0) {
      renderRemoteState(route, "no-feeds", [
        "github identity found",
        "no visible settled story streams",
      ], ticket.profile);
      logInfo("feed.user.no_visible_streams", {
        login: ticket.profile?.login || route.login,
        github_user_id: ticket.github_user_id || ticket.resolved_github_id,
      });
      return;
    }
    if (route.interactive) {
      renderTimeline(route, ticket);
      logInfo("feed.timeline.ready", {
        selection: route.selection,
        feeds: feedCount,
      });
      return;
    }
    renderRemoteState(route, "waiting", [
      "github identity found",
      "searching mainnet",
      "connected · waiting for first story",
    ], ticket.profile);
  } catch (error) {
    renderRemoteState(route, "failed", [
      "edge snapshot mode unavailable",
      "waiting for p2p live path",
    ]);
    logError("remote route resolution failed", error);
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

function startSubscribedRoute(route) {
  const targets = route.subscriptionTargets.length
    ? route.subscriptionTargets
    : route.kind === "global"
      ? []
      : [route.selection];
  showStage();
  document.body.dataset.mode = "dispatch";
  setText(liveState, "wait");
  setText(eyebrow, `feed / ${route.network} / subscribed`);
  setText(headline, "subscribed feed");
  setText(
    deck,
    [
      "showing explicit feed subscriptions",
      "waiting for signed story capsules",
      "network discovery feed is not mixed in",
    ].join(" · "),
  );
  renderPublisher(undefined);
  renderHeadlineImage(undefined);
  clearAuthAction();
  renderChips(["subscribed", "explicit", route.network, "redacted"]);
  renderTicker(targets.length ? targets.map((target) => `follow ${target}`) : ["no subscriptions selected"]);
  if (route.interactive) {
    renderSubscribedTimeline(route, targets);
  } else {
    stopStageProgress();
  }
  logInfo("feed.subscribed.selected", {
    network: route.network,
    subscriptions: targets,
  });
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

function applyRemoteHeadlines(route, headlines) {
  bulletins = headlines
    .map((item, index) => remoteHeadlineToBulletin(route, item, index))
    .filter(Boolean)
    .slice(-12);
  if (!bulletins.length) {
    renderRemoteState(route, "waiting", [
      "network directory connected",
      "waiting for display-safe headlines",
    ]);
    return;
  }
  activeIndex = bulletins.length - 1;
  const active = bulletins[activeIndex];
  renderBulletin(active);
  scheduleNext(active.dwell_ms || 14000);
  updateSourceCount();
  logInfo("feed.network.discovery.headlines.render", {
    headlines: bulletins.length,
  });
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
    eyebrow: `feed / ${route.network} / discovery`,
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
    renderRemoteState(route, "failed", ["timeline surface unavailable"]);
    return;
  }
  const feeds = snapshotFeeds(snapshot);
  const headlines = snapshotHeadlines(snapshot);
  logInfo("feed.network.timeline.render", {
    network: route.network,
    feeds: feeds.length,
    headlines: headlines.length,
  });
  if (stage) {
    stage.hidden = true;
  }
  stopStageProgress();
  timeline.hidden = false;
  document.body.dataset.view = "timeline";
  document.body.dataset.mode = "dispatch";
  setText(liveState, "browse");
  setText(sourceCount, `${Math.max(feeds.length, headlines.length)} net`);
  timeline.replaceChildren();

  const toolbar = document.createElement("div");
  toolbar.className = "timeline-toolbar";
  const label = document.createElement("span");
  label.textContent = `network discovery / ${route.network}`;
  toolbar.appendChild(label);
  const nav = document.createElement("nav");
  nav.className = "timeline-feeds";
  nav.appendChild(rootModeLink(route, "discovery", "all public feeds", true));
  nav.appendChild(rootModeLink(route, "subscribed", "subscribed", false));
  toolbar.appendChild(nav);
  timeline.appendChild(toolbar);

  if (headlines.length) {
    for (const item of headlines) {
      const card = document.createElement("article");
      card.className = "timeline-card";
      card.tabIndex = 0;
      const meta = document.createElement("div");
      meta.className = "timeline-meta";
      meta.textContent = item.lower_third || item.publisher_label || "network headline";
      const title = document.createElement("h2");
      title.textContent = item.headline || item.title || "settled story";
      const copy = document.createElement("p");
      copy.textContent = item.deck || item.summary || "story-only capsule";
      card.append(meta, title, copy);
      timeline.appendChild(card);
    }
  } else {
    for (const feed of feeds) {
      const card = document.createElement("article");
      card.className = "timeline-card";
      card.tabIndex = 0;
      card.appendChild(timelinePublisher(feed, { profile: {} }));
      const meta = document.createElement("div");
      meta.className = "timeline-meta";
      meta.textContent = feed.label || feed.feed_label || "feed";
      const title = document.createElement("h2");
      title.textContent = "waiting for published headline";
      const copy = document.createElement("p");
      copy.textContent =
        "this feed is visible on the network. settled story capsules will appear without raw events.";
      card.append(meta, title, copy, timelineStatus(feed, route));
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
  renderTicker(["interactive network discovery", "subscribed feeds stay explicit"]);
}

function rootModeLink(route, mode, label, current = false) {
  const link = document.createElement("a");
  const params = new URLSearchParams();
  params.set("feed_mode", mode);
  if (route.network && route.network !== "mainnet") {
    params.set("network", route.network);
  }
  link.href = `/?${params.toString()}`;
  link.textContent = label;
  if (current) {
    link.setAttribute("aria-current", "page");
  }
  return link;
}

function renderTimeline(route, ticket) {
  if (!timeline) {
    renderRemoteState(route, "failed", ["timeline surface unavailable"]);
    return;
  }
  const feeds = ticket.feeds || ticket.candidate_feeds || [];
  logInfo("feed.timeline.render", {
    login: ticket.profile?.login || route.login,
    selection: route.selection,
    feeds: feeds.length,
    wildcard: route.feed === "*" || !route.feed,
  });
  if (stage) {
    stage.hidden = true;
  }
  stopStageProgress();
  timeline.hidden = false;
  document.body.dataset.view = "timeline";
  document.body.dataset.mode = "dispatch";
  setText(liveState, "browse");
  updateSourceCountFromFeeds(ticket.feeds || ticket.candidate_feeds || []);
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
  toolbar.appendChild(nav);
  timeline.appendChild(toolbar);

  for (const feed of feeds) {
    const feedLabel = feed.label || feed.feed_label || "feed";
    if (route.feed && route.feed !== "*" && route.feed !== feedLabel) {
      continue;
    }
    const card = document.createElement("article");
    card.className = "timeline-card";
    card.tabIndex = 0;
    card.appendChild(timelinePublisher(feed, ticket));
    const meta = document.createElement("div");
    meta.className = "timeline-meta";
    meta.textContent = feedLabel;
    const title = document.createElement("h2");
    title.textContent = `waiting for ${feedLabel}`;
    const copy = document.createElement("p");
    copy.textContent =
      "settled story capsules will appear here as a vertical feed. raw events remain unavailable.";
    card.append(meta, title, copy, timelineStatus(feed, route));
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
  renderTicker([`interactive timeline · ${routeStreamLabel(route)}`, "projection mode remains automatic"]);
}

function renderSubscribedTimeline(route, targets) {
  if (!timeline) {
    renderRemoteState(route, "failed", ["timeline surface unavailable"]);
    return;
  }
  logInfo("feed.subscribed.timeline.render", {
    network: route.network,
    subscriptions: targets,
  });
  if (stage) {
    stage.hidden = true;
  }
  stopStageProgress();
  timeline.hidden = false;
  document.body.dataset.view = "timeline";
  document.body.dataset.mode = "dispatch";
  setText(liveState, "follow");
  setText(sourceCount, `${targets.length} sub`);
  timeline.replaceChildren();

  const toolbar = document.createElement("div");
  toolbar.className = "timeline-toolbar";
  const label = document.createElement("span");
  label.textContent = `subscribed / ${route.network}`;
  toolbar.appendChild(label);
  const nav = document.createElement("nav");
  nav.className = "timeline-feeds";
  for (const target of targets) {
    const link = document.createElement("a");
    link.href = subscribedTargetUrl(target);
    link.textContent = target;
    nav.appendChild(link);
  }
  toolbar.appendChild(nav);
  timeline.appendChild(toolbar);

  for (const target of targets) {
    const card = document.createElement("article");
    card.className = "timeline-card";
    card.tabIndex = 0;
    const meta = document.createElement("div");
    meta.className = "timeline-meta";
    meta.textContent = `${target} / subscribed`;
    const title = document.createElement("h2");
    title.textContent = "waiting for story";
    const copy = document.createElement("p");
    copy.textContent =
      "only explicitly subscribed settled story capsules appear here. discovery results are not mixed in.";
    const status = document.createElement("div");
    status.className = "timeline-status";
    status.append(statusItem("target", target), statusItem("mode", "subscribed only"));
    card.append(meta, title, copy, status);
    timeline.appendChild(card);
  }
  if (!targets.length) {
    const card = document.createElement("article");
    card.className = "timeline-card timeline-empty";
    card.tabIndex = 0;
    const meta = document.createElement("div");
    meta.className = "timeline-meta";
    meta.textContent = "subscribed";
    const title = document.createElement("h2");
    title.textContent = "no subscriptions selected";
    const copy = document.createElement("p");
    copy.textContent =
      "choose explicit feeds to follow. network discovery remains separate from subscriptions.";
    card.append(meta, title, copy);
    timeline.appendChild(card);
  }
  renderTicker(["interactive subscribed timeline", "move mouse for mode switcher"]);
}

function subscribedTargetUrl(target) {
  const clean = target.replace(/^@/, "");
  const [login, feed = "*"] = clean.split("/");
  return `/${encodeURIComponent(login)}/${encodeURIComponent(feed)}?feed_mode=subscribed&view=timeline&subscriptions=${encodeURIComponent(target)}`;
}

function feedLink(login, label, text, current = false) {
  const link = document.createElement("a");
  link.href = `/${encodeURIComponent(login)}/${encodeURIComponent(label)}?view=timeline`;
  link.textContent = text;
  if (current) {
    link.setAttribute("aria-current", "page");
  }
  return link;
}

function timelinePublisher(feed, ticket) {
  const node = document.createElement("div");
  node.className = "publisher";
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
  label.textContent = publisherText(feed, ticket);
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

function publisherText(feed, ticket) {
  const login =
    feed.publisher_login ||
    feed.owner?.current_login ||
    ticket.profile?.login ||
    "verified";
  return `@${login}`;
}

function updateSourceCountFromFeeds(feeds) {
  setText(sourceCount, `${feeds.length} feeds`);
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
      bulletins.push(bulletin);
      bulletins = bulletins.slice(-12);
      activeIndex = bulletins.length - 1;
      renderBulletin(bulletin);
      scheduleNext(bulletin.dwell_ms || 14000);
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
}

function updateSourceCount() {
  const sources = new Set();
  for (const bulletin of bulletins) {
    const firstChip = bulletin.chips?.[0];
    const label = typeof firstChip === "string" ? firstChip : firstChip?.label;
    if (label) {
      sources.add(label);
    }
  }
  setText(sourceCount, `${sources.size} src`);
}

updateClock();
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
}
