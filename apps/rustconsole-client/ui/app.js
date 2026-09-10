const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const machineStorageKey = "rustconsole-recent-hosts";
const bitrateStorageKey = "rustconsole-maximum-bitrate";
const latencyDiagnosticsStorageKey = "rustconsole-latency-diagnostics";
const defaultPort = 47999;
const defaultBitrate = 20;
const discoveryAuthQueue = new window.RustConsoleDiscoveryAuth.UniqueAsyncQueue();

const elements = {
  pages: [...document.querySelectorAll("[data-page-panel]")],
  navigation: [...document.querySelectorAll("[data-page]")],
  brand: document.querySelector("#brand-link"),
  machineTabs: [...document.querySelectorAll("[data-machine-tab]")],
  recentPanel: document.querySelector("#recent-panel"),
  discoveredPanel: document.querySelector("#discovered-panel"),
  discoveredGrid: document.querySelector("#discovered-grid"),
  noDiscoveredMachines: document.querySelector("#no-discovered-machines"),
  discoveryErrors: document.querySelector("#discovery-errors"),
  discoveryProgress: document.querySelector("#discovery-progress"),
  discoveryProgressBar: document.querySelector("#discovery-progress-bar"),
  refreshDiscovery: document.querySelector("#refresh-discovery"),
  machineGrid: document.querySelector("#machine-grid"),
  noMachines: document.querySelector("#no-machines"),
  cardTemplate: document.querySelector("#machine-card-template"),
  addMachine: document.querySelector("#add-machine-button"),
  emptyAdd: document.querySelector("#empty-add-button"),
  machineDialog: document.querySelector("#machine-dialog"),
  machineForm: document.querySelector("#machine-form"),
  machineAddress: document.querySelector("#machine-address"),
  machinePort: document.querySelector("#machine-port"),
  machineError: document.querySelector("#machine-error"),
  credentialDialog: document.querySelector("#credential-dialog"),
  credentialForm: document.querySelector("#credential-form"),
  credentialMachine: document.querySelector("#credential-machine"),
  machinePassword: document.querySelector("#machine-password"),
  rememberPassword: document.querySelector("#remember-password"),
  credentialError: document.querySelector("#credential-error"),
  credentialSubmit: document.querySelector("#credential-submit"),
  bitrate: document.querySelector("#maximum-bitrate"),
  bitrateValue: document.querySelector("#maximum-bitrate-value"),
  latencyDiagnostics: document.querySelector("#latency-diagnostics"),
  disconnect: document.querySelector("#disconnect-button"),
  appMessage: document.querySelector("#app-message"),
  audioPrerequisiteDialog: document.querySelector("#audio-prerequisite-dialog"),
  audioPrerequisiteTitle: document.querySelector("#audio-prerequisite-title"),
  audioPrerequisiteMessage: document.querySelector("#audio-prerequisite-message"),
  audioPrerequisiteError: document.querySelector("#audio-prerequisite-error"),
  audioPrerequisiteClose: document.querySelector("#audio-prerequisite-close"),
  audioPrerequisiteContinue: document.querySelector("#audio-prerequisite-continue"),
};

let machines = loadMachines();
let playerSession = { phase: "idle", endpoint: null, generation: 0 };
let audioLaunchNoticeShown = false;
let discoveredRoutes = [];
let previousDiscoveredRoutes = [];
let currentDiscoveredRoutes = [];
let discoveryGeneration = 0;
let activeDiscoveryRequest = null;
let authenticatedDiscoveryRoutes = new Set();
let discoveryHasRun = false;
let credentialTarget = null;

function machine(endpoint, identity = null, endpoints = [endpoint], displayName = null, operatingSystem = "unknown") {
  return {
    identity,
    endpoints,
    preferredEndpoint: endpoint,
    resolvedEndpoint: null,
    displayName,
    operatingSystem,
    availability: "checking",
    availabilityDetail: "Checking availability",
    vbCableStatus: "unspecified",
    firewallStatus: "unknown",
    probeGeneration: 0,
  };
}

function canonicalEndpoint(address, port = defaultPort) {
  const host = String(address).trim();
  const numericPort = Number(port);
  if (!host || /[\s/]/.test(host)) throw new Error("Enter a valid IP address or DNS name.");
  if (!Number.isInteger(numericPort) || numericPort < 1 || numericPort > 65535) {
    throw new Error("Port must be between 1 and 65535.");
  }
  if (host.startsWith("[") && host.endsWith("]")) return `${host}:${numericPort}`;
  return host.includes(":") ? `[${host}]:${numericPort}` : `${host}:${numericPort}`;
}

