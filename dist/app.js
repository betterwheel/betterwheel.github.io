// BetterWheel desktop — renders the live App snapshot and drives the
// preview → arm → execute order flow through the backend (which runs the same
// guardrailed code as the TUI). Vanilla JS over window.__TAURI__; no build step.

const { invoke } = window.__TAURI__.core;
const $app = document.getElementById("app");

function money(v) {
  if (v == null) return "—";
  return Math.abs(v) >= 1000 ? "$" + Math.round(v) : "$" + v.toFixed(2);
}
const price = (v) => (v == null ? "—" : v.toFixed(2));
const esc = (s) =>
  String(s).replace(/[&<>"]/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" }[c]));

// ---- inline-SVG payoff chart (no chart lib, CSP-safe) ----
function svgPayoff(xs, ys) {
  if (!xs || xs.length < 2) return "";
  const W = 360, H = 150, padL = 6, padR = 6, padT = 8, padB = 14;
  const xmin = xs[0], xmax = xs[xs.length - 1];
  let ymin = Math.min.apply(null, ys.concat(0));
  let ymax = Math.max.apply(null, ys.concat(0));
  if (ymin === ymax) { ymin -= 1; ymax += 1; }
  const sx = (x) => padL + ((x - xmin) / (xmax - xmin)) * (W - padL - padR);
  const sy = (y) => padT + (1 - (y - ymin) / (ymax - ymin)) * (H - padT - padB);
  const zeroY = sy(0).toFixed(1);
  let line = "", area = "M" + sx(xs[0]).toFixed(1) + " " + zeroY + " ";
  for (let i = 0; i < xs.length; i++) {
    const x = sx(xs[i]).toFixed(1), y = sy(ys[i]).toFixed(1);
    line += (i ? "L" : "M") + x + " " + y + " ";
    area += "L" + x + " " + y + " ";
  }
  area += "L" + sx(xs[xs.length - 1]).toFixed(1) + " " + zeroY + " Z";
  return (
    `<svg viewBox="0 0 ${W} ${H}" class="chart" preserveAspectRatio="none">` +
    `<path d="${area}" class="pf-area"/>` +
    `<line x1="${padL}" y1="${zeroY}" x2="${W - padR}" y2="${zeroY}" class="pf-zero"/>` +
    `<path d="${line}" class="pf-line"/></svg>`
  );
}

// ---- the order controls (mode / arm / live-confirm / status) ----
function controlBar(snap) {
  const arm = snap.armed
    ? `<button class="armbtn on" data-act="arm" data-on="0">⚡ ARMED — disarm</button>`
    : `<button class="armbtn" data-act="arm" data-on="1">Arm</button>`;
  let live = "";
  if (snap.needs_live_confirm) {
    live =
      `<span class="liveconf">LIVE locked —` +
      `<input id="live-phrase" placeholder="LIVE" autocomplete="off" spellcheck="false" />` +
      `<button class="mini" data-act="confirm-live">Confirm</button></span>`;
  }
  const auto = snap.auto_trading > 0 ? `<span class="autotag">⚡ ${snap.auto_trading} AUTO-TRADING</span>` : "";
  return (
    `<div class="controls${snap.armed ? " is-armed" : ""}">` +
    `<span class="mode ${esc(snap.mode)}">${esc(snap.mode)}</span>` +
    arm + live + auto +
    `<span class="spacer"></span>` +
    `<button class="mini" data-act="refresh">Refresh</button>` +
    `</div>`
  );
}

function statusBar(snap) {
  const dot = snap.connected ? "dot live" : "dot off";
  const conn = snap.connected ? "live" : "offline";
  let acct = "";
  if (snap.account) {
    acct =
      `<span class="kv"><b>net liq</b> ${money(snap.account.net_liq)}</span>` +
      `<span class="kv"><b>cash</b> ${money(snap.account.cash)}</span>` +
      `<span class="kv"><b>buying power</b> ${money(snap.account.buying_power)}</span>`;
  }
  return (
    `<div class="status"><span class="${dot}"></span><span class="conn">${conn}</span>${acct}` +
    `<span class="spacer"></span><span class="updated">updated ${esc(snap.updated)}</span></div>`
  );
}

