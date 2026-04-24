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
const timeline = document.querySelector("#timeline");

let bulletins = [];
let activeIndex = 0;
let dwellTimer = undefined;

const githubAuthCallback = parseGithubAuthCallback(window.location);
const remoteRoute = githubAuthCallback ? undefined : parseRemoteRoute(window.location);

function logError(context, error) {
  console.error(`[feed] ${context}`, error);
}

function logWarn(context, detail) {
  console.warn(`[feed] ${context}`, detail || "");
}

function setText(node, value) {
  if (node) {
    node.textContent = value || "";
  }
}

function setAuthAction(url, label = "sign in with github") {
  if (!authAction) {
    return;
  }
  if (!url || !isNetworkView()) {
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

function renderBulletin(bulletin) {
  if (!bulletin) {
    logWarn("render skipped empty bulletin");
    return;
  }

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
    window.localStorage.getItem("agent_reel.images") === "enabled"
  );
}

function renderRemoteState(route, state, lines = [], nextPublisher = undefined) {
  showStage();
  document.body.dataset.mode = state === "failed" ? "breaking" : "dispatch";
  setText(liveState, state === "live" ? "LIVE" : "WAIT");
  setText(eyebrow, `@${route.login} / ${route.selection.toUpperCase()} / DISCOVERY`);
  setText(headline, `@${route.login}`);
  setText(deck, lines.join(" · "));
  renderPublisher(nextPublisher);
  renderHeadlineImage(undefined);
  clearAuthAction();
  renderChips(["verified lookup", "story-only", route.network, "redacted"]);
  renderTicker(lines);
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
  nextChips.slice(0, 5).forEach((chip) => {
    const item = document.createElement("span");
    item.textContent = typeof chip === "string" ? chip : chip.label;
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
  }
  updateSourceCount();
}

async function hydrate() {
  try {
    const response = await fetch("/api/reel/snapshot", { cache: "no-store" });
    if (!response.ok) {
      throw new Error(`snapshot failed: ${response.status}`);
    }
    applySnapshot(await response.json());
  } catch (error) {
    setText(liveState, "WAIT");
    logError("snapshot hydration failed", error);
  }
}

function parseRemoteRoute(location) {
  const path = location.pathname.replace(/^\/+|\/+$/g, "");
  if (
    !path ||
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
    login,
    network: params.get("network") || "mainnet",
    feed: feedSegment,
    selection: feedSegment ? `${login}/${feedSegment}` : routeSelection(login, params),
    interactive:
      params.get("view") === "timeline" ||
      params.get("mode") === "timeline" ||
      ["1", "true", "on"].includes(params.get("timeline") || ""),
    query: location.search,
  };
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
  if (params.has("all") || streams === "all" || streams === "*" || streams === `${login}/*`) {
    return `${login}/*`;
  }
  if (streams && !streams.includes(",")) {
    return `${login}/${streams}`;
  }
  return login;
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
  if (window.AGENT_REEL_EDGE_BASE_URL) {
    return window.AGENT_REEL_EDGE_BASE_URL;
  }
  if (window.location.hostname === "feed.aberration.technology") {
    return "https://edge.feed.aberration.technology";
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
      window.localStorage.getItem("agent_reel.github.session");
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
  setText(liveState, "AUTH");
  setText(eyebrow, "GITHUB / SIGN-IN / CALLBACK");
  setText(headline, "github sign-in");
  renderPublisher(undefined);
  renderHeadlineImage(undefined);
  renderChips(["github", "verified", "browser", "session"]);
  const expectedState =
    window.localStorage.getItem("feed.github.auth_state") ||
    window.localStorage.getItem("agent_reel.github.auth_state") ||
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
  window.localStorage.removeItem("agent_reel.github.auth_state");
  setText(deck, `signed in as @${callback.login}. returning to network.`);
  console.info("[feed] github browser sign-in complete", {
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
  document.body.dataset.mode = "dispatch";
  setText(liveState, session ? "AUTH" : "WAIT");
  setText(eyebrow, "FEED / NETWORK / GITHUB");
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
}

async function startRemoteRoute(route) {
  renderRemoteState(route, "resolving", [
    "resolving github identity",
    `finding feeds on ${route.network}`,
    "dialing p2p peers",
    "waiting for story capsules",
  ]);
  const endpoint = `${edgeBaseUrl()}/resolve/github/${encodeURIComponent(route.login)}${resolverQuery(route)}`;
  try {
    const response = await fetch(endpoint, {
      cache: "no-store",
      headers: githubAuthHeaders(),
    });
    if (response.status === 404) {
      renderRemoteState(route, "not-found", ["github user not found"]);
      return;
    }
    if (response.status === 401 || response.status === 403) {
      renderAuthRequired(route);
      console.info("[feed] remote route requires github sign-in", {
        login: route.login,
        status: response.status,
      });
      return;
    }
    if (!response.ok) {
      throw new Error(`resolver failed: ${response.status}`);
    }
    const ticket = await response.json();
    const feedCount = ticket.feeds?.length || ticket.candidate_feeds?.length || 0;
    if (feedCount === 0) {
      renderRemoteState(route, "no-feeds", [
        "github identity found",
        "no visible settled story streams",
      ], ticket.profile);
      return;
    }
    if (route.interactive) {
      renderTimeline(route, ticket);
      console.info("[feed] remote timeline ready", {
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
    console.info("[feed] remote discovery ticket", {
      login: ticket.profile?.login || route.login,
      github_user_id: ticket.github_user_id || ticket.resolved_github_id,
      feeds: feedCount,
    });
  } catch (error) {
    renderRemoteState(route, "failed", [
      "edge snapshot mode unavailable",
      "waiting for p2p live path",
    ]);
    logError("remote route resolution failed", error);
  }
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

function renderTimeline(route, ticket) {
  if (!timeline) {
    renderRemoteState(route, "failed", ["timeline surface unavailable"]);
    return;
  }
  if (stage) {
    stage.hidden = true;
  }
  timeline.hidden = false;
  document.body.dataset.view = "timeline";
  document.body.dataset.mode = "dispatch";
  setText(liveState, "BROWSE");
  updateSourceCountFromFeeds(ticket.feeds || ticket.candidate_feeds || []);
  timeline.replaceChildren();

  const feeds = ticket.feeds || ticket.candidate_feeds || [];
  const toolbar = document.createElement("div");
  toolbar.className = "timeline-toolbar";
  const label = document.createElement("span");
  label.textContent = `@${ticket.profile?.login || route.login} / ${route.selection}`;
  toolbar.appendChild(label);
  const nav = document.createElement("nav");
  nav.className = "timeline-feeds";
  nav.appendChild(feedLink(route.login, "*", `${route.login}/*`));
  for (const feed of feeds) {
    const feedLabel = feed.label || feed.feed_label || "feed";
    nav.appendChild(feedLink(route.login, feedLabel, feedLabel));
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
    const meta = document.createElement("div");
    meta.className = "timeline-meta";
    meta.textContent = `${publisherText(feed, ticket)} / ${feedLabel}`;
    const title = document.createElement("h2");
    title.textContent = `waiting for ${feedLabel}`;
    const copy = document.createElement("p");
    copy.textContent =
      "settled story capsules will appear here as a vertical feed. raw events remain unavailable.";
    const chipsRow = document.createElement("div");
    chipsRow.className = "chips";
    for (const chip of ["story-only", "settled", "redacted", route.network]) {
      const item = document.createElement("span");
      item.textContent = chip;
      chipsRow.appendChild(item);
    }
    card.append(meta, title, copy, chipsRow);
    timeline.appendChild(card);
  }
  if (timeline.children.length === 1) {
    const card = document.createElement("article");
    card.className = "timeline-card";
    const meta = document.createElement("div");
    meta.className = "timeline-meta";
    meta.textContent = `@${route.login} / ${route.selection}`;
    const title = document.createElement("h2");
    title.textContent = "no matching feed";
    const copy = document.createElement("p");
    copy.textContent = "the selected logical feed is not visible from this session.";
    card.append(meta, title, copy);
    timeline.appendChild(card);
  }
  renderTicker([`interactive timeline · ${route.selection}`, "projection mode remains automatic"]);
}

function feedLink(login, label, text) {
  const link = document.createElement("a");
  link.href = `/${encodeURIComponent(login)}/${encodeURIComponent(label)}?view=timeline`;
  link.textContent = text;
  return link;
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
  setText(sourceCount, `${feeds.length} FEEDS`);
}

function connectSse() {
  const source = new EventSource("/events.sse");
  source.addEventListener("open", () => {
    setText(liveState, "LIVE");
    console.info("[feed] sse connected");
  });
  source.addEventListener("error", (event) => {
    setText(liveState, "RETRY");
    logError("sse connection error", event);
  });
  source.addEventListener("bulletin", (event) => {
    try {
      const envelope = JSON.parse(event.data);
      const bulletin = envelope.bulletin || envelope;
      if (!bulletin || !bulletin.id) {
        logWarn("sse bulletin missing id", envelope);
      }
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
  setText(sourceCount, `${sources.size} SRC`);
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
