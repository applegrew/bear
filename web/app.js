import init, { BearWasmClient } from "../bear-wasm/pkg/bear_wasm.js";

const terminal = document.getElementById("terminal");
const serverUrlInput = document.getElementById("serverUrl");
const sessionSelect = document.getElementById("sessionSelect");
const refreshBtn = document.getElementById("refreshSessions");
const connectBtn = document.getElementById("connect");
const inputForm = document.getElementById("inputForm");
const commandInput = document.getElementById("commandInput");
const proxyMode = document.getElementById("proxyMode");
const proxyPanel = document.getElementById("proxyPanel");
const outboundQueue = document.getElementById("outboundQueue");
const proxyInbound = document.getElementById("proxyInbound");
const feedInbound = document.getElementById("feedInbound");

let wasmClient = null;
let useProxy = false;

function logLine(text, type = "info") {
  const line = document.createElement("div");
  line.textContent = text;
  line.className = `line ${type}`;
  terminal.appendChild(line);
  terminal.scrollTop = terminal.scrollHeight;
}

async function fetchSessions() {
  const serverUrl = serverUrlInput.value.trim();
  const response = await fetch(`${serverUrl}/sessions`);
  const data = await response.json();
  sessionSelect.innerHTML = "";
  data.sessions.forEach((session) => {
    const option = document.createElement("option");
    option.value = session.id;
    option.textContent = `${session.id} | ${session.cwd}`;
    sessionSelect.appendChild(option);
  });
  const newOption = document.createElement("option");
  newOption.value = "new";
  newOption.textContent = "Create new session";
  sessionSelect.appendChild(newOption);
}

async function ensureSession() {
  const serverUrl = serverUrlInput.value.trim();
  if (sessionSelect.value && sessionSelect.value !== "new") {
    return sessionSelect.value;
  }
  const response = await fetch(`${serverUrl}/sessions`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({}),
  });
  const data = await response.json();
  return data.session.id;
}

function buildWsUrl(serverUrl, sessionId) {
  const url = new URL(serverUrl);
  url.protocol = url.protocol === "https:" ? "wss:" : "ws:";
  url.pathname = `/ws/${sessionId}`;
  url.search = "";
  return url.toString();
}

async function connect() {
  const serverUrl = serverUrlInput.value.trim();
  const sessionId = await ensureSession();
  const wsUrl = buildWsUrl(serverUrl, sessionId);
  logLine(`Connecting to ${wsUrl}`);

  wasmClient = useProxy
    ? BearWasmClient.newProxy(onMessage, onError)
    : BearWasmClient.connectWebSocket(wsUrl, onMessage, onError);

  if (useProxy) {
    outboundQueue.textContent = "";
    logLine("Proxy mode enabled. Outbound messages will queue for your API.", "notice");
  }
}

function onMessage(payload) {
  try {
    const message = JSON.parse(payload);
    if (message.type === "output") {
      logLine(message.text);
    } else if (message.type === "notice") {
      logLine(`[notice] ${message.text}`, "notice");
    } else if (message.type === "error") {
      logLine(`[error] ${message.text}`, "error");
    } else if (message.type === "session_info") {
      logLine(`Connected to session ${message.session.id}`);
      logLine(`Session cwd: ${message.session.cwd}`, "notice");
    } else {
      logLine(payload);
    }
  } catch (err) {
    logLine(payload);
  }
}

function onError(message) {
  logLine(`[error] ${message}`, "error");
}

function updateProxyQueue() {
  if (!wasmClient) {
    return;
  }
  const queued = wasmClient.drainOutbound();
  if (queued.length > 0) {
    const lines = [];
    for (let i = 0; i < queued.length; i++) {
      lines.push(queued[i]);
    }
    outboundQueue.textContent += `${lines.join("\n")}\n`;
  }
}

refreshBtn.addEventListener("click", fetchSessions);
connectBtn.addEventListener("click", connect);
proxyMode.addEventListener("change", (event) => {
  useProxy = event.target.checked;
  proxyPanel.style.display = useProxy ? "block" : "none";
});

feedInbound.addEventListener("click", (event) => {
  event.preventDefault();
  if (!wasmClient) {
    return;
  }
  const payload = proxyInbound.value.trim();
  if (payload) {
    wasmClient.feedServerMessage(payload);
    proxyInbound.value = "";
  }
});

inputForm.addEventListener("submit", (event) => {
  event.preventDefault();
  if (!wasmClient) {
    logLine("Not connected yet.", "error");
    return;
  }
  const text = commandInput.value.trim();
  if (!text) {
    return;
  }
  wasmClient.sendInput(text);
  commandInput.value = "";
  if (useProxy) {
    updateProxyQueue();
  }
});

(async () => {
  await init();
  proxyPanel.style.display = "none";
  await fetchSessions();
})();
