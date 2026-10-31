const { invoke } = window.__TAURI__;
const { listen } = window.__TAURI__.event;

const mainBtn = document.getElementById("main-btn");
const statusDot = document.getElementById("status-dot");
const statusText = document.getElementById("status-text");
const strategyLabel = document.getElementById("strategy-label");
const messageEl = document.getElementById("message");
const updateBtn = document.getElementById("update-btn");
const settingsBtn = document.getElementById("settings-btn");
const settingsPanel = document.getElementById("settings-panel");
const logsBtn = document.getElementById("logs-btn");
const autostartToggle = document.getElementById("autostart-toggle");
const autoconnectToggle = document.getElementById("autoconnect-toggle");

// Статус приходит из Rust как объект: { state: "connected" | "error" | ..., message?: string }
// (см. EngineStatus в engine.rs). Раньше это была строка вида Error("..."),
// которую приходилось разбирать регуляркой — и она ломалась на кавычках.
const STATUS_MAP = {
  disconnected: { text: "Отключено", dot: "disconnected", btn: "off", label: "Подключить" },
  connecting: { text: "Подключение…", dot: "connecting", btn: "busy", label: "Отмена" },
  connected: { text: "Подключено", dot: "connected", btn: "on", label: "Отключить" },
  reconnecting: { text: "Переподключение…", dot: "connecting", btn: "busy", label: "Отмена" },
  error: { text: "Ошибка", dot: "error", btn: "off", label: "Повторить" },
};

let currentState = "disconnected";

function applyStatus(status, strategyName) {
  const state = status && status.state ? status.state : "disconnected";
  const meta = STATUS_MAP[state] || STATUS_MAP.disconnected;
  currentState = state;

  statusText.textContent = meta.text;
  statusDot.className = `dot dot--${meta.dot}`;
  mainBtn.textContent = meta.label;
  mainBtn.className = `main-btn main-btn--${meta.btn}`;
  // Кнопка не блокируется никогда: во время перебора по ней можно отменить
  // попытку, иначе окно «залипает» на несколько секунд без реакции.
  mainBtn.disabled = false;

  // Имя стратегии лежит рядом со статусом в payload ({status, strategy}),
  // а не внутри объекта статуса — поэтому передаётся отдельным аргументом.
  strategyLabel.textContent = `Стратегия: ${strategyName || "—"}`;

  const text = state === "error" && status && status.message ? status.message : "";
  messageEl.textContent = text;
  messageEl.classList.toggle("message--hidden", text === "");
}

async function refreshStatus() {
  try {
    const s = await invoke("cmd_get_status");
    applyStatus(s.status, s.strategy);
  } catch (e) {
    console.error("не получил статус:", e);
  }
}

let clickBusy = false;
mainBtn.addEventListener("click", async () => {
  if (clickBusy) return;
  clickBusy = true;

  try {
    if (["connected", "connecting", "reconnecting"].includes(currentState)) {
      await invoke("cmd_disconnect");
    } else {
      // cmd_connect отвечает только после окончания перебора стратегий,
      // поэтому не ждём его — статусы приходят событием engine-status.
      invoke("cmd_connect").catch((e) => console.error("connect:", e));
    }
  } catch (e) {
    console.error("ошибка команды:", e);
  } finally {
    clickBusy = false;
  }
});

updateBtn.addEventListener("click", async () => {
  updateBtn.disabled = true;
  updateBtn.textContent = "Обновляю…";

  // Прогресс скачивания: архив весит десятки мегабайт, без счётчика кнопка
  // выглядит зависшей на несколько минут.
  const unlisten = await listen("update-progress", (event) => {
    const mb = (event.payload.bytes / 1048576).toFixed(1);
    updateBtn.textContent = `Обновляю… ${mb} МБ`;
  });

  try {
    const list = await invoke("cmd_update_strategies");
    // source_tag — тег релиза GitHub; сразу видно, какая версия применена.
    const tag = list.source_tag ? ` ${list.source_tag}` : "";
    updateBtn.textContent = `Готово:${tag} · ${list.strategies.length}`;
    updateBtn.title = `Применён релиз ${list.source_tag || "неизвестно"}, стратегий: ${list.strategies.length}`;
  } catch (e) {
    updateBtn.textContent = "Не обновилось";
    console.error("update:", e);
  } finally {
    unlisten();
    setTimeout(() => {
      updateBtn.textContent = "Обновить стратегии";
      updateBtn.disabled = false;
    }, 2500);
  }
});

settingsBtn.addEventListener("click", () => {
  settingsPanel.classList.toggle("settings-panel--hidden");
});

logsBtn.addEventListener("click", async () => {
  try {
    await invoke("cmd_open_logs");
  } catch (e) {
    console.error("не удалось открыть логи:", e);
  }
});

autostartToggle.addEventListener("change", async () => {
  try {
    await invoke("cmd_set_autostart", { enabled: autostartToggle.checked });
  } catch (e) {
    // Не даём UI показывать состояние, которого нет на самом деле.
    autostartToggle.checked = !autostartToggle.checked;
    console.error("autostart:", e);
  }
});

autoconnectToggle.addEventListener("change", async () => {
  try {
    await invoke("cmd_set_autoconnect", { enabled: autoconnectToggle.checked });
  } catch (e) {
    autoconnectToggle.checked = !autoconnectToggle.checked;
    console.error("autoconnect:", e);
  }
});

async function init() {
  try {
    autostartToggle.checked = await invoke("cmd_get_autostart");
    autoconnectToggle.checked = await invoke("cmd_get_autoconnect");
  } catch (e) {
    console.error("настройки:", e);
  }

  // Подписка на события до первой синхронизации, иначе можно потерять
  // статус, отправленный бэкендом сразу после старта окна.
  await listen("engine-status", (event) =>
    applyStatus(event.payload.status, event.payload.strategy)
  );
  await refreshStatus();
}

init();