function normalizeStoredEndpoint(value) {
  if (typeof value !== "string") return null;
  const endpoint = value.trim();
  if (!endpoint) return null;
  if (/^\[[^\]]+\]:\d+$/.test(endpoint)) return endpoint;
  const match = endpoint.match(/^([^:]+):(\d+)$/);
  if (match) return canonicalEndpoint(match[1], Number(match[2]));
  try {
    return canonicalEndpoint(endpoint);
  } catch {
    return null;
  }
}

function endpointParts(endpoint) {
  const ipv6 = endpoint.match(/^\[([^\]]+)\]:(\d+)$/);
  if (ipv6) return { address: ipv6[1], port: ipv6[2] };
  const separator = endpoint.lastIndexOf(":");
  return { address: endpoint.slice(0, separator), port: endpoint.slice(separator + 1) };
}

function loadMachines() {
  try {
    const stored = JSON.parse(localStorage.getItem(machineStorageKey) ?? "[]");
    if (!Array.isArray(stored)) return [];
    const loaded = [];
    for (const value of stored) {
      if (typeof value === "string") {
        const endpoint = normalizeStoredEndpoint(value);
        if (endpoint && !loaded.some((item) => item.endpoints.includes(endpoint))) loaded.push(machine(endpoint));
        continue;
      }
      if (!value || typeof value !== "object") continue;
      const endpoints = [...new Set((Array.isArray(value.endpoints) ? value.endpoints : [])
        .map(normalizeStoredEndpoint)
        .filter(Boolean))];
      const preferred = normalizeStoredEndpoint(value.preferredEndpoint);
      if (preferred && !endpoints.includes(preferred)) endpoints.unshift(preferred);
      if (endpoints.length === 0) continue;
      const identity = typeof value.identity === "string" && /^[0-9a-f]{64}$/i.test(value.identity)
        ? value.identity.toLowerCase()
        : null;
      const item = machine(preferred ?? endpoints[0], identity, endpoints);
      item.displayName = typeof value.displayName === "string" && value.displayName.length <= 63
        ? value.displayName
        : null;
      item.operatingSystem = ["windows", "unknown"].includes(value.operatingSystem)
        ? value.operatingSystem
        : "unknown";
      const duplicate = identity
        ? loaded.find((candidate) => candidate.identity === identity)
        : null;
      if (duplicate) {
        duplicate.endpoints = [...new Set([...duplicate.endpoints, ...endpoints])];
        duplicate.displayName ??= item.displayName;
        if (duplicate.operatingSystem === "unknown") duplicate.operatingSystem = item.operatingSystem;
      } else {
        loaded.push(item);
      }
    }
    return loaded;
  } catch {
    return [];
  }
}

function saveMachines() {
  localStorage.setItem(machineStorageKey, JSON.stringify(machines.map((item) => ({
    identity: item.identity,
    endpoints: item.endpoints,
    preferredEndpoint: item.preferredEndpoint,
    displayName: item.displayName,
    operatingSystem: item.operatingSystem,
  }))));
}

function loadBitrate() {
  const value = Number(localStorage.getItem(bitrateStorageKey));
  return Number.isInteger(value) && value >= 5 && value <= 100 ? value : defaultBitrate;
}

function updateBitrate(value) {
  elements.bitrate.value = String(value);
  elements.bitrateValue.value = `${value} Mbit/s`;
}

function showAppMessage(message) {
  elements.appMessage.textContent = String(message);
  elements.appMessage.hidden = false;
}

function clearAppMessage() {
  elements.appMessage.hidden = true;
  elements.appMessage.textContent = "";
}

function setPage(page) {
  for (const panel of elements.pages) {
    const selected = panel.dataset.pagePanel === page;
    panel.hidden = !selected;
    panel.classList.toggle("is-active", selected);
  }
  for (const button of elements.navigation) {
    const selected = button.dataset.page === page;
    button.classList.toggle("is-active", selected);
    if (selected) button.setAttribute("aria-current", "page");
    else button.removeAttribute("aria-current");
  }
  location.hash = page;
  if (page === "machines" && !elements.recentPanel.hidden) probeAllMachines();
}

function setMachineTab(tab) {
  const recent = tab === "recent";
  elements.recentPanel.hidden = !recent;
  elements.discoveredPanel.hidden = recent;
  for (const button of elements.machineTabs) {
    button.setAttribute("aria-selected", String(button.dataset.machineTab === tab));
  }
  if (recent) probeAllMachines();
  else if (!discoveryHasRun) refreshDiscoveredMachines();
}

