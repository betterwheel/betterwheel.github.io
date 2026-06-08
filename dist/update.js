// BetterWheel desktop — auto-update via tauri-plugin-updater (modeled on
// marie-lookapp). Checks latest.json on the public releases repo at launch and
// hourly; a found update shows a banner. Install downloads the signed artifact
// and relaunches into the new version.

const UPDATE_PROMPT_KEY = "tw-update-prompt-last";
const CHECK_INTERVAL_MS = 60 * 60 * 1000; // hourly background check
const PROMPT_INTERVAL_MS = 24 * 60 * 60 * 1000; // once per day per version

const $banner = document.getElementById("update-banner");
const $text = document.getElementById("update-text");
const $install = document.getElementById("update-install");
const $later = document.getElementById("update-later");
const $check = document.getElementById("check-updates");
const $version = document.getElementById("app-version");

let pendingUpdate = null;

async function checkForUpdate() {
  const updater = window.__TAURI__.updater;
  if (!updater) return false;
  // Drop the previous check's plugin-side resource before re-checking.
  if (pendingUpdate) {
    try { await pendingUpdate.close(); } catch (_) {}
    pendingUpdate = null;
  }
  try {
    pendingUpdate = await updater.check();
  } catch (_) {
    return false; // offline, endpoint unreachable, or no artifact for this platform
  }
  if (!pendingUpdate) return false;
  $text.textContent = `v${pendingUpdate.version} available (you have ${pendingUpdate.currentVersion})`;
  $banner.classList.remove("hidden");
  return true;
}

function promptAllowed(version) {
  try {
    const last = JSON.parse(localStorage.getItem(UPDATE_PROMPT_KEY) || "{}");
    if (last.version !== version) return true; // new release → show now
    return Date.now() - last.ts > PROMPT_INTERVAL_MS;
  } catch (_) {
    return true;
  }
}

async function maybePrompt() {
  if (!(await checkForUpdate())) return;
  if (!promptAllowed(pendingUpdate.version)) {
    $banner.classList.add("hidden");
    return;
  }
  localStorage.setItem(
    UPDATE_PROMPT_KEY,
    JSON.stringify({ version: pendingUpdate.version, ts: Date.now() })
  );
}

$later.addEventListener("click", () => $banner.classList.add("hidden"));

$install.addEventListener("click", async () => {
  if (!pendingUpdate) return;
  $install.disabled = true;
  $later.disabled = true;
  let total = 0;
  let got = 0;
  try {
    await pendingUpdate.downloadAndInstall((ev) => {
      if (ev.event === "Started") {
        total = ev.data.contentLength || 0;
        $text.textContent = "Downloading update…";
      } else if (ev.event === "Progress") {
        got += ev.data.chunkLength;
        const pct = total ? ` ${Math.round((got / total) * 100)}%` : "";
        $text.textContent = `Downloading update…${pct}`;
      } else if (ev.event === "Finished") {
        $text.textContent = "Installing — restarting…";
      }
    });
    // Windows: the installer exits + relaunches us. macOS: the new .app is on
    // disk but this process keeps running, so relaunch into it ourselves.
    await window.__TAURI__.process.relaunch();
  } catch (err) {
    $text.textContent = `Update failed: ${err}`;
    $install.disabled = false;
    $later.disabled = false;
  }
});

// Explicit user check — bypasses the daily cap and reports "up to date".
$check.addEventListener("click", async () => {
  const dialog = window.__TAURI__.dialog;
  const found = await checkForUpdate();
  if (found) {
    localStorage.setItem(
      UPDATE_PROMPT_KEY,
      JSON.stringify({ version: pendingUpdate.version, ts: Date.now() })
    );
  } else if (dialog) {
    await dialog.message("You're running the latest version.", { title: "BetterWheel", kind: "info" });
  }
});

(async function init() {
  try {
    $version.textContent = "v" + (await window.__TAURI__.app.getVersion());
  } catch (_) {}
  maybePrompt();
  setInterval(maybePrompt, CHECK_INTERVAL_MS);
})();