function acts(list, i, armed) {
  return (
    `<button class="mini" data-act="preview" data-list="${list}" data-index="${i}">Preview</button>` +
    `<button class="mini exec" data-act="execute" data-list="${list}" data-index="${i}"${armed ? "" : " disabled"}>Execute</button>`
  );
}

function zerodteCard(slot, idx, armed) {
  const s = slot.suggestion;
  if (!s) {
    return `<div class="card"><div class="card-head">${esc(slot.name)}</div>` +
      `<div class="empty">no structure fits right now</div></div>`;
  }
  let rows =
    `<tr><td>Credit</td><td class="num">${money(s.premium_total)}</td></tr>` +
    `<tr><td>Max loss</td><td class="num">${money(s.capital_required)}</td></tr>`;
  const be = slot.breakevens || [];
  if (be.length >= 2) rows += `<tr><td>Breakevens</td><td class="num">${price(be[0])} / ${price(be[be.length - 1])}</td></tr>`;
  else if (be.length === 1) rows += `<tr><td>Breakeven</td><td class="num">${price(be[0])}</td></tr>`;
  if (slot.pop != null) rows += `<tr><td>Est. win</td><td class="num">${Math.round(slot.pop * 100)}%</td></tr>`;
  return (
    `<div class="card"><div class="card-head">${esc(slot.name)}</div>` +
    `<div class="card-sym">${esc(s.symbol)} · ${esc(s.action)} · ${s.dte}DTE × ${s.quantity}</div>` +
    `<table class="mini">${rows}</table>` +
    svgPayoff(slot.payoff_xs, slot.payoff_ys) +
    `<div class="pf-cap">P&amp;L at expiry vs. underlying</div>` +
    `<div class="cardacts">${acts("zerodte", idx, armed)}</div></div>`
  );
}

function suggestionTable(rows, list, armed) {
  if (!rows || !rows.length) return `<div class="empty">nothing ranked right now</div>`;
  const body = rows
    .map(
      (s, i) =>
        `<tr><td>${esc(s.symbol)}</td><td>${esc(s.action)}</td><td>${price(s.strike)}</td>` +
        `<td>${esc(s.expiry)}</td><td class="num">${s.dte}</td><td class="num">${s.quantity}</td>` +
        `<td class="num">${money(s.premium_total)}</td><td class="num">${Math.round(s.annualized_yield * 100)}%</td>` +
        `<td class="num">${s.delta == null ? "—" : s.delta.toFixed(2)}</td>` +
        `<td class="acts">${acts(list, i, armed)}</td></tr>`
    )
    .join("");
  return (
    `<table><thead><tr><th>Symbol</th><th>Action</th><th>Strike</th><th>Exp</th>` +
    `<th class="num">DTE</th><th class="num">Qty</th><th class="num">Credit</th>` +
    `<th class="num">Ann.Yield</th><th class="num">Δ</th><th></th></tr></thead><tbody>${body}</tbody></table>`
  );
}

function positionsTable(rows) {
  if (!rows || !rows.length) return `<div class="empty">no open wheel positions</div>`;
  const body = rows
    .map(
      (p) =>
        `<tr><td>${esc(p.symbol)}</td><td>${esc(p.state)}</td><td class="num">${p.shares}</td>` +
        `<td class="num">${money(p.cost_basis)}</td><td class="num">${money(p.premium)}</td></tr>`
    )
    .join("");
  return (
    `<table><thead><tr><th>Symbol</th><th>State</th><th class="num">Shares</th>` +
    `<th class="num">Cost basis</th><th class="num">Premium collected</th></tr></thead><tbody>${body}</tbody></table>`
  );
}