function availabilityText(value) {
  if (value === "available") return "Available";
  if (value === "busy") return "Busy: another player is streaming";
  return "Desktop session unavailable";
}

function errorNeedsCredential(error) {
  return /credential|password|authentication|opaque/i.test(String(error));
}

async function probeMachine(item, password = "", announceAudio = true) {
  const endpoint = item.preferredEndpoint;
  const generation = ++item.probeGeneration;
  item.availability = "checking";
  item.availabilityDetail = "Checking availability";
  renderMachines();
  try {
    const result = await invoke("probe", { host: endpoint, password });
    if (generation !== item.probeGeneration) return { availability: result.availability, machine: item };
    const identified = mergeAuthenticatedRoute(item, endpoint, result.hostIdentity);
    identified.availability = result.availability;
    identified.availabilityDetail = availabilityText(result.availability);
    identified.resolvedEndpoint = result.endpoint;
    identified.displayName = result.displayName;
    identified.operatingSystem = result.operatingSystem;
    identified.firewallStatus = result.firewallStatus;
    identified.vbCableStatus = ["ready", "unavailable", "check-failed"].includes(result.vbCableStatus)
      ? result.vbCableStatus
      : "unspecified";
    if (announceAudio && identified.vbCableStatus === "unavailable" && !audioLaunchNoticeShown) {
      audioLaunchNoticeShown = true;
      void showAudioPrerequisite("unavailable", false);
    }
    return { availability: result.availability, machine: identified };
  } catch (error) {
    if (generation !== item.probeGeneration) return { availability: item.availability, machine: item };
    item.availability = errorNeedsCredential(error) ? "authentication" : "unreachable";
    item.availabilityDetail = item.availability === "authentication" ? "Authentication required" : "Could not reach machine";
    return { availability: item.availability, machine: item };
  } finally {
    renderMachines();
  }
}

function mergeAuthenticatedRoute(item, endpoint, identity) {
  const authenticatedIdentity = String(identity).toLowerCase();
  if (!/^[0-9a-f]{64}$/.test(authenticatedIdentity)) throw new Error("Host returned an invalid authenticated identity.");
  const existing = machines.find((candidate) => candidate !== item && candidate.identity === authenticatedIdentity);
  if (existing) {
    if (!existing.endpoints.includes(endpoint)) existing.endpoints.push(endpoint);
    existing.preferredEndpoint = endpoint;
    existing.resolvedEndpoint = item.resolvedEndpoint;
    existing.probeGeneration += 1;
    machines = machines.filter((candidate) => candidate !== item);
    saveMachines();
    return existing;
  }
  item.identity = authenticatedIdentity;
  if (!item.endpoints.includes(endpoint)) item.endpoints.push(endpoint);
  item.preferredEndpoint = endpoint;
  saveMachines();
  return item;
}

async function probeAllMachines() {
  clearAppMessage();
  await Promise.allSettled(machines.map((item) => probeMachine(item)));
}

async function probeUntilSessionReleased(item, playerGeneration) {
  for (let attempt = 0; attempt < 30; attempt += 1) {
    if (playerSession.generation !== playerGeneration || window.RustConsoleMachineState.playerActive(playerSession)) return;
    await new Promise((resolve) => window.setTimeout(resolve, 500));
    if (playerSession.generation !== playerGeneration || window.RustConsoleMachineState.playerActive(playerSession)) return;
    const result = await probeMachine(item);
    if (["available", "authentication", "desktop-session-unavailable"].includes(result.availability)) return;
  }
}

function discoveredRoute(value, storedEndpoint = null) {
  return {
    endpoint: value.endpoint,
    storedEndpoint: storedEndpoint ?? value.endpoint,
    source: value.source,
    sourceName: value.sourceName,
    hostIdentity: String(value.hostIdentity).toLowerCase(),
    displayName: value.displayName,
    operatingSystem: value.operatingSystem,
    firewallStatus: value.firewallStatus,
    authenticated: false,
    authenticating: false,
  };
}

function routePriority(route) {
  if (route.source === "manual") return 0;
  if (route.source === "lan") return 1;
  return 2;
}

function discoveryRouteKey(route) {
  return `${route.hostIdentity}\u0000${route.endpoint}`;
}

function operatingSystemLabel(value) {
  return value === "windows" ? "Windows" : "Unknown OS";
}

