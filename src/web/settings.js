
(function () {
function boot() {
const $ = (selector, root = document) => root.querySelector(selector);
const $$ = (selector, root = document) => Array.from(root.querySelectorAll(selector));

let state = null;
let lastNotice = 0;
let hotkeyCapture = false;
let lastModels = "";
let lastWords = "";
let lastHistory = "";
let level = 0;
let tab = "control";

const LABELS = {
  alt_r: "Right Alt", alt_l: "Left Alt", alt: "Alt",
  ctrl_r: "Right Ctrl", ctrl_l: "Left Ctrl", ctrl: "Ctrl",
  shift_r: "Right Shift", shift_l: "Left Shift", shift: "Shift",
  cmd_l: "Left Win", cmd_r: "Right Win", cmd: "Win",
  esc: "Esc", enter: "Enter", space: "Space", tab: "Tab"
};

// Rust 側へ JSON コマンドを送る。
function ipc(payload) {
  window.ipc.postMessage(JSON.stringify(payload));
}

// 設定を楽観的に反映してから保存を依頼する。
function save(patch) {
  if (!state) return;
  state.settings = Object.assign({}, state.settings, patch);
  ipc({ cmd: "save_settings", settings: state.settings });
  render();
}

function esc(value) {
  return String(value == null ? "" : value)
    .replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;").replace(/'/g, "&#39;");
}

function keyLabel(value) {
  if (LABELS[value]) return LABELS[value];
  if (/^f\d{1,2}$/.test(value)) return value.toUpperCase();
  return value.replace(/_/g, " ");
}

function dotRow(value, max) {
  let html = '<span class="dots">';
  for (let i = 1; i <= max; i += 1) {
    html += `<i class="${i <= value ? "on" : ""}"></i>`;
  }
  return html + "</span>";
}

// 数値を桁区切りで表示する。
function formatCount(value) {
  const count = Math.max(0, Math.round(Number(value) || 0));
  return count.toLocaleString("ja-JP");
}

// 200 文字/分で換算した削減時間を読みやすい単位にする。
function savedTimeLabel(minutes) {
  const value = Number(minutes) || 0;
  if (value <= 0) return "まだ実績はありません";
  if (value < 60) return `約 ${Math.max(1, Math.round(value))} 分のタイピング削減`;
  return `約 ${(value / 60).toFixed(1)} 時間のタイピング削減`;
}

// 累積文字数・発話回数・当日分・削減時間を表示する。
function renderStats() {
  if (!state) return;
  const stats = state.stats || {};
  const totalChars = Number(stats.totalChars) || 0;
  $("#statTotalChars").textContent = formatCount(totalChars);
  $("#statTotalChars").title = `総音声入力: ${formatCount(totalChars)} 文字`;
  $("#statSessions").textContent = `${formatCount(stats.totalSessions)} 回`;
  $("#statTodayChars").textContent = formatCount(stats.todayChars);
  $("#statSaved").textContent = savedTimeLabel(stats.estimatedSavedMinutes);
}

function icon(name) {
  const icons = {
    model: '<path d="m12 3 9 5-9 5-9-5 9-5Z"/><path d="m3 13 9 5 9-5"/>',
    download: '<path d="M12 3v12"/><path d="m7 10 5 5 5-5"/><path d="M5 21h14"/>',
    trash: '<path d="M3 6h18"/><path d="M8 6V4h8v2"/><path d="m6 6 1 15h10l1-15"/>',
    check: '<path d="M20 6 9 17l-5-5"/>',
    copy: '<rect x="9" y="9" width="13" height="13" rx="2"/><path d="M5 15H4a2 2 0 0 1-2-2V4a2 2 0 0 1 2-2h9a2 2 0 0 1 2 2v1"/>',
    close: '<path d="M6 6l12 12M18 6 6 18"/>',
    spin: '<path d="M12 3a9 9 0 1 0 9 9"/>'
  };
  return `<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.9" stroke-linecap="round" stroke-linejoin="round">${icons[name] || ""}</svg>`;
}

// 録音・推論・貼り付けの状態とエンジン状態を表示する。
function setStatus() {
  if (!state) return;
  const visual = state.visual || "idle";
  const dot = $("#statusDot");
  const text = $("#statusText");
  dot.className = "status-dot";
  if (visual === "recording") {
    dot.classList.add("live");
    text.textContent = "録音中";
  } else if (visual === "processing" || visual === "pasting") {
    dot.classList.add("busy");
    text.textContent = visual === "pasting" ? "貼り付け中" : "推論中";
  } else {
    text.textContent = "待機中";
  }

  const badge = $("#engineBadge");
  badge.className = "status-badge";
  if (state.engine.ready) {
    badge.textContent = "準備完了";
    badge.classList.add("ok");
  } else if (state.engine.building) {
    badge.textContent = "読み込み中";
    badge.classList.add("busy");
  } else {
    badge.textContent = "モデル未準備";
  }

  const model = (state.models || []).find(item => item.id === state.settings.model);
  $("#heroModel").textContent = model ? model.name : state.settings.model;
  $("#heroHotkey").textContent = keyLabel(state.settings.hotkey);
  $("#heroMode").textContent = "長押し / 2回押し";

  const caption = {
    idle: "待機中 — ホットキーを押すと波形が動きます",
    recording: "録音中 — マイクに追従しています",
    processing: "推論中 — 中央に収束しています",
    pasting: "貼り付け中"
  };
  $("#previewCaption").textContent = caption[visual] || caption.idle;
}

function setHotkeyBox() {
  if (!state || hotkeyCapture) return;
  $("#hotkeyValue").textContent = keyLabel(state.settings.hotkey);
  $("#hotkeyHint").textContent = "クリックしてキーを押してください";
}

function friendlyModelError(error) {
  const text = String(error || "");
  if (/curl\s+-LO|https:\/\/github\.com\/k2-fsa/.test(text)) {
    return "このモデルは未ダウンロードです。下の「ダウンロード」ボタンから追加してください。";
  }
  return text;
}

// モデル一覧を進捗・選択状態つきで描画する。
function renderModels() {
  const grid = $("#modelGrid");
  if (!state) return;
  // 選択モデルも再描画条件に含める。これが無いと選択枠が移動しない。
  const signature = `${state.settings.model}|${JSON.stringify(state.models || [])}`;
  if (signature !== lastModels) {
    lastModels = signature;
    grid.innerHTML = (state.models || []).map(model => {
      const selected = model.id === state.settings.model;
      const active = model.installed && selected;
      const download = model.download || {};
      const busy = download.phase === "downloading"
        || download.phase === "verifying"
        || download.phase === "extracting";
      const percent = download.total > 0
        ? Math.min(100, (download.downloaded / download.total) * 100)
        : 0;
      const barPercent = download.phase === "downloading" ? percent : 100;
      const phaseLabel = download.phase === "verifying"
        ? "検証中…"
        : download.phase === "extracting" ? "展開中…" : Math.round(percent) + "%";
      let action = "";
      if (busy) {
        action = `<span class="size">${phaseLabel}</span>`;
      } else if (model.installed) {
        action = `<button class="btn danger icon" data-delete="${esc(model.id)}" ${active ? "disabled" : ""} title="ストレージを解放">${icon("trash")}</button>`;
      } else {
        action = `<button class="btn primary" data-download="${esc(model.id)}">${icon("download")}ダウンロード</button>`;
      }
      const progress = busy
        ? `<div class="progress"><i style="width:${barPercent}%"></i></div>
           <div class="progress-note"><span>${download.phase === "extracting" ? "アーカイブを展開しています" : download.phase === "verifying" ? "SHA-256 を検証しています" : "ダウンロード中"}</span><span>${Math.round(percent)}%</span></div>`
        : "";
      const failed = download.phase === "failed" && download.error
        ? `<div class="progress-note" style="color:#be123c"><span>${esc(download.error)}</span></div>`
        : "";
      return `
        <article class="card model-card ${selected ? "selected" : ""}" data-model="${esc(model.id)}">
          <div class="model-top">
            <div class="model-icon">${icon("model")}</div>
            <div style="flex:1">
              <div class="model-name">${esc(model.name)}
                ${model.recommended ? '<span class="badge recommend">★ 推奨（0.2秒・最速）</span>' : ""}
                ${model.installed ? '<span class="badge green">DL済み</span>' : (busy ? "" : '<span class="badge warn">未ダウンロード</span>')}
              </div>
              <div class="model-sub">${esc(model.subtitle)}</div>
            </div>
            <div class="model-meta" style="flex:none">
              <span class="metric">速度 ${dotRow(model.speed, 5)}</span>
              <span class="metric">精度 ${dotRow(model.accuracy, 5)}</span>
            </div>
          </div>
          ${progress}
          ${failed}
          <div class="model-foot">
            <span class="size">${esc(model.size)}${model.installed ? " · " + esc(model.installedLabel) : ""}</span>
            <div style="display:flex;gap:6px;align-items:center">${action}</div>
          </div>
        </article>`;
    }).join("");
  }

  const banner = $("#modelBanner");
  if (state.engine.error && !state.engine.ready) {
    banner.classList.add("show");
    $("#modelBannerText").textContent = state.engine.building
      ? "モデルを読み込んでいます…"
      : friendlyModelError(state.engine.error);
  } else {
    banner.classList.remove("show");
  }
}

// 入力デバイスの選択肢とマイクエラーを更新する。
function renderDevices() {
  if (!state) return;
  const select = $("#deviceSelect");
  const devices = state.devices || [];
  const defaultDevice = state.defaultDevice || "";
  const current = state.settings.device == null ? "" : state.settings.device;
  const signature = JSON.stringify({ devices, defaultDevice }) + "|" + current;
  if (select.dataset.signature === signature) return;
  select.dataset.signature = signature;
  const defaultLabel = defaultDevice
    ? `システムの既定 (現在: ${defaultDevice})`
    : "システムの既定";
  let options = `<option value="">${esc(defaultLabel)}</option>`;
  let found = false;
  if (devices.length) {
    options += `<optgroup label="特定のデバイスに固定">`;
    for (const device of devices) {
      const selected = device.value === current;
      if (selected) found = true;
      const label = device.label + (device.isDefault ? "（システム既定）" : "");
      options += `<option value="${esc(device.value)}" ${selected ? "selected" : ""}>${esc(label)}</option>`;
    }
    options += `</optgroup>`;
  }
  if (current && !found) {
    options += `<option value="${esc(current)}" selected>現在の指定: ${esc(current)}</option>`;
  }
  select.innerHTML = options;

  const banner = $("#micBanner");
  if (state.captureError) {
    banner.classList.add("show");
    $("#micBannerText").textContent = state.captureError;
  } else {
    banner.classList.remove("show");
  }
}

// 単語辞書の一覧を描画する。
function renderWords() {
  if (!state) return;
  const words = state.words || [];
  $("#wordCount").textContent = `${words.length} 語`;
  const signature = JSON.stringify(words);
  if (signature === lastWords) return;
  lastWords = signature;
  if (!words.length) {
    $("#wordList").innerHTML = `<div class="empty">まだ単語がありません。<br>上のフォームから追加できます。</div>`;
    return;
  }
  $("#wordList").innerHTML = words.map(word => `
    <div class="word-row">
      <span class="word-target">${esc(word.target)}</span>
      <span class="word-reading">${esc(word.reading || "—")}</span>
      <button class="word-del" data-target="${esc(word.target)}" data-reading="${esc(word.reading || "")}" aria-label="削除">${icon("close")}</button>
    </div>`).join("");
}

// 認識履歴を新しい順に描画する。
function renderHistory() {
  if (!state) return;
  const history = state.history || [];
  const signature = JSON.stringify(history);
  if (signature === lastHistory) return;
  lastHistory = signature;
  $("#clearHistory").disabled = !history.length;
  if (!history.length) {
    $("#historyList").innerHTML = `<div class="empty">まだ履歴がありません。<br>音声入力を確定すると、ここに直近10件が表示されます。</div>`;
    return;
  }
  $("#historyList").innerHTML = history.map((entry, index) => `
    <div class="history-row">
      <div class="history-main">
        <p class="history-text">${esc(entry.text)}</p>
        <div class="history-meta">
          <span>${esc(entry.time)}</span>
          <span class="history-model">${esc(entry.model)}</span>
        </div>
      </div>
      <button class="btn icon history-copy" data-copy-history="${index}" title="クリップボードへコピー" aria-label="コピー">${icon("copy")}</button>
    </div>`).join("");
}

// トグル・音量・辞書パスなどのコントロールを同期する。
function renderControls() {
  if (!state) return;
  const settings = state.settings;
  setHotkeyBox();
  $$("#themeGrid .theme-card").forEach(card => {
    card.classList.toggle("selected", card.dataset.theme === settings.sound_theme);
  });
  $("#autostartToggle").classList.toggle("on", !!settings.autostart);
  $("#floatingToggle").classList.toggle("on", !!settings.floating_bar);
  $("#soundStartToggle").classList.toggle("on", !!settings.sound_start);
  $("#soundFinishToggle").classList.toggle("on", !!settings.sound_finish);
  if (document.activeElement !== $("#volume")) {
    $("#volume").value = String(settings.sound_volume);
  }
  $("#volumeValue").textContent = `${Math.round(settings.sound_volume * 100)}%`;

  const paths = state.paths || {};
  $("#dictPaths").innerHTML = `<b>words.txt</b> ${esc(paths.words || "")}<br><b>config.toml</b> ${esc(paths.config || "")}`;
  $("#aboutPaths").innerHTML = `<b>設定</b> ${esc(paths.config || "")}<br><b>モデル</b> ${esc(paths.models || "")}<br><b>単語辞書</b> ${esc(paths.words || "")}`;
  const sourceLine = paths.modelSource
    ? `<b>配布元</b> <span title="${esc(paths.modelSource)}">${esc(paths.modelSource)}</span>`
    : `<b>配布元</b> <span>公式配布元（sherpa-onnx GitHub / Hugging Face）</span>`;
  $("#modelPaths").innerHTML = paths.models
    ? `<b>ダウンロード先</b> <span title="${esc(paths.models)}">${esc(paths.models)}</span><br>${sourceLine}`
    : sourceLine;
  $("#configPath").textContent = paths.config || "";
  $("#version").textContent = `v${state.version || "0.1.5"}`;
}

// 受信した状態を各セクションへ反映する。
function render() {
  if (!state) return;
  setStatus();
  renderStats();
  renderControls();
  renderModels();
  renderHistory();
  renderDevices();
  renderWords();
  if (state.notice && state.notice.id !== lastNotice) {
    lastNotice = state.notice.id;
    toast(state.notice.message, state.notice.kind);
  }
  if (state.engine.error && state.visual === "idle") {
    $("#statusDot").title = state.engine.error;
  }
}

function toast(message, kind) {
  const node = $("#toast");
  node.textContent = message;
  node.className = "show " + (kind || "info");
  clearTimeout(node._timer);
  node._timer = setTimeout(() => { node.className = ""; }, 2600);
}

function levelPercent(value) {
  return Math.min(1, Math.sqrt(Math.max(0, value) * 9));
}

// 入力レベルをメーターと波形へ反映する。
function applyLevel(value) {
  level = value || 0;
  const percent = levelPercent(level);
  $("#levelFill").style.width = `${(percent * 100).toFixed(1)}%`;
  $("#levelValue").textContent = `${Math.round(percent * 100)}%`;
  const bars = $$("#meterBars span");
  const now = performance.now() / 90;
  bars.forEach((bar, index) => {
    const wobble = Math.sin(now + index * 0.85) * 0.5 + 0.5;
    const height = 8 + (percent * 74) * (0.5 + 0.5 * wobble);
    bar.style.height = `${height.toFixed(1)}%`;
  });
}

// プレビューの波形を状態に合わせて毎フレーム動かす。
function animatePreview() {
  const bars = $$("#previewPill span");
  const visual = state ? state.visual : "idle";
  const now = performance.now() / 100;
  const normalized = levelPercent(level);
  bars.forEach((bar, index) => {
    let height = 8;
    if (visual === "recording") {
      const weight = [0.55, 0.82, 1, 0.76, 0.6][index];
      const wobble = Math.sin(now + index * 0.9) * 0.5 + 0.5;
      height = 8 + (5 + normalized * 26) * weight * (0.7 + 0.3 * wobble);
    } else if (visual === "processing" || visual === "pasting") {
      const pulse = Math.sin(performance.now() / 130) * 0.5 + 0.5;
      height = 8 + pulse * 13;
    }
    bar.style.height = `${height.toFixed(1)}px`;
  });
  requestAnimationFrame(animatePreview);
}

window.__barevoice = {
  apply(next) {
    state = next;
    render();
  },
  level(value) {
    applyLevel(value);
  },
  openTab(name) {
    activateTab(name);
  }
};

function activateTab(name) {
  const button = $$(".tab").find(item => item.dataset.tab === name);
  if (!button) return;
  tab = name;
  $$(".tab").forEach(item => item.classList.toggle("active", item === button));
  $$(".panel").forEach(panel => panel.classList.toggle("active", panel.dataset.panel === tab));
  $("#app").scrollTo({ top: 0, behavior: "smooth" });
}

$$(".tab").forEach(button => {
  button.addEventListener("click", () => activateTab(button.dataset.tab));
});

$("#hotkeyBox").addEventListener("click", () => {
  hotkeyCapture = true;
  $("#hotkeyBox").classList.add("listening");
  $("#hotkeyHint").textContent = "キーを押してください…（Esc で取消）";
});

// KeyboardEvent.code を Rust 側のキー名へ変換する。
function mapKey(event) {
  const code = event.code || "";
  const map = {
    AltRight: "alt_r", AltLeft: "alt_l",
    ControlRight: "ctrl_r", ControlLeft: "ctrl_l",
    ShiftRight: "shift_r", ShiftLeft: "shift_l",
    MetaRight: "cmd_r", MetaLeft: "cmd_l",
    Escape: "esc", Enter: "enter", Space: "space", Tab: "tab"
  };
  if (map[code]) return map[code];
  if (/^F([1-9]|1[0-2])$/.test(code)) return code.toLowerCase();
  const letter = code.match(/^Key([A-Z])$/);
  if (letter) return letter[1].toLowerCase();
  const digit = code.match(/^Digit([0-9])$/);
  if (digit) return digit[1];
  return null;
}

window.addEventListener("keydown", event => {
  if (!hotkeyCapture) return;
  event.preventDefault();
  event.stopPropagation();
  if (event.key === "Escape") {
    hotkeyCapture = false;
    $("#hotkeyBox").classList.remove("listening");
    setHotkeyBox();
    return;
  }
  const key = mapKey(event);
  if (!key) return;
  hotkeyCapture = false;
  $("#hotkeyBox").classList.remove("listening");
  save({ hotkey: key });
}, true);

$("#themeGrid").addEventListener("click", event => {
  const card = event.target.closest("[data-theme]");
  if (!card) return;
  const play = event.target.closest("[data-play]");
  if (play) {
    ipc({ cmd: "play_sound", kind: play.dataset.play, theme: card.dataset.theme });
    return;
  }
  save({ sound_theme: card.dataset.theme });
});

$("#themeGrid").addEventListener("keydown", event => {
  if (event.key !== "Enter" && event.key !== " ") return;
  const card = event.target.closest("[data-theme]");
  if (!card) return;
  event.preventDefault();
  save({ sound_theme: card.dataset.theme });
});

$("#autostartToggle").addEventListener("click", () => {
  if (!state) return;
  save({ autostart: !state.settings.autostart });
});

$("#floatingToggle").addEventListener("click", () => {
  if (!state) return;
  save({ floating_bar: !state.settings.floating_bar });
});

$("#deviceSelect").addEventListener("change", event => {
  const value = event.target.value;
  save({ device: value === "" ? null : value });
});

$("#wordForm").addEventListener("submit", event => {
  event.preventDefault();
  const target = $("#wordTarget").value.trim();
  const reading = $("#wordReading").value.trim();
  if (!target || !reading) return;
  ipc({ cmd: "add_word", target, reading });
  $("#wordTarget").value = "";
  $("#wordReading").value = "";
  $("#wordTarget").focus();
});

$("#wordList").addEventListener("click", event => {
  const button = event.target.closest("button[data-target]");
  if (!button) return;
  ipc({
    cmd: "delete_word",
    target: button.dataset.target,
    reading: button.dataset.reading || null
  });
});

$("#historyList").addEventListener("click", event => {
  const button = event.target.closest("button[data-copy-history]");
  if (!button) return;
  const row = button.closest(".history-row");
  const text = row && row.querySelector(".history-text")
    ? row.querySelector(".history-text").textContent
    : "";
  if (!text) return;
  ipc({ cmd: "copy_history", text });
});

$("#clearHistory").addEventListener("click", () => {
  if (!state || !(state.history || []).length) return;
  if (confirm("認識履歴をすべて削除しますか？")) {
    ipc({ cmd: "clear_history" });
  }
});

$("#modelGrid").addEventListener("click", event => {
  const download = event.target.closest("button[data-download]");
  if (download) {
    event.stopPropagation();
    ipc({ cmd: "download_model", id: download.dataset.download });
    return;
  }
  const remove = event.target.closest("button[data-delete]");
  if (remove) {
    event.stopPropagation();
    if (confirm("このモデルを削除してストレージを解放しますか？")) {
      ipc({ cmd: "delete_model", id: remove.dataset.delete });
    }
    return;
  }
  const card = event.target.closest("[data-model]");
  if (!card || !state) return;
  const model = (state.models || []).find(item => item.id === card.dataset.model);
  if (!model) return;
  if (!model.installed) {
    // 未ダウンロードのカードは、そのままクリックで取得を開始する。
    ipc({ cmd: "download_model", id: model.id });
    return;
  }
  if (card.dataset.model !== state.settings.model) {
    save({ model: card.dataset.model });
  }
});

$("#soundStartToggle").addEventListener("click", () => {
  if (!state) return;
  save({ sound_start: !state.settings.sound_start });
});
$("#soundFinishToggle").addEventListener("click", () => {
  if (!state) return;
  save({ sound_finish: !state.settings.sound_finish });
});

$("#volume").addEventListener("input", event => {
  const value = Number(event.target.value);
  $("#volumeValue").textContent = `${Math.round(value * 100)}%`;
});
$("#volume").addEventListener("change", event => {
  save({ sound_volume: Number(event.target.value) });
});

$("#previewStart").addEventListener("click", () => ipc({ cmd: "play_sound", kind: "start" }));
$("#previewFinish").addEventListener("click", () => ipc({ cmd: "play_sound", kind: "finish" }));

$("#openWords").addEventListener("click", event => {
  event.preventDefault();
  ipc({ cmd: "open_words" });
});
$("#openReplacements").addEventListener("click", event => {
  event.preventDefault();
  ipc({ cmd: "open_replacements" });
});

$("#minBtn").addEventListener("click", () => ipc({ cmd: "window", action: "minimize" }));
$("#closeBtn").addEventListener("click", () => ipc({ cmd: "window", action: "close" }));

$("#dragArea").addEventListener("mousedown", event => {
  if (event.button !== 0) return;
  ipc({ cmd: "window", action: "drag" });
});

animatePreview();
}
if (document.readyState === "loading") {
  document.addEventListener("DOMContentLoaded", boot, { once: true });
} else {
  boot();
}
})();
