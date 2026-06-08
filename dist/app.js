// BetterWheel desktop — renders the cached Snapshot the Rust backend emits.
// Pure vanilla JS over window.__TAURI__ (withGlobalTauri); no build step.

const { invoke } = window.__TAURI__.core;

const $app = document.getElementById("app");

function money(v) {
  if (v == null) return "—";
  return Math.abs(v) >= 1000 ? "$" + Math.round(v) : "$" + v.toFixed(2);
}
function price(v) {
  return v == null ? "—" : v.toFixed(2);
}
function esc(s) {
  return String(s).replace(/[&<>"]/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" }[c]));
}

// ---- payoff chart: a self-contained inline SVG (no chart lib, CSP-safe) -------
function svgPayoff(xs, ys) {
  if (!xs || xs.length < 2) return "";
  const W = 360, H = 168, padL = 6, padR = 6, padT = 8, padB = 14;
  const xmin = xs[0], xmax = xs[xs.length - 1];
  let ymin = Math.min.apply(null, ys.concat(0));
  let ymax = Math.max.apply(null, ys.concat(0));
  if (ymin === ymax) { ymin -= 1; ymax += 1; }
  const sx = (x) => padL + ((x - xmin) / (xmax - xmin)) * (W - padL - padR);
  const sy = (y) => padT + (1 - (y - ymin) / (ymax - ymin)) * (H - padT - padB);
  const zeroY = sy(0).toFixed(1);
  let d = "";
  for (let i = 0; i < xs.length; i++) {
    d += (i ? "L" : "M") + sx(xs[i]).toFixed(1) + " " + sy(ys[i]).toFixed(1) + " ";
  }
  // Split the area fill at the zero line: green above, red below.
  const baseY = sy(0).toFixed(1);
  let area = "M" + sx(xs[0]).toFixed(1) + " " + baseY + " ";
  for (let i = 0; i < xs.length; i++) area += "L" + sx(xs[i]).toFixed(1) + " " + sy(ys[i]).toFixed(1) + " ";
  area += "L" + sx(xs[xs.length - 1]).toFixed(1) + " " + baseY + " Z";
  return (
    `<svg viewBox="0 0 ${W} ${H}" class="chart" preserveAspectRatio="none">` +
    `<path d="${area}" class="pf-area"/>` +
    `<line x1="${padL}" y1="${zeroY}" x2="${W - padR}" y2="${zeroY}" class="pf-zero"/>` +
    `<path d="${d}" class="pf-line"/>` +
    `</svg>`
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
    `<div class="status"><span class="${dot}"></span><span class="conn">${conn}</span>` +
    acct +
    `<span class="spacer"></span><span class="updated">updated ${esc(snap.updated)}</span></div>`
  );
}

function zerodteCard(slot) {
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
    `<div class="pf-cap">P&amp;L at expiry vs. underlying</div></div>`
  );
}

function suggestionTable(rows) {
  if (!rows || !rows.length) return `<div class="empty">nothing ranked right now</div>`;
  const body = rows
    .map(
      (s) =>
        `<tr><td>${esc(s.symbol)}</td><td>${esc(s.action)}</td><td>${price(s.strike)}</td>` +
        `<td>${esc(s.expiry)}</td><td class="num">${s.dte}</td><td class="num">${s.quantity}</td>` +
        `<td class="num">${money(s.premium_total)}</td><td class="num">${Math.round(s.annualized_yield * 100)}%</td>` +
        `<td class="num">${s.delta == null ? "—" : s.delta.toFixed(2)}</td></tr>`
    )
    .join("");
  return (
    `<table><thead><tr><th>Symbol</th><th>Action</th><th>Strike</th><th>Exp</th>` +
    `<th class="num">DTE</th><th class="num">Qty</th><th class="num">Credit</th>` +
    `<th class="num">Ann.Yield</th><th class="num">Δ</th></tr></thead><tbody>${body}</tbody></table>`
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
function shortTs(ts) {
  const t = String(ts).split("T")[1];
  return t ? t.slice(0, 5) : ts;
}
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
  const note = snap.note ? `<div class="note">${esc(snap.note)}</div>` : "";
  const grid = (snap.zerodte || []).map(zerodteCard).join("");
  const hedged = snap.hedged && snap.hedged.length
    ? `<section><h2>Hedged Wheel — put credit spreads</h2>${suggestionTable(snap.hedged)}</section>`
    : "";
  $app.innerHTML =
    statusBar(snap) +
    note +
    `<section><h2>0DTE structures</h2><div class="grid">${grid}</div></section>` +
    `<section><h2>Suggestions — cash-secured puts</h2>${suggestionTable(snap.suggestions)}</section>` +
    hedged +
    `<section><h2>Positions</h2>${positionsTable(snap.positions)}</section>` +
    `<section><h2>Journal</h2>${journalTable(snap.journal)}</section>`;
}

async function refresh() {
  try {
    render(await invoke("get_snapshot"));
  } catch (e) {
    $app.innerHTML = `<div class="empty">backend not ready: ${esc(e)}</div>`;
  }
}

// Re-render whenever the backend emits a fresh snapshot; poll as a fallback.
window.__TAURI__.event.listen("snapshot", (ev) => render(ev.payload));
refresh();
setInterval(refresh, 20000);