async function refreshDiscoveredMachines() {
  discoveryHasRun = true;
  const generation = ++discoveryGeneration;
  activeDiscoveryRequest = generation;
  discoveryAuthQueue.reset(generation);
  authenticatedDiscoveryRoutes = new Set();
  previousDiscoveredRoutes = discoveredRoutes;
  currentDiscoveredRoutes = [];
  elements.refreshDiscovery.disabled = true;
  elements.discoveryErrors.hidden = true;
  elements.discoveryErrors.textContent = "";
  elements.discoveryProgress.hidden = false;
  elements.discoveryProgressBar.max = 1;
  elements.discoveryProgressBar.value = 0;
  try {
    const result = await invoke("discover", { requestId: generation });
    if (generation !== discoveryGeneration) return;
    activeDiscoveryRequest = null;
    const provisional = new Map(discoveredRoutes.map((route) => [discoveryRouteKey(route), route]));
    discoveredRoutes = result.endpoints.map((endpoint) => {
      const route = discoveredRoute(endpoint);
      const prior = provisional.get(discoveryRouteKey(route));
      route.authenticated = authenticatedDiscoveryRoutes.has(discoveryRouteKey(route));
      route.authenticating = prior?.authenticating ?? false;
      return route;
    });
    previousDiscoveredRoutes = [];
    currentDiscoveredRoutes = [];
    elements.discoveryProgress.hidden = true;
    const errors = [];
    if (result.lanError) errors.push(`LAN: ${result.lanError}`);
    if (result.tailscaleError) errors.push(`Tailscale: ${result.tailscaleError}`);
    elements.discoveryErrors.textContent = errors.join(" · ");
    elements.discoveryErrors.hidden = errors.length === 0;
    renderDiscoveredMachines();
    for (const route of discoveredRoutes) queueRememberedDiscoveryAuthentication(route, generation);
  } catch (error) {
    if (generation !== discoveryGeneration) return;
    activeDiscoveryRequest = null;
    elements.discoveryErrors.textContent = String(error);
    elements.discoveryErrors.hidden = false;
    elements.discoveryProgress.hidden = true;
    discoveredRoutes = [];
    previousDiscoveredRoutes = [];
    currentDiscoveredRoutes = [];
    renderDiscoveredMachines();
  } finally {
    if (generation === discoveryGeneration) elements.refreshDiscovery.disabled = false;
  }
}

function queueRememberedDiscoveryAuthentication(route, generation) {
  if (!machines.some((item) => item.identity === route.hostIdentity)) return;
  const key = discoveryRouteKey(route);
  if (authenticatedDiscoveryRoutes.has(key)) return;
  if (!discoveryAuthQueue.enqueue(generation, key, { ...route })) return;
  route.authenticating = true;
  void discoveryAuthQueue.drain(authenticateRememberedDiscoveryRoute);
}

async function authenticateRememberedDiscoveryRoute(route, generation) {
  let result;
  try {
    result = await invoke("probe", { host: route.endpoint, password: "" });
  } catch {
    result = null;
  }
  if (!discoveryAuthQueue.isCurrent(generation) || generation !== discoveryGeneration) return;

  const key = discoveryRouteKey(route);
  const current = discoveredRoutes.find((candidate) => discoveryRouteKey(candidate) === key);
  if (current) current.authenticating = false;
  if (!result || String(result.hostIdentity).toLowerCase() !== route.hostIdentity || !current) {
    renderDiscoveredMachines();
    return;
  }

  current.authenticated = true;
  authenticatedDiscoveryRoutes.add(key);
  const item = machines.find((candidate) => candidate.identity === route.hostIdentity);
  if (!item) return;
  if (!item.endpoints.includes(current.storedEndpoint)) item.endpoints.push(current.storedEndpoint);
  const preferred = discoveredRoutes
    .filter((candidate) => candidate.hostIdentity === route.hostIdentity && authenticatedDiscoveryRoutes.has(discoveryRouteKey(candidate)))
    .sort((left, right) => routePriority(left) - routePriority(right))[0];
  item.preferredEndpoint = preferred.storedEndpoint;
  item.resolvedEndpoint = preferred.endpoint;
  item.displayName = result.displayName;
  item.operatingSystem = result.operatingSystem;
  item.firewallStatus = result.firewallStatus;
  item.availability = result.availability;
  item.availabilityDetail = availabilityText(result.availability);
  item.vbCableStatus = result.vbCableStatus;
  saveMachines();
  renderMachines();
  renderDiscoveredMachines();
}