function statusTag(s) {
  if (s === "filled") return "tag ok";
  if (s === "submitted") return "tag work";
  if (s === "rejected" || s === "cancelled") return "tag bad";
  return "tag";
}
const shortTs = (ts) => (String(ts).split("T")[1] || ts).slice(0, 5);
function journalTable(rows) {
  if (!rows || !rows.length) return `<div class="empty">no journal entries yet</div>`;
  const body = rows
    .slice(0, 40)
    .map(
      (j) =>
        `<tr><td class="dim">${esc(shortTs(j.ts))}</td><td>${esc(j.symbol)}</td><td>${esc(j.action)}</td>` +
        `<td class="num">${j.strike == null ? "—" : price(j.strike)}</td><td class="num">${j.quantity}</td>` +
        `<td><span class="${statusTag(j.status)}">${esc(j.status)}</span></td></tr>`
    )
    .join("");
  return (
    `<table><thead><tr><th>Time</th><th>Symbol</th><th>Action</th>` +
    `<th class="num">Strike</th><th class="num">Qty</th><th>Status</th></tr></thead><tbody>${body}</tbody></table>`
  );
}

function render(snap) {
  const liveVal = (document.getElementById("live-phrase") || {}).value || "";
  const armed = snap.armed;
  const note = snap.note ? `<div class="note">${esc(snap.note)}</div>` : "";
  const armedBanner = armed
    ? `<div class="armbanner">⚡ ARMED — Execute transmits a REAL order. Press Disarm to stand down.</div>`
    : "";
  const grid = (snap.zerodte || []).map((slot, i) => zerodteCard(slot, i, armed)).join("");
  const hedged = snap.hedged && snap.hedged.length
    ? `<section><h2>Hedged Wheel — put credit spreads</h2>${suggestionTable(snap.hedged, "hedged", armed)}</section>`
    : "";
  $app.innerHTML =
    controlBar(snap) +
    statusBar(snap) +
    (snap.status ? `<div class="statusline">${esc(snap.status)}</div>` : "") +
    armedBanner +
    note +
    `<section><h2>0DTE structures</h2><div class="grid">${grid}</div></section>` +
    `<section><h2>Suggestions — cash-secured puts</h2>${suggestionTable(snap.suggestions, "classic", armed)}</section>` +
    hedged +
    `<section><h2>Positions</h2>${positionsTable(snap.positions)}</section>` +
    `<section><h2>Journal</h2>${journalTable(snap.journal)}</section>`;
  // Preserve a half-typed live-confirm phrase across re-renders.
  const lp = document.getElementById("live-phrase");
  if (lp) lp.value = liveVal;
}

// ---- command wiring (one delegated listener; survives re-renders) ----
async function call(cmd, args) {
  try {
    render(await invoke(cmd, args));
  } catch (e) {
    $app.querySelector(".statusline")?.replaceChildren(document.createTextNode(String(e)));
  }
}
document.addEventListener("click", (e) => {
  const b = e.target.closest("button[data-act]");
  if (!b || b.disabled) return;
  const act = b.dataset.act;
  if (act === "preview") call("preview", { list: b.dataset.list, index: +b.dataset.index });
  else if (act === "execute") call("execute", { list: b.dataset.list, index: +b.dataset.index });
  else if (act === "arm") call("set_armed", { on: b.dataset.on === "1" });
  else if (act === "refresh") call("refresh", {});
  else if (act === "confirm-live") {
    const p = (document.getElementById("live-phrase") || {}).value || "";
    call("confirm_live", { phrase: p });
  }
});

window.__TAURI__.event.listen("snapshot", (ev) => render(ev.payload));
(async () => {
  try { render(await invoke("get_snapshot")); } catch (e) { $app.innerHTML = `<div class="empty">starting…</div>`; }
})();