async function confirmDiscoveredRoutes(selected, password) {
  const group = selected.group ?? discoveredRoutes.filter((route) => route.hostIdentity === selected.hostIdentity);
  const attempts = await Promise.all(group.map(async (route) => {
    try {
      const result = await invoke("probe", { host: route.endpoint, password });
      return result.hostIdentity === selected.hostIdentity ? { route, result } : null;
    } catch {
      return null;
    }
  }));
  const confirmed = attempts.filter(Boolean).sort((left, right) => routePriority(left.route) - routePriority(right.route));
  if (confirmed.length === 0) throw new Error("Authentication failed on every discovered route.");

  let item = machines.find((candidate) => candidate.identity === selected.hostIdentity);
  if (!item) {
    const preferred = confirmed[0].route.storedEndpoint;
    item = machine(
      preferred,
      selected.hostIdentity,
      [],
      confirmed[0].result.displayName,
      confirmed[0].result.operatingSystem,
    );
    machines.unshift(item);
  }
  for (const confirmation of confirmed) {
    const endpoint = confirmation.route.storedEndpoint;
    if (!item.endpoints.includes(endpoint)) item.endpoints.push(endpoint);
    confirmation.route.authenticated = true;
    confirmation.route.authenticating = false;
    authenticatedDiscoveryRoutes.add(discoveryRouteKey(confirmation.route));
  }
  item.preferredEndpoint = confirmed[0].route.storedEndpoint;
  item.resolvedEndpoint = confirmed[0].result.endpoint;
  item.displayName = confirmed[0].result.displayName;
  item.operatingSystem = confirmed[0].result.operatingSystem;
  item.firewallStatus = confirmed[0].result.firewallStatus;
  item.availability = confirmed[0].result.availability;
  item.availabilityDetail = availabilityText(confirmed[0].result.availability);
  item.vbCableStatus = confirmed[0].result.vbCableStatus;
  saveMachines();
  return item;
}

function renderDiscoveredMachines() {
  elements.discoveredGrid.replaceChildren();
  elements.noDiscoveredMachines.hidden = discoveredRoutes.length !== 0;
  const groups = new Map();
  for (const route of discoveredRoutes) {
    if (!groups.has(route.hostIdentity)) groups.set(route.hostIdentity, []);
    groups.get(route.hostIdentity).push(route);
  }
  for (const group of groups.values()) {
    for (const route of group) route.group = group;
    const route = group[0];
    const item = machines.find((candidate) => candidate.identity === route.hostIdentity);
    const authenticated = group.some((candidate) => candidate.authenticated) && item;
    const authenticating = group.some((candidate) => candidate.authenticating);
    const card = elements.cardTemplate.content.firstElementChild.cloneNode(true);
    const status = authenticated
      ? window.RustConsoleMachineState.presentation(item, playerSession)
      : null;
    card.dataset.state = status?.state ?? "authentication";
    card.querySelector("h2").textContent = route.displayName ?? route.sourceName ?? route.endpoint;
    const sources = [...new Set(group.map((candidate) => candidate.source === "tailscale" ? "Tailscale" : candidate.source === "lan" ? "LAN" : "Manual"))];
    card.querySelector(".machine-identity p").textContent = `${operatingSystemLabel(route.operatingSystem)} · ${sources.join(" + ")} · ${group.length} ${group.length === 1 ? "route" : "routes"}`;
    card.querySelector(".machine-status span").textContent = authenticated ? status.detail : authenticating ? "Authenticating" : "Not authenticated";
    const routeList = card.querySelector(".route-list");
    routeList.replaceChildren(...group.map((candidate) => {
      const entry = document.createElement("li");
      const source = candidate.source === "tailscale" ? "Tailscale" : candidate.source === "lan" ? "LAN" : "Manual";
      entry.textContent = `${source} · ${candidate.endpoint}`;
      return entry;
    }));
    routeList.hidden = false;
    card.querySelector(".audio-requirement").remove();
    card.querySelector(".firewall-warning").remove();
    card.querySelector(".remove-machine").remove();
    const control = card.querySelector(".card-action");
    if (authenticated) {
      const action = machineAction(item);
      control.textContent = action.label;
      control.disabled = action.disabled;
      if (action.action) control.addEventListener("click", () => Promise.resolve(action.action()).catch(() => {}));
    } else if (authenticating) {
      control.textContent = "Authenticating";
      control.disabled = true;
    } else {
      control.textContent = "Authenticate";
      control.addEventListener("click", () => openCredentialDialogForDiscovery(route));
    }
    elements.discoveredGrid.append(card);
  }
}

async function startPlayer(item, password = "", remember = false) {
  clearAppMessage();
  const generation = playerSession.generation + 1;
  playerSession = { phase: "launching", endpoint: item.preferredEndpoint, generation };
  elements.disconnect.hidden = false;
  renderMachines();
  try {
    await invoke("start", {
      host: item.resolvedEndpoint ?? item.preferredEndpoint,
      password,
      remember,
      maximumBitrateMbps: Number(elements.bitrate.value),
      latencyDiagnostics: elements.latencyDiagnostics.checked,
    });
  } catch (error) {
    if (playerSession.generation === generation) {
      playerSession = { phase: "idle", endpoint: null, generation };
      elements.disconnect.hidden = true;
      renderMachines();
    }
    showAppMessage(error);
    throw error;
  }
}

async function connectMachine(item, password = "", remember = false, alreadyProbed = false) {
  let result = { availability: item.availability, machine: item };
  if (!alreadyProbed) result = await probeMachine(item, password, false);
  if (result.availability !== "available") throw new Error(result.machine.availabilityDetail);
  if (["unavailable", "check-failed"].includes(result.machine.vbCableStatus)) {
    const proceed = await showAudioPrerequisite(result.machine.vbCableStatus, true);
    if (!proceed) return;
  }
  await startPlayer(result.machine, password, remember);
}

function showAudioPrerequisite(status, allowContinue) {
  const failed = status === "check-failed";
  elements.audioPrerequisiteTitle.textContent = failed ? "Audio setup could not be verified" : "Audio setup required";
  elements.audioPrerequisiteMessage.innerHTML = failed
    ? "<strong>Rust Console could not verify the Windows host audio prerequisite.</strong><p>Video can continue, but host audio may be unavailable. Check the host and try again.</p>"
    : "<strong>VB-CABLE is third-party software from VB-Audio Software. It is not included with Rust Console.</strong><p>Rust Console uses VB-CABLE Standard as a virtual audio device on Windows hosts. Download and install it from the official website, then restart Windows.</p><p>VB-CABLE is donationware. All contributions are welcome. A paid license is required for some professional uses.</p>";
  elements.audioPrerequisiteError.hidden = true;
  elements.audioPrerequisiteClose.textContent = allowContinue ? "Cancel" : "Close";
  elements.audioPrerequisiteClose.hidden = false;
  elements.audioPrerequisiteContinue.hidden = !allowContinue;
  elements.audioPrerequisiteDialog.returnValue = "cancel";
  elements.audioPrerequisiteDialog.showModal();
  return new Promise((resolve) => {
    elements.audioPrerequisiteDialog.addEventListener("close", () => {
      resolve(elements.audioPrerequisiteDialog.returnValue === "continue");
    }, { once: true });
  });
}

async function openVbAudioPage(page) {
  elements.audioPrerequisiteError.hidden = true;
  try {
    await invoke("open_vb_audio_page", { page });
  } catch (error) {
    elements.audioPrerequisiteError.textContent = String(error);
    elements.audioPrerequisiteError.hidden = false;
  }
}

function openMachineDialog() {
  elements.machineForm.reset();
  elements.machinePort.value = String(defaultPort);
  elements.machineError.hidden = true;
  elements.machineDialog.showModal();
  elements.machineAddress.focus();
}

function openCredentialDialog(item) {
  credentialTarget = { kind: "recent", item };
  elements.credentialForm.reset();
  elements.credentialMachine.textContent = item.preferredEndpoint;
  elements.credentialError.hidden = true;
  elements.credentialDialog.showModal();
  elements.machinePassword.focus();
}

function openCredentialDialogForDiscovery(route) {
  credentialTarget = { kind: "discovered", route };
  elements.credentialForm.reset();
  elements.credentialMachine.textContent = `${route.displayName ?? route.sourceName ?? route.endpoint} · ${operatingSystemLabel(route.operatingSystem)}`;
  elements.credentialError.hidden = true;
  elements.credentialDialog.showModal();
  elements.machinePassword.focus();
}

function closeDialog(dialog) {
  if (dialog === elements.credentialDialog) {
    elements.machinePassword.value = "";
    credentialTarget = null;
  }
  dialog.close();
}

function machineAction(item) {
  const state = window.RustConsoleMachineState.action(item, playerSession);
  const action = state.kind === "connect"
    ? () => connectMachine(item)
    : state.kind === "authenticate"
      ? () => openCredentialDialog(item)
      : state.kind === "probe"
        ? () => probeMachine(item)
        : null;
  return { ...state, action };
}

function renderMachines() {
  elements.machineGrid.replaceChildren();
  elements.noMachines.hidden = machines.length !== 0;
  for (const item of machines) {
    const card = elements.cardTemplate.content.firstElementChild.cloneNode(true);
    const parts = endpointParts(item.preferredEndpoint);
    const status = window.RustConsoleMachineState.presentation(item, playerSession);
    card.dataset.state = status.state;
    card.dataset.audio = item.vbCableStatus;
    card.querySelector("h2").textContent = item.displayName ?? parts.address;
    const os = item.operatingSystem === "windows" ? "Windows" : "Unknown OS";
    card.querySelector(".machine-identity p").textContent = item.displayName
      ? `${os} · ${item.preferredEndpoint}`
      : `Port ${parts.port}`;
    card.querySelector(".machine-status span").textContent = status.detail;
    const audioRequirement = card.querySelector(".audio-requirement");
    const firewallWarning = card.querySelector(".firewall-warning");
    if (item.vbCableStatus === "unavailable") {
      audioRequirement.textContent = "Audio setup required";
      audioRequirement.hidden = false;
    } else if (item.vbCableStatus === "check-failed") {
      audioRequirement.textContent = "Audio setup could not be verified";
      audioRequirement.hidden = false;
    }
    const warnings = [];
    if (item.firewallStatus === "missing") {
      warnings.push("Windows has no Rust Console firewall rule for UDP 47999. Run rustconsole-host firewall enable <scope> as administrator.");
    } else if (item.firewallStatus === "check-failed") {
      warnings.push("The Windows host could not verify its Rust Console firewall rule.");
    }
    const identityRoutes = discoveredRoutes.filter((route) => route.hostIdentity === item.identity);
    if (identityRoutes.some((route) => route.source === "tailscale")
      && !identityRoutes.some((route) => route.source === "lan")) {
      warnings.push("No LAN route was found. A firewall or network isolation may be hiding it.");
    }
    firewallWarning.textContent = warnings.join(" ");
    firewallWarning.hidden = warnings.length === 0;

    const remove = card.querySelector(".remove-machine");
    remove.disabled = window.RustConsoleMachineState.playerTargets(playerSession, item);
    remove.addEventListener("click", () => {
      machines = machines.filter((candidate) => candidate !== item);
      saveMachines();
      renderMachines();
    });

    const control = card.querySelector(".card-action");
    const action = machineAction(item);
    control.textContent = action.label;
    control.disabled = action.disabled;
    if (action.action) {
      control.addEventListener("click", () => Promise.resolve(action.action()).catch(() => {}));
    }
    elements.machineGrid.append(card);
  }
}

elements.navigation.forEach((button) => button.addEventListener("click", () => setPage(button.dataset.page)));
elements.brand.addEventListener("click", (event) => {
  event.preventDefault();
  setPage("machines");
});
elements.machineTabs.forEach((button) => button.addEventListener("click", () => setMachineTab(button.dataset.machineTab)));
elements.refreshDiscovery.addEventListener("click", refreshDiscoveredMachines);
elements.addMachine.addEventListener("click", openMachineDialog);
elements.emptyAdd.addEventListener("click", openMachineDialog);

document.querySelectorAll("[data-close-dialog]").forEach((button) => {
  button.addEventListener("click", () => closeDialog(document.querySelector(`#${button.dataset.closeDialog}`)));
});

document.querySelectorAll("[data-vb-audio-page]").forEach((button) => {
  button.addEventListener("click", () => openVbAudioPage(button.dataset.vbAudioPage));
});

elements.machineForm.addEventListener("submit", async (event) => {
  event.preventDefault();
  elements.machineError.hidden = true;
  try {
    const endpoint = canonicalEndpoint(elements.machineAddress.value, elements.machinePort.value);
    if (machines.some((item) => item.endpoints.includes(endpoint))) throw new Error("That machine is already saved.");
    const discovered = await invoke("discover_manual", { host: endpoint });
    if (discovered.length === 0) throw new Error("No Rust Console service found on this address and port.");
    const routes = discovered.map((value) => discoveredRoute(value, endpoint));
    for (const route of routes) route.group = routes;
    closeDialog(elements.machineDialog);
    openCredentialDialogForDiscovery(routes[0]);
  } catch (error) {
    elements.machineError.textContent = String(error.message ?? error);
    elements.machineError.hidden = false;
  }
});

elements.credentialForm.addEventListener("submit", async (event) => {
  event.preventDefault();
  elements.credentialError.hidden = true;
  const target = credentialTarget;
  if (!target) return closeDialog(elements.credentialDialog);
  elements.credentialSubmit.disabled = true;
  const password = elements.machinePassword.value;
  const remember = elements.rememberPassword.checked;
  try {
    let item;
    if (target.kind === "discovered") {
      item = await confirmDiscoveredRoutes(target.route, password);
    } else {
      const result = await probeMachine(target.item, password, false);
      if (result.availability !== "available") throw new Error(result.machine.availabilityDetail);
      item = result.machine;
    }
    if (item.availability !== "available") throw new Error(item.availabilityDetail);
    closeDialog(elements.credentialDialog);
    renderMachines();
    renderDiscoveredMachines();
    await connectMachine(item, password, remember, true);
  } catch (error) {
    elements.credentialError.textContent = String(error.message ?? error);
    elements.credentialError.hidden = false;
  } finally {
    elements.machinePassword.value = "";
    elements.credentialSubmit.disabled = false;
  }
});

elements.credentialDialog.addEventListener("close", () => {
  elements.machinePassword.value = "";
  credentialTarget = null;
});

elements.bitrate.addEventListener("input", () => {
  const value = Number(elements.bitrate.value);
  localStorage.setItem(bitrateStorageKey, String(value));
  updateBitrate(value);
});

elements.latencyDiagnostics.addEventListener("change", () => {
  localStorage.setItem(latencyDiagnosticsStorageKey, String(elements.latencyDiagnostics.checked));
});

elements.disconnect.addEventListener("click", async () => {
  if (!window.RustConsoleMachineState.playerActive(playerSession)) return;
  const previousPhase = playerSession.phase;
  playerSession.phase = "stopping";
  renderMachines();
  try {
    await invoke("disconnect");
  } catch (error) {
    playerSession.phase = previousPhase;
    renderMachines();
    showAppMessage(error);
  }
});

await listen("player-authenticated", () => {
  if (window.RustConsoleMachineState.playerActive(playerSession)) {
    playerSession.phase = "authenticated";
  }
  renderMachines();
});
await listen("discovery-progress", ({ payload }) => {
  if (payload.requestId !== activeDiscoveryRequest) return;
  const completed = payload.lanCompleted + payload.tailscaleCompleted;
  const total = payload.lanTotal + payload.tailscaleTotal;
  elements.discoveryProgress.hidden = false;
  elements.discoveryProgressBar.max = Math.max(total, 1);
  elements.discoveryProgressBar.value = completed;
});
await listen("discovery-result", ({ payload }) => {
  if (payload.requestId !== activeDiscoveryRequest) return;
  const route = discoveredRoute(payload.endpoint);
  currentDiscoveredRoutes = currentDiscoveredRoutes.filter((candidate) => candidate.endpoint !== route.endpoint);
  currentDiscoveredRoutes.push(route);
  const currentEndpoints = new Set(currentDiscoveredRoutes.map((candidate) => candidate.endpoint));
  discoveredRoutes = [
    ...previousDiscoveredRoutes.filter((candidate) => !currentEndpoints.has(candidate.endpoint)),
    ...currentDiscoveredRoutes,
  ];
  queueRememberedDiscoveryAuthentication(route, payload.requestId);
  renderDiscoveredMachines();
});
await listen("player-started", () => {
  if (window.RustConsoleMachineState.playerActive(playerSession)) {
    playerSession.phase = "streaming";
  }
  renderMachines();
});
await listen("player-ended", ({ payload }) => {
  const endedEndpoint = playerSession.endpoint;
  const endedGeneration = playerSession.generation;
  playerSession = { phase: "idle", endpoint: null, generation: endedGeneration };
  elements.disconnect.hidden = true;
  const item = machines.find((candidate) => candidate.endpoints.includes(endedEndpoint));
  if (payload) {
    showAppMessage(payload);
  }
  renderMachines();
  if (item) void probeUntilSessionReleased(item, endedGeneration);
});

updateBitrate(loadBitrate());
elements.latencyDiagnostics.checked = localStorage.getItem(latencyDiagnosticsStorageKey) === "true";
renderMachines();
setPage(location.hash === "#settings" ? "settings" : "machines");
