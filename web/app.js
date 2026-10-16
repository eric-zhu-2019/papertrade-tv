"use strict";
const $ = (s, r = document) => r.querySelector(s);
const $$ = (s, r = document) => [...r.querySelectorAll(s)];
const store = {
  get(k) { try { return localStorage.getItem(k); } catch { return null; } },
  set(k, v) { try { v == null ? localStorage.removeItem(k) : localStorage.setItem(k, v); } catch {} },
};
const TZ = -new Date().getTimezoneOffset() * 60; // charts show local time
const S = {
  token: store.get("pt_token"),
  lang: store.get("pt_lang") || ((navigator.language || "en").startsWith("zh") ? "zh" : "en"),
  symbol: store.get("pt_symbol"), interval: +store.get("pt_interval") || 15,
  me: null, cfg: null, inst: {}, tickers: {}, feedOk: true, futFeedOk: true, side: "buy", tab: "open", view: "trade",
  fut: null, // futures account: wallet, positions, settings
  book: { bids: [], asks: [] }, tape: [], candles: [], portfolio: null,
  sort: { key: "return_pct", dir: -1 }, board: null, boardSel: null, authMode: "login",
};

// ---------------------------------------------------------------- helpers
const t = (k) => (STRINGS[S.lang] && STRINGS[S.lang][k]) || STRINGS.en[k] || k;
const esc = (s) => String(s ?? "").replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]));
const num = (x, min, max) => Number(x).toLocaleString("en-US", { minimumFractionDigits: min, maximumFractionDigits: max });
const fmtUsd = (x) => (x == null || x === "" ? "–" : num(x, 2, 2));
const fmtQty = (x) => (x == null ? "–" : num(x, 0, 8));
const fmtPrice = (sym, x) => {
  if (x == null || x === "") return "–";
  const p = S.inst[sym] ? S.inst[sym].price_precision : 2;
  return num(x, p, p);
};
const pct = (x, digits = 2) => (x == null ? "–" : (x > 0 ? "+" : "") + Number(x).toFixed(digits) + "%");
const cls = (x) => (x > 0 ? "up" : x < 0 ? "down" : "");
const pad = (n) => String(n).padStart(2, "0");
const fmtTime = (ms) => { const d = new Date(ms); return `${pad(d.getHours())}:${pad(d.getMinutes())}:${pad(d.getSeconds())}`; };
const fmtDT = (ms) => { const d = new Date(ms); return `${pad(d.getMonth() + 1)}-${pad(d.getDate())} ${fmtTime(ms)}`; };
const base = (sym) => sym.split("/")[0].replace(/-PERP$/, "");
const isPerp = (sym) => !!(S.cfg && S.cfg.perps.includes(sym));
const signUsd = (x) => (x == null ? "–" : (+x > 0 ? "+" : "") + fmtUsd(x));
const position = (sym) => (S.fut ? S.fut.positions.find((p) => p.symbol === sym) : null);
const floorTo = (x, dp) => Math.floor(x * 10 ** dp + 1e-9) / 10 ** dp;

async function api(path, opts = {}) {
  const headers = { "Content-Type": "application/json" };
  if (S.token) headers.Authorization = "Bearer " + S.token;
  const r = await fetch("/api" + path, { method: opts.method || "GET", headers, body: opts.body ? JSON.stringify(opts.body) : undefined });
  let j = {};
  try { j = await r.json(); } catch {}
  if (r.status === 401 && S.token && !opts.keep) { signOut(); throw new Error(j.error || "signed out"); }
  if (!r.ok) throw new Error(j.error || "HTTP " + r.status);
  return j;
}

function toast(msg, kind = "") {
  const el = document.createElement("div");
  el.className = "toast " + kind;
  el.textContent = msg;
  $("#toasts").append(el);
  setTimeout(() => el.remove(), 6000);
}

function table(el, cols, rows, opts = {}) {
  if (!rows.length) { el.innerHTML = `<tr><td class="empty">${esc(t("nothing"))}</td></tr>`; return; }
  const head = cols.map((c) => `<th class="${c.sort ? "sortable" : ""} ${opts.sorted === c.sort && c.sort ? "sorted" : ""}" data-sort="${c.sort || ""}">${esc(c.h)}</th>`).join("");
  const body = rows.map((r) => `<tr class="${opts.rowClass ? opts.rowClass(r) : ""}" data-id="${esc(opts.rowId ? opts.rowId(r) : "")}">${cols.map((c) => `<td class="${c.c || ""}">${c.f(r)}</td>`).join("")}</tr>`).join("");
  el.innerHTML = `<thead><tr>${head}</tr></thead><tbody>${body}</tbody>`;
}

function applyLang() {
  document.documentElement.lang = S.lang === "zh" ? "zh-CN" : "en";
  $$("[data-t]").forEach((el) => (el.textContent = t(el.dataset.t)));
  $$("[data-tp]").forEach((el) => (el.placeholder = t(el.dataset.tp)));
  $$(".lang-toggle").forEach((el) => (el.textContent = S.lang === "en" ? "中文" : "English"));
  if (S.me) { updateForm(); renderBanner(); renderTab(); renderView(); }
  if (S.cfg && !S.cfg.registration_open) $("#register-hint").textContent = t("registration_closed");
}

// ---------------------------------------------------------------- auth
function showAuth() {
  $("#app").classList.add("hidden");
  $("#auth").classList.remove("hidden");
  $('#auth-mode [data-mode="login"]').click();
}

function signOut() {
  S.token = null; S.me = null;
  store.set("pt_token", null);
  if (S.ws) S.ws.close();
  showAuth();
}

$("#auth-mode").addEventListener("click", (e) => {
  const m = e.target.dataset.mode;
  if (!m) return;
  S.authMode = m;
  $$("#auth-mode button").forEach((b) => b.classList.toggle("on", b.dataset.mode === m));
  const reg = m === "register";
  $("#invite-row").classList.toggle("hidden", !reg || !S.cfg.registration_open);
  $("#register-hint").classList.toggle("hidden", !reg);
  $("#auth-form [name=password]").autocomplete = reg ? "new-password" : "current-password";
  $("#auth-err").textContent = "";
});

$("#auth-form").addEventListener("submit", async (e) => {
  e.preventDefault();
  const f = Object.fromEntries(new FormData(e.target));
  try {
    const r = await api(S.authMode === "register" ? "/register" : "/login", { method: "POST", body: f, keep: true });
    S.token = r.token;
    store.set("pt_token", r.token);
    e.target.reset();
    await startApp();
  } catch (err) { $("#auth-err").textContent = err.message; }
});

$("#logout").addEventListener("click", async () => { try { await api("/logout", { method: "POST" }); } catch {} signOut(); });
document.addEventListener("click", (e) => {
  if (!e.target.classList.contains("lang-toggle")) return;
  S.lang = S.lang === "en" ? "zh" : "en";
  store.set("pt_lang", S.lang);
  applyLang();
});

// ---------------------------------------------------------------- app start
async function startApp() {
  S.me = await api("/me");
  $("#auth").classList.add("hidden");
  $("#app").classList.remove("hidden");
  $("#hdr-user").textContent = S.me.username;
  $("#nav-admin").classList.toggle("hidden", !S.me.is_admin);
  $("#season-pill").textContent = S.cfg.season.name;
  await loadInstruments();
  if (!S.cfg.symbols.includes(S.symbol) && !isPerp(S.symbol)) S.symbol = S.cfg.symbols[0];
  $$(".perp-only").forEach((el) => el.classList.toggle("hidden", !S.cfg.perps.length));
  $("#intervals").innerHTML = S.cfg.intervals.map((i) => `<button data-iv="${i}" class="${i === S.interval ? "on" : ""}">${i < 60 ? i + "m" : i < 1440 ? i / 60 + "h" : "1D"}</button>`).join("");
  if (!S.chart) initChart();
  applyLang();
  connectWS();
  selectSymbol(S.symbol);
  refresh();
}

async function loadInstruments() {
  const r = await api("/instruments");
  for (const i of r.instruments) S.inst[i.symbol] = i;
  if (r.instruments.length < S.cfg.symbols.length + S.cfg.perps.length) setTimeout(() => S.me && loadInstruments().then(() => { updateForm(); loadCandles(); }), 3000);
}

function connectWS() {
  const ws = new WebSocket((location.protocol === "https:" ? "wss://" : "ws://") + location.host + "/ws");
  S.ws = ws;
  ws.onopen = () => ws.send(JSON.stringify({ op: "auth", token: S.token }));
  ws.onmessage = (ev) => onMessage(JSON.parse(ev.data));
  ws.onclose = () => { if (S.ws === ws && S.token) setTimeout(connectWS, 1500); };
}
setInterval(() => { if (S.ws && S.ws.readyState === 1) S.ws.send('{"op":"ping"}'); }, 25000);

function onMessage(m) {
  switch (m.type) {
    case "auth":
      if (m.ok) S.ws.send(JSON.stringify({ op: "subscribe", symbol: S.symbol }));
      break;
    case "tickers":
      S.feedOk = m.feed_ok; S.futFeedOk = m.fut_feed_ok;
      for (const x of m.tickers) S.tickers[x.symbol] = x;
      renderMarkets(); renderBanner(); renderSymHead();
      break;
    case "book":
      if (m.symbol === S.symbol) { S.book = m; queueBook(); }
      break;
    case "trades":
      if (m.symbol === S.symbol) { S.tape = m.trades; renderTape(); }
      break;
    case "trade":
      if (m.symbol !== S.symbol) break;
      S.tape.unshift(m.trade); S.tape.length = Math.min(S.tape.length, 60);
      renderTape(); chartTrade(m.trade);
      break;
    case "fill": {
      const f = m.fill;
      toast(`${t("filled_toast")}: ${t(f.side)} ${fmtQty(f.qty)} ${base(f.symbol)} @ ${fmtPrice(f.symbol, f.price)}`, "good");
      refreshSoon();
      break;
    }
    case "liquidation":
      toast(`${t("liquidated")}: ${m.symbol} ${t(m.side)} ${fmtQty(m.size)} @ ${fmtPrice(m.symbol, m.mark)}`, "bad");
      refreshSoon();
      break;
    case "futures":
      refreshSoon();
      break;
    case "order": {
      const o = m.order;
      if (o.status === "rejected" || (o.status === "canceled" && o.reason && !/by user/.test(o.reason))) toast(`#${o.id} ${t("s_" + o.status)}: ${o.reason}`, "bad");
      refreshSoon();
      break;
    }
  }
}

// ---------------------------------------------------------------- markets, book, tape
function renderMarkets() {
  const perp = isPerp(S.symbol);
  const seg = S.cfg.perps.length
    ? `<div class="seg mkt-seg"><button data-mk="spot" class="${perp ? "" : "on"}">${esc(t("spot"))}</button><button data-mk="perp" class="${perp ? "on" : ""}">${esc(t("perps"))}</button></div>`
    : "";
  $("#markets").innerHTML = seg + (perp ? S.cfg.perps : S.cfg.symbols).map((s) => {
    const x = S.tickers[s] || {};
    const ch = x.change_pct == null ? null : +x.change_pct;
    return `<div class="mkt ${s === S.symbol ? "on" : ""}" data-sym="${esc(s)}"><b>${esc(s)}</b><span>${fmtPrice(s, x.last)}</span><span></span><span class="${cls(ch)}">${pct(ch)}</span></div>`;
  }).join("");
}
$("#markets").addEventListener("click", (e) => {
  const mk = e.target.dataset.mk;
  if (mk) {
    if ((mk === "perp") === isPerp(S.symbol)) return;
    const last = store.get("pt_sym_" + mk), list = mk === "perp" ? S.cfg.perps : S.cfg.symbols;
    return selectSymbol(list.includes(last) ? last : list[0]);
  }
  const m = e.target.closest(".mkt"); if (m) selectSymbol(m.dataset.sym);
});

function renderSymHead() {
  const x = S.tickers[S.symbol] || {};
  $("#sym-name").textContent = S.symbol;
  $("#sym-last").textContent = fmtPrice(S.symbol, x.last);
  const ch = x.change_pct == null ? null : +x.change_pct;
  $("#sym-chg").textContent = pct(ch); $("#sym-chg").className = cls(ch);
  if (isPerp(S.symbol)) {
    const left = x.next_funding_ms ? Math.max(0, x.next_funding_ms - Date.now()) : null;
    const mm = left == null ? "" : `${pad(Math.floor(left / 60000))}:${pad(Math.floor(left / 1000) % 60)}`;
    const fr = x.funding_rate == null ? null : +x.funding_rate * 100;
    $("#sym-stats").innerHTML = `${esc(t("mark_price"))} <b>${fmtPrice(S.symbol, x.mark)}</b> · ${esc(t("index_price"))} ${fmtPrice(S.symbol, x.index)} · ${esc(t("funding_rate"))} <b class="${cls(fr)}">${fr == null ? "–" : (fr > 0 ? "+" : "") + fr.toFixed(4) + "%"}</b>/1h ${mm ? `· ${esc(t("next_funding"))} ${mm}` : ""} · ${esc(t("max_lev"))} ${esc(x.max_leverage || "–")}x`;
  } else {
    $("#sym-stats").textContent = x.high ? `24h  H ${fmtPrice(S.symbol, x.high)}  L ${fmtPrice(S.symbol, x.low)}  Vol ${num(x.volume, 0, 2)} ${base(S.symbol)}` : "";
  }
  const eq = S.portfolio ? +S.portfolio.equity : null;
  if (eq != null) {
    const r = (eq / +S.cfg.start_balance - 1) * 100;
    $("#hdr-equity").textContent = "$" + fmtUsd(eq);
    $("#hdr-ret").textContent = pct(r); $("#hdr-ret").className = cls(r);
  }
}

function renderBanner() {
  const b = $("#banner");
  const perp = isPerp(S.symbol), ok = perp ? S.futFeedOk : S.feedOk;
  const msg = !ok ? t(perp ? "fut_feed_down" : "feed_down") : S.cfg.simulated_feed ? t("feed_sim") : "";
  b.textContent = msg; b.className = "banner" + (msg ? "" : " hidden") + (!ok ? " bad" : "");
  const all = S.feedOk && (!S.cfg.perps.length || S.futFeedOk);
  $("#feed-dot").className = "dot " + (all ? "ok" : "bad");
}

function selectSymbol(sym) {
  S.symbol = sym; store.set("pt_symbol", sym); store.set("pt_sym_" + (isPerp(sym) ? "perp" : "spot"), sym);
  S.book = { bids: [], asks: [] }; S.tape = [];
  if (S.ws && S.ws.readyState === 1) S.ws.send(JSON.stringify({ op: "subscribe", symbol: sym }));
  renderMarkets(); renderSymHead(); renderBanner(); renderBook(); renderTape(); updateForm(); loadCandles();
}

let bookQueued = false;
function queueBook() { if (!bookQueued) { bookQueued = true; requestAnimationFrame(() => { bookQueued = false; renderBook(); updateEstimate(); }); } }
function renderBook() {
  const N = 12, sym = S.symbol;
  const side = (lv) => { let cum = 0; return lv.slice(0, N).map(([p, q]) => ({ p, q: +q, cum: (cum += +q) })); };
  const asks = side(S.book.asks), bids = side(S.book.bids);
  const max = Math.max(asks.length ? asks[asks.length - 1].cum : 0, bids.length ? bids[bids.length - 1].cum : 0) || 1;
  const row = (l) => `<div class="lvl" data-p="${esc(l.p)}"><i style="width:${(l.cum / max * 100).toFixed(1)}%"></i><span>${fmtPrice(sym, l.p)}</span><span>${num(l.q, 0, 5)}</span><span>${num(l.cum, 0, 4)}</span></div>`;
  $("#asks").innerHTML = asks.reverse().map(row).join("");
  $("#bids").innerHTML = bids.map(row).join("");
  if (S.book.asks.length && S.book.bids.length) {
    const a = +S.book.asks[0][0], b = +S.book.bids[0][0];
    $("#spread").textContent = `${t("spread")} ${fmtPrice(sym, a - b)} (${((a - b) / a * 100).toFixed(3)}%)`;
  } else $("#spread").textContent = "–";
}
for (const id of ["#asks", "#bids"]) $(id).addEventListener("click", (e) => {
  const l = e.target.closest(".lvl"); if (!l) return;
  const f = $("#order-form");
  const type = f.type.value;
  if (type === "market") f.type.value = "limit";
  f.limit_price.value = l.dataset.p;
  updateForm();
});

function renderTape() {
  $("#tape").innerHTML = S.tape.slice(0, 40).map((x) => `<div class="tick"><span class="${x.side === "buy" ? "up" : "down"}">${fmtPrice(S.symbol, x.price)}</span><span>${num(x.qty, 0, 5)}</span><span class="muted">${fmtTime(x.ts)}</span></div>`).join("");
}

// ---------------------------------------------------------------- chart
const chartOpts = () => ({
  layout: { background: { color: "transparent" }, textColor: "#8492a3" },
  grid: { vertLines: { color: "#1f2630" }, horzLines: { color: "#1f2630" } },
  timeScale: { timeVisible: true, secondsVisible: false, borderColor: "#2a323d" },
  rightPriceScale: { borderColor: "#2a323d" }, crosshair: { mode: 0 }, autoSize: true,
});
function initChart() {
  S.chart = LightweightCharts.createChart($("#chart"), chartOpts());
  S.series = S.chart.addCandlestickSeries({ upColor: "#2fbf8f", downColor: "#f0616d", borderVisible: false, wickUpColor: "#2fbf8f", wickDownColor: "#f0616d" });
}
async function loadCandles() {
  const sym = S.symbol, iv = S.interval, inst = S.inst[sym];
  if (!inst || !S.series) return;
  let r;
  try { r = await api(`/candles?symbol=${encodeURIComponent(sym)}&interval=${iv}`); } catch { return; }
  if (sym !== S.symbol || iv !== S.interval) return;
  S.series.applyOptions({ priceFormat: { type: "price", precision: inst.price_precision, minMove: +inst.price_increment } });
  S.candles = r.candles.map((c) => ({ time: c[0] + TZ, open: +c[1], high: +c[2], low: +c[3], close: +c[4] }));
  S.series.setData(S.candles);
  const n = S.candles.length;
  S.chart.timeScale().setVisibleLogicalRange({ from: Math.max(0, n - 140), to: n + 4 });
}
function chartTrade(tr) {
  const span = S.interval * 60, b = Math.floor(tr.ts / 1000 / span) * span + TZ, p = +tr.price;
  let last = S.candles[S.candles.length - 1];
  if (last && last.time === b) { last.high = Math.max(last.high, p); last.low = Math.min(last.low, p); last.close = p; }
  else if (!last || b > last.time) { last = { time: b, open: p, high: p, low: p, close: p }; S.candles.push(last); }
  else return;
  S.series.update(last);
}
$("#intervals").addEventListener("click", (e) => {
  const iv = +e.target.dataset.iv; if (!iv) return;
  S.interval = iv; store.set("pt_interval", iv);
  $$("#intervals button").forEach((b) => b.classList.toggle("on", +b.dataset.iv === iv));
  loadCandles();
});

function lineChart(el, key, points) {
  if (!S[key]) {
    const c = LightweightCharts.createChart(el, chartOpts());
    S[key] = { chart: c, series: c.addAreaSeries({ lineColor: "#e8b339", topColor: "rgba(232,179,57,.25)", bottomColor: "rgba(232,179,57,0)", lineWidth: 2, priceFormat: { type: "price", precision: 0, minMove: 1 } }) };
  }
  const seen = new Set(), data = [];
  for (const [ts, eq] of points) { const time = Math.floor(ts / 1000) + TZ; if (!seen.has(time)) { seen.add(time); data.push({ time, value: +eq }); } }
  S[key].series.setData(data);
  S[key].chart.timeScale().fitContent();
}

// ---------------------------------------------------------------- order form
const form = $("#order-form");
function assetAvail(asset) {
  const a = S.portfolio && S.portfolio.assets.find((x) => x.asset === asset);
  return a ? +a.available : 0;
}
function futSettings(sym) {
  return S.fut && S.fut.settings[sym] ? S.fut.settings[sym] : { margin_mode: "isolated", leverage: "10", max_leverage: "10" };
}
function updateForm() {
  const type = form.type.value, sym = S.symbol, perp = isPerp(sym);
  const hasLimit = type.includes("limit"), isStop = type !== "limit" && type !== "market";
  $("#row-price").classList.toggle("hidden", !hasLimit);
  $("#row-trigger").classList.toggle("hidden", !isStop);
  $("#row-tif").classList.toggle("hidden", type !== "limit");
  $("#qty-unit").textContent = base(sym);
  $("#row-margin").classList.toggle("hidden", !perp);
  $("#row-reduce").classList.toggle("hidden", !perp);
  $("#fut-panel").classList.toggle("hidden", !perp);
  const btn = $("#submit-order");
  const [buyLbl, sellLbl] = perp ? [t("buy_long"), t("sell_short")] : [t("buy"), t("sell")];
  $('#side-seg [data-side="buy"]').textContent = buyLbl;
  $('#side-seg [data-side="sell"]').textContent = sellLbl;
  const reduce = perp && form.reduce_only.checked;
  btn.textContent = perp
    ? `${t(reduce ? (S.side === "buy" ? "close_short" : "close_long") : S.side === "buy" ? "open_long" : "open_short")} ${base(sym)}`
    : `${t(S.side)} ${base(sym)}`;
  btn.className = "primary " + S.side;
  $$("#side-seg button").forEach((b) => b.classList.toggle("on", b.dataset.side === S.side));
  if (perp) {
    const st = futSettings(sym);
    if (document.activeElement !== form.margin_mode) form.margin_mode.value = st.margin_mode;
    const locked = !!position(sym) || (S.orders || []).some((o) => o.symbol === sym);
    form.margin_mode.disabled = locked;
    form.margin_mode.title = locked ? t("mode_locked") : "";
    if (document.activeElement !== form.leverage) form.leverage.value = st.leverage;
    form.leverage.max = st.max_leverage;
    $("#max-lev").textContent = `≤ ${st.max_leverage}x`;
    $("#avail").textContent = `${t("fut_avail")}: ${S.fut ? fmtUsd(S.fut.available) : "–"} ${S.cfg.quote}`;
    renderFutPanel();
  } else {
    const asset = S.side === "buy" ? S.cfg.quote : base(sym);
    $("#avail").textContent = `${t("avail_lbl")}: ${num(assetAvail(asset), 0, 8)} ${asset}`;
  }
  form.note.required = S.cfg.require_note;
  updateEstimate();
}
function walkBook(qty) { // average price for a market order of this size on the visible book
  const lv = S.side === "buy" ? S.book.asks : S.book.bids;
  let left = qty, cost = 0;
  for (const [p, q] of lv) { const f = Math.min(left, +q); cost += f * +p; left -= f; if (left <= 1e-12) break; }
  return left > 1e-12 ? null : cost / qty;
}
function refPrice() {
  const type = form.type.value;
  if (type.includes("limit")) return +form.limit_price.value || null;
  if (type !== "market") return +form.trigger_price.value || null;
  const lv = S.side === "buy" ? S.book.asks : S.book.bids;
  return lv.length ? +lv[0][0] : null;
}
function updateEstimate() {
  const qty = +form.qty.value, type = form.type.value, el = $("#est");
  if (!(qty > 0)) { el.textContent = ""; return; }
  const px = type === "market" ? walkBook(qty) : refPrice();
  if (!px) { el.textContent = type === "market" ? "–" : ""; return; }
  const cost = qty * px;
  const maker = type === "limit" && form.post_only.checked;
  const slip = type === "market" ? ` · ${t("est_slip")} ${fmtPrice(S.symbol, px)}` : "";
  if (isPerp(S.symbol)) {
    const fee = cost * +(maker ? S.cfg.fut_maker_fee : S.cfg.fut_taker_fee);
    const margin = form.reduce_only.checked ? 0 : cost / Math.max(1, +form.leverage.value || 1);
    el.textContent = `${t("value")} $${fmtUsd(cost)} · ${t("est_margin")} ≈ $${fmtUsd(margin)} · ${t("est_fee")} ≤ $${fmtUsd(fee)}` + slip;
    return;
  }
  const fee = cost * +(maker ? S.cfg.maker_fee : S.cfg.taker_fee);
  el.textContent = `${t("est_cost")} $${fmtUsd(cost)} · ${t("est_fee")} ≤ $${fmtUsd(fee)}` + slip;
}
form.addEventListener("input", updateForm);
$("#side-seg").addEventListener("click", (e) => { if (e.target.dataset.side) { S.side = e.target.dataset.side; updateForm(); } });
$("#pcts").addEventListener("click", (e) => {
  const p = +e.target.dataset.p; if (!p) return;
  const inst = S.inst[S.symbol]; if (!inst) return;
  const dp = Math.min(inst.qty_precision, 6);
  let q;
  if (isPerp(S.symbol)) {
    const pos = position(S.symbol);
    if (form.reduce_only.checked) q = pos ? +pos.size * p : 0;
    else {
      let px = refPrice(); if (!px || !S.fut) return;
      if (form.type.value === "market") px *= 1.003;
      const lev = Math.max(1, +form.leverage.value || 1);
      q = (+S.fut.available * p) / (px * (1 / lev + +S.cfg.fut_taker_fee)) * 0.999;
    }
    form.qty.value = String(floorTo(q, dp));
    return updateEstimate();
  }
  if (S.side === "sell") q = assetAvail(base(S.symbol)) * p;
  else {
    let px = refPrice(); if (!px) return;
    if (form.type.value === "market") px *= 1.003; // room for slippage
    q = assetAvail(S.cfg.quote) * p / (px * (1 + Math.max(+S.cfg.taker_fee, +S.cfg.maker_fee)) * 1.0001);
  }
  // selling 100% must sell the exact balance, dust included
  form.qty.value = S.side === "sell" && p === 1 ? String(assetAvail(base(S.symbol))) : String(floorTo(q, dp));
  updateEstimate();
});
form.addEventListener("submit", async (e) => {
  e.preventDefault();
  const f = Object.fromEntries(new FormData(form));
  const type = f.type, body = { symbol: S.symbol, side: S.side, type, qty: f.qty.trim(), note: f.note };
  if (type.includes("limit")) body.limit_price = f.limit_price.trim();
  if (type !== "limit" && type !== "market") body.trigger_price = f.trigger_price.trim();
  if (type === "limit") { body.time_in_force = f.time_in_force; body.post_only = !!f.post_only; }
  if (isPerp(S.symbol)) body.reduce_only = !!f.reduce_only;
  const err = $("#order-err"); err.textContent = "";
  try {
    const r = await api("/orders", { method: "POST", body });
    toast(`${t("order_sent")} #${r.order.id}`);
    form.qty.value = ""; form.note.value = "";
    updateEstimate(); refreshSoon();
  } catch (ex) { err.textContent = ex.message; }
});

// ---------------------------------------------------------------- futures account
/// Liquidation price of an isolated position holding margin `m` (the server's formula, for the
/// add/remove-margin preview; leverage changes are previewed by the server itself).
function isoLiq(p, m) {
  const q = +p.size, e = +p.entry, mm = +p.maint_rate;
  const l = p.side === "long" ? (q * e - m) / (q * (1 - mm)) : (q * e + m) / (q * (1 + mm));
  return l > 0 ? l : null;
}
async function saveFutSettings() {
  clearTimeout(levTimer);
  const err = $("#order-err"); err.textContent = "";
  const sym = S.symbol, mode = form.margin_mode.value, lev = Math.round(+form.leverage.value);
  const body = { symbol: sym, margin_mode: mode, leverage: lev };
  const send = async () => {
    S.fut = await api("/futures/settings", { method: "POST", body });
    toast(`${sym}: ${t(mode)} ${lev}x`);
    refreshSoon();
  };
  form.margin_mode.blur(); form.leverage.blur();
  const p = position(sym), old = +futSettings(sym).leverage;
  if (p && lev !== old && lev >= 1) {
    // the position changes with the leverage: show what the server will do before doing it
    let plan;
    try { plan = await api("/futures/settings", { method: "POST", body: { ...body, dry_run: true } }); }
    catch (ex) { err.textContent = ex.message; updateForm(); return; }
    const iso = p.margin_mode === "isolated", diff = +plan.margin_after - +plan.margin_before;
    const moved = iso && Math.abs(diff) >= 0.005 ? ` (${esc(t(diff > 0 ? "lev_refill" : "lev_release"))} $${fmtUsd(Math.abs(diff))})` : "";
    const locks = Math.abs(+plan.orders_lock_delta) >= 0.005 ? `<br>${esc(t("orders_lock"))} <b>${signUsd(plan.orders_lock_delta)}</b>` : "";
    const html = `<p class="muted small">${esc(t(iso ? "lev_iso_hint" : "lev_cross_hint"))}</p>` +
      `<p class="small">${esc(t(iso ? "cur_margin" : "margin_used"))} <b>$${fmtUsd(plan.margin_before)}</b> → <b>$${fmtUsd(plan.margin_after)}</b>${moved}<br>` +
      `${esc(t("liq_price"))} <b>${plan.liq_before ? fmtPrice(sym, plan.liq_before) : "–"}</b> → <b class="down">${plan.liq_after ? fmtPrice(sym, plan.liq_after) : "–"}</b>${locks}</p>`;
    openModal({ title: `${t("change_lev")} · ${sym} · ${old}x → ${lev}x`, html, ok: send, onCancel: updateForm });
    return;
  }
  try { await send(); } catch (ex) { err.textContent = ex.message; }
  updateForm();
}
// spinner clicks fire "change" each time: wait until the value settles before applying it
let levTimer = null;
function queueFutSettings() { clearTimeout(levTimer); levTimer = setTimeout(saveFutSettings, 600); }

// ---------------------------------------------------------------- modal
const M = { seq: 0, busy: false };
function openModal({ title, html = "", input = false, max = null, preview = null, ok, okLabel, onCancel = null }) {
  Object.assign(M, { max, preview, ok, onCancel, busy: false });
  M.seq++;
  $("#modal-ok").disabled = false;
  $("#modal-title").textContent = title;
  $("#modal-body").innerHTML = html;
  $("#modal-input").classList.toggle("hidden", !input);
  $("#modal-amount").value = "";
  $("#modal-amount").required = input;
  $("#modal-max").classList.toggle("hidden", max == null);
  $("#modal-ok").textContent = okLabel || t("confirm");
  $("#modal-err").textContent = "";
  $("#modal-preview").innerHTML = preview ? preview("") : "";
  $("#modal").classList.remove("hidden");
  if (input) $("#modal-amount").focus();
}
function closeModal(cancelled) {
  if ($("#modal").classList.contains("hidden")) return;
  if (cancelled && M.busy) return; // the request is already on its way; wait for its answer
  $("#modal").classList.add("hidden");
  if (cancelled && M.onCancel) M.onCancel();
  M.ok = null;
}
$("#modal-amount").addEventListener("input", () => { if (M.preview) $("#modal-preview").innerHTML = M.preview($("#modal-amount").value.trim()); });
$("#modal-max").addEventListener("click", () => { $("#modal-amount").value = M.max; $("#modal-amount").dispatchEvent(new Event("input")); });
$("#modal-cancel").addEventListener("click", () => closeModal(true));
// a click that started inside the card (e.g. selecting text) and ended on the backdrop is not a dismissal
let downOnBackdrop = false;
$("#modal").addEventListener("mousedown", (e) => { downOnBackdrop = e.target.id === "modal"; });
$("#modal").addEventListener("click", (e) => { if (e.target.id === "modal" && downOnBackdrop) closeModal(true); });
document.addEventListener("keydown", (e) => { if (e.key === "Escape") closeModal(true); });
$("#modal-form").addEventListener("submit", async (e) => {
  e.preventDefault();
  if (M.busy || !M.ok) return;
  const seq = M.seq, btn = $("#modal-ok");
  btn.disabled = true; M.busy = true;
  try {
    await M.ok($("#modal-amount").value.trim());
    if (seq === M.seq) { M.busy = false; closeModal(false); }
  } catch (ex) {
    if (seq === M.seq) $("#modal-err").textContent = ex.message;
  } finally {
    if (seq === M.seq) { M.busy = false; btn.disabled = false; }
  }
});

/// Add (+) or remove (-) margin on an isolated position.
function marginDialog(sym, add) {
  const p = position(sym); if (!p || !S.fut) return;
  const limit = floorTo(Math.max(0, add ? +S.fut.available : +p.margin_removable), 2);
  const max = String(limit);
  openModal({
    title: `${t(add ? "add_margin" : "remove_margin")} · ${sym} ${t(p.side)} ${fmtQty(p.size)}`,
    html: `<div class="fut-stats"><div><span>${esc(t("cur_margin"))}</span><b>$${fmtUsd(p.margin)}</b></div>` +
      `<div><span>${esc(t("liq_price"))}</span><b class="down">${p.liq_price ? fmtPrice(sym, p.liq_price) : "–"}</b></div>` +
      `<div><span>${esc(t(add ? "can_add" : "can_remove"))}</span><b>$${fmtUsd(limit)}</b></div></div>`,
    input: true, max,
    preview: (v) => {
      const a = +v; if (!(a > 0)) return "";
      const m = +p.margin + (add ? a : -a), liq = isoLiq(p, m);
      return `${esc(t("new_margin"))} <b>$${fmtUsd(m)}</b> · ${esc(t("new_liq"))} <b class="down">${liq ? fmtPrice(sym, liq) : "–"}</b>`;
    },
    okLabel: t(add ? "add_margin" : "remove_margin"),
    ok: async (v) => {
      const a = +v;
      if (!(a > 0)) throw new Error(t("bad_amount"));
      S.fut = await api("/futures/margin", { method: "POST", body: { symbol: sym, amount: String(add ? a : -a) } });
      toast(`${sym}: ${t(add ? "add_margin" : "remove_margin")} $${fmtUsd(v)}`, "good");
      refreshSoon();
    },
  });
}
form.margin_mode.addEventListener("change", saveFutSettings);
form.leverage.addEventListener("change", queueFutSettings);

function renderFutPanel() {
  const a = S.fut; if (!a) return;
  const u = +a.unrealized_pnl;
  $("#fut-stats").innerHTML = [
    [t("fut_equity"), "$" + fmtUsd(a.equity)], [t("fut_avail"), "$" + fmtUsd(a.available)],
    [t("margin_used"), "$" + fmtUsd(a.margin_used)], [t("pnl"), `<span class="${cls(u)}">${signUsd(a.unrealized_pnl)}</span>`],
  ].map(([k, v]) => `<div><span>${esc(k)}</span><b>${v}</b></div>`).join("");
  // first visit with an empty futures wallet: show how to fund it, once
  if (!S.futHinted && +a.wallet === 0 && !a.positions.length) { S.futHinted = true; $("#fut-transfer").open = true; }
}
$("#transfer-max").addEventListener("click", () => {
  const f = $("#transfer-form");
  const max = f.to.value === "futures" ? assetAvail(S.cfg.quote) : S.fut ? +S.fut.available : 0;
  f.amount.value = String(floorTo(max, 2));
});
$("#transfer-form").addEventListener("submit", async (e) => {
  e.preventDefault();
  const f = e.target, err = $("#transfer-err"); err.textContent = "";
  try {
    S.fut = await api("/futures/transfer", { method: "POST", body: { amount: f.amount.value.trim(), to: f.to.value } });
    toast(`${t("transfer")}: $${fmtUsd(f.amount.value)} ${t(f.to.value === "futures" ? "to_futures" : "to_spot")}`, "good");
    f.amount.value = "";
    refresh();
  } catch (ex) { err.textContent = ex.message; }
});

// ---------------------------------------------------------------- tabs
const sideCell = (o) => `<span class="${o.side === "buy" ? "up" : "down"}">${esc(t(o.side))}</span>`;
const typeName = (ty) => ({ limit: t("t_limit"), market: t("t_market"), "stop-loss": t("t_stop"), "take-profit": t("t_tp"), "stop-loss-limit": t("t_stop_limit"), "take-profit-limit": t("t_tp_limit") }[ty] || ty);
const TABS = {
  open: { load: () => api("/orders").then((r) => r.orders), cols: () => [
    { h: t("time"), f: (o) => fmtDT(o.created_ms) }, { h: t("market"), f: (o) => esc(o.symbol) }, { h: t("side"), f: sideCell },
    { h: t("type"), f: (o) => esc(typeName(o.type)) + (o.post_only ? " · PO" : "") },
    { h: t("price"), f: (o) => fmtPrice(o.symbol, o.limit_price) }, { h: t("trigger"), f: (o) => fmtPrice(o.symbol, o.trigger_price) },
    { h: t("qty"), f: (o) => fmtQty(o.qty) }, { h: t("filled"), f: (o) => fmtQty(o.filled) },
    { h: t("queue"), f: (o) => (o.status === "open" && o.queue_ahead != null ? fmtQty(o.queue_ahead) : "–") },
    { h: t("status"), f: (o) => esc(t("s_" + o.status)) },
    { h: "", f: (o) => `<button data-cancel="${o.id}">${esc(t("cancel"))}</button>` },
  ] },
  history: { load: () => api("/orders?status=closed&limit=200").then((r) => r.orders), cols: () => [
    { h: t("time"), f: (o) => fmtDT(o.created_ms) }, { h: t("market"), f: (o) => esc(o.symbol) }, { h: t("side"), f: sideCell },
    { h: t("type"), f: (o) => esc(typeName(o.type)) }, { h: t("price"), f: (o) => fmtPrice(o.symbol, o.limit_price || o.trigger_price) },
    { h: t("qty"), f: (o) => fmtQty(o.qty) }, { h: t("filled"), f: (o) => fmtQty(o.filled) },
    { h: t("avg_price"), f: (o) => fmtPrice(o.symbol, o.avg_price) }, { h: t("fee"), f: (o) => num(o.fee, 2, 4) },
    { h: t("status"), f: (o) => esc(t("s_" + o.status)) },
    { h: t("reason"), c: "txt", f: (o) => esc(o.status === "filled" ? "" : o.reason) }, { h: t("note"), c: "txt", f: (o) => esc(o.note) },
  ] },
  fills: { load: () => api("/fills?limit=200").then((r) => r.fills), cols: () => [
    { h: t("time"), f: (x) => fmtDT(x.ts) }, { h: t("market"), f: (x) => esc(x.symbol) }, { h: t("side"), f: sideCell },
    { h: t("price"), f: (x) => fmtPrice(x.symbol, x.price) }, { h: t("qty"), f: (x) => fmtQty(x.qty) },
    { h: t("cost"), f: (x) => fmtUsd(x.cost) }, { h: t("fee"), f: (x) => num(x.fee, 2, 4) },
    { h: t("role"), f: (x) => esc(t(x.liquidity)) }, { h: t("note"), c: "txt", f: (x) => esc(x.note) },
  ] },
  positions: { load: () => api("/futures/account").then((r) => { S.fut = r; return r.positions; }), cols: () => [
    { h: t("market"), c: "name", f: (p) => `<a href="#" data-go="${esc(p.symbol)}">${esc(p.symbol)}</a>` },
    { h: t("side"), f: (p) => `<span class="${p.side === "long" ? "up" : "down"}">${esc(t(p.side))}</span>` },
    { h: t("size"), f: (p) => fmtQty(p.size) }, { h: t("entry_price"), f: (p) => fmtPrice(p.symbol, p.entry) },
    { h: t("mark_price"), f: (p) => fmtPrice(p.symbol, p.mark) },
    { h: t("liq_price"), f: (p) => (p.liq_price ? `<span class="down">${fmtPrice(p.symbol, p.liq_price)}</span>` : "–") },
    { h: t("margin"), f: (p) => `$${fmtUsd(p.margin)} <span class="muted">${esc(t(p.margin_mode))} ${esc(p.leverage)}x</span>` +
      (p.margin_mode === "isolated" ? `<button class="mini" data-addm="${esc(p.symbol)}" title="${esc(t("add_margin"))}">+</button><button class="mini" data-subm="${esc(p.symbol)}" title="${esc(t("remove_margin"))}">−</button>` : "") },
    { h: t("pnl"), f: (p) => `<span class="${cls(+p.unrealized_pnl)}">${signUsd(p.unrealized_pnl)} (${pct(+p.roe_pct)})</span>` },
    { h: t("funding"), f: (p) => `<span class="${cls(+p.funding)}">${signUsd(p.funding)}</span>` },
    { h: "", f: (p) => `<button data-close="${esc(p.symbol)}">${esc(t("close_pos"))}</button>` },
  ] },
  pos_history: { load: () => api("/futures/positions?limit=200").then((r) => r.positions), cols: () => [
    { h: t("time"), f: (p) => fmtDT(p.closed_ms) }, { h: t("market"), f: (p) => esc(p.symbol) },
    { h: t("side"), f: (p) => `<span class="${p.side === "long" ? "up" : "down"}">${esc(t(p.side))}</span>` },
    { h: t("size"), f: (p) => fmtQty(p.size) }, { h: t("entry_price"), f: (p) => fmtPrice(p.symbol, p.entry) },
    { h: t("exit_price"), f: (p) => fmtPrice(p.symbol, p.exit) },
    { h: t("net_pnl"), f: (p) => `<b class="${cls(+p.net)}">${signUsd(p.net)}</b>` }, { h: t("realized"), f: (p) => signUsd(p.realized) },
    { h: t("fee"), f: (p) => fmtUsd(p.fees) }, { h: t("funding"), f: (p) => signUsd(p.funding) },
    { h: t("margin"), f: (p) => `${esc(t(p.margin_mode))} ${esc(p.leverage)}x` },
    { h: t("reason"), c: "txt", f: (p) => `<span class="${p.reason === "liquidated" ? "down" : ""}">${esc(t("r_" + p.reason))}</span>` },
  ] },
  funding: { load: () => api("/futures/funding?limit=200").then((r) => r.funding), cols: () => [
    { h: t("time"), f: (x) => fmtDT(x.ts) }, { h: t("market"), f: (x) => esc(x.symbol) },
    { h: t("rate"), f: (x) => (+x.rate * 100).toFixed(4) + "%" }, { h: t("mark_price"), f: (x) => fmtPrice(x.symbol, x.mark) },
    { h: t("amount_col"), f: (x) => `<span class="${cls(+x.amount)}">${signUsd(x.amount)}</span>` },
  ] },
  balances: { load: () => api("/portfolio").then((r) => r.assets), cols: () => [
    { h: t("asset"), f: (a) => `<b>${esc(a.asset)}</b>` }, { h: t("total"), f: (a) => fmtQty(a.total) },
    { h: t("available"), f: (a) => fmtQty(a.available) }, { h: t("in_orders"), f: (a) => fmtQty(a.locked) },
    { h: t("avg_cost"), f: (a) => (a.avg_cost ? num(a.avg_cost, 2, 6) : "–") }, { h: t("price"), f: (a) => (a.asset === S.cfg.quote ? "–" : num(a.price, 2, 6)) },
    { h: t("value"), f: (a) => "$" + fmtUsd(a.value) },
    { h: t("pnl"), f: (a) => (a.unrealized_pnl == null ? "–" : `<span class="${cls(+a.unrealized_pnl)}">${(+a.unrealized_pnl > 0 ? "+" : "") + fmtUsd(a.unrealized_pnl)}</span>`) },
  ] },
};
let tabRows = [];
async function loadTab() {
  const tab = S.tab;
  try { const rows = await TABS[tab].load(); if (tab === S.tab) { tabRows = rows; renderTab(); } } catch {}
  if (tab === "open") S.orders = tabRows;
  else api("/orders").then((r) => { S.orders = r.orders; $("#open-count").textContent = r.orders.length || ""; }).catch(() => {});
  $("#pos-count").textContent = S.fut && S.fut.positions.length ? S.fut.positions.length : "";
}
function renderTab() {
  table($("#tab-table"), TABS[S.tab].cols(), tabRows);
  if (S.tab === "open") $("#open-count").textContent = tabRows.length || "";
  $("#cancel-all").classList.toggle("hidden", S.tab !== "open" || !tabRows.length);
}
$("#tabs").addEventListener("click", (e) => {
  const b = e.target.closest("[data-tab]"); if (!b) return;
  S.tab = b.dataset.tab; tabRows = [];
  $$("#tabs [data-tab]").forEach((x) => x.classList.toggle("on", x === b));
  renderTab(); loadTab();
});
let closeArmed = null;
$("#tab-table").addEventListener("click", async (e) => {
  const go = e.target.dataset.go;
  if (go) { e.preventDefault(); return selectSymbol(go); }
  if (e.target.dataset.addm) return marginDialog(e.target.dataset.addm, true);
  if (e.target.dataset.subm) return marginDialog(e.target.dataset.subm, false);
  const sym = e.target.dataset.close;
  if (sym) {
    // closing at market is not undoable: ask for a second click
    if (closeArmed !== sym) { closeArmed = sym; e.target.textContent = t("confirm_close"); setTimeout(() => { if (closeArmed === sym) { closeArmed = null; renderTab(); } }, 3000); return; }
    closeArmed = null;
    try { const r = await api("/futures/close", { method: "POST", body: { symbol: sym } }); toast(`${t("order_sent")} #${r.order.id}`); } catch (ex) { toast(ex.message, "bad"); }
    return refreshSoon();
  }
  const id = e.target.dataset.cancel; if (!id) return;
  try { await api("/orders/" + id, { method: "DELETE" }); } catch (ex) { toast(ex.message, "bad"); }
  refreshSoon();
});
$("#cancel-all").addEventListener("click", async () => { try { await api("/orders", { method: "DELETE" }); } catch {} refreshSoon(); });

async function refresh() {
  if (!S.me) return;
  try {
    const [p, f] = await Promise.all([api("/portfolio"), S.cfg.perps.length ? api("/futures/account") : null]);
    S.portfolio = p; if (f) S.fut = f;
    renderSymHead(); updateForm();
  } catch {}
  if (S.view === "trade") loadTab();
}
let refreshTimer = null;
function refreshSoon() { clearTimeout(refreshTimer); refreshTimer = setTimeout(refresh, 200); }
setInterval(() => { if (S.me && !document.hidden) { refresh(); if (S.view === "board") loadBoard(); } }, 5000);

// ---------------------------------------------------------------- views
$("#nav").addEventListener("click", (e) => {
  const v = e.target.dataset.view; if (!v) return;
  S.view = v;
  $$("#nav button").forEach((b) => b.classList.toggle("on", b.dataset.view === v));
  $$(".view").forEach((m) => m.classList.toggle("hidden", m.id !== "view-" + v));
  renderView(true);
});
function renderView(load) {
  if (S.view === "board") load || !S.board ? loadBoard() : renderBoard();
  if (S.view === "account") loadAccount();
  if (S.view === "admin") loadAdmin();
  if (S.view === "trade" && load) loadTab();
}

// leaderboard
async function loadBoard() {
  try { S.board = await api("/leaderboard"); } catch { return; }
  renderBoard();
  if (S.boardSel == null) showCurve(S.me.id, S.me.username);
}
function renderBoard() {
  const b = S.board; if (!b) return;
  $("#board-season").textContent = b.season.name;
  const { key, dir } = S.sort;
  const rows = [...b.rows].sort((x, y) => ((x[key] ?? -Infinity) - (y[key] ?? -Infinity)) * dir);
  const f2 = (v) => (v == null ? "–" : Number(v).toFixed(2));
  const dd = (v) => (v >= 0.005 ? "-" : "") + f2(v) + "%";
  table($("#board-table"), [
    { h: t("rank"), f: (r) => r.rank }, { h: t("trader"), c: "name", f: (r) => esc(r.username) },
    { h: t("equity"), sort: "return_pct", f: (r) => "$" + fmtUsd(r.equity) },
    { h: t("ret"), sort: "return_pct", f: (r) => `<span class="${cls(r.return_pct)}">${pct(r.return_pct)}</span>` },
    { h: t("max_dd"), sort: "max_dd_pct", f: (r) => dd(r.max_dd_pct) },
    { h: t("risk_adj"), sort: "risk_adj", f: (r) => f2(r.risk_adj) },
    { h: t("sharpe"), sort: "sharpe", f: (r) => f2(r.sharpe) }, { h: t("trades"), sort: "trades", f: (r) => r.trades },
  ], rows, { sorted: key, rowId: (r) => r.user_id, rowClass: (r) => "clickable" + (r.user_id === S.me.id ? " me" : "") + (r.user_id === S.boardSel ? " sel" : "") });
  const past = b.past || [];
  $("#past-panel").classList.toggle("hidden", !past.length);
  if (past.length) table($("#past-table"), [
    { h: t("season"), c: "name", f: (r) => esc(r.season) }, { h: t("rank"), f: (r) => r.rank }, { h: t("trader"), c: "name", f: (r) => esc(r.username) },
    { h: t("equity"), f: (r) => "$" + fmtUsd(r.equity) }, { h: t("ret"), f: (r) => `<span class="${cls(r.return_pct)}">${pct(r.return_pct)}</span>` },
    { h: t("max_dd"), f: (r) => dd(r.max_dd_pct) }, { h: t("trades"), f: (r) => r.trades },
  ], past);
}
$("#board-table").addEventListener("click", (e) => {
  const th = e.target.closest("th");
  if (th && th.dataset.sort) {
    const k = th.dataset.sort;
    S.sort = { key: k, dir: S.sort.key === k ? -S.sort.dir : k === "max_dd_pct" ? 1 : -1 };
    return renderBoard();
  }
  const tr = e.target.closest("tr[data-id]");
  if (tr && tr.dataset.id) { const r = S.board.rows.find((x) => String(x.user_id) === tr.dataset.id); if (r) showCurve(r.user_id, r.username); }
});
async function showCurve(uid, name) {
  S.boardSel = uid;
  renderBoard();
  $("#curve-title").textContent = `${t("equity_curve")} · ${name}`;
  try { const r = await api("/equity?user_id=" + uid); lineChart($("#board-chart"), "boardChart", r.points); } catch {}
}

// account
async function loadAccount() {
  try {
    const [p, keys, eq] = await Promise.all([api("/portfolio"), api("/apikeys"), api("/equity")]);
    const ret = (+p.equity / +p.start_balance - 1) * 100;
    $("#acct-stats").innerHTML = `<div><span>${esc(t("equity"))}</span><b>$${fmtUsd(p.equity)}</b></div><div><span>${esc(t("return_lbl"))}</span><b class="${cls(ret)}">${pct(ret)}</b></div><div><span>${esc(t("start_balance"))}</span><b>$${fmtUsd(p.start_balance)}</b></div><div><span>${esc(t("season"))}</span><b>${esc(S.cfg.season.name)}</b></div>`;
    lineChart($("#acct-chart"), "acctChart", eq.points);
    table($("#keys-table"), [
      { h: "Key", f: (k) => esc(k.prefix) + "…" }, { h: t("note"), c: "name", f: (k) => esc(k.label) }, { h: t("created"), f: (k) => fmtDT(k.created_ms) },
      { h: "", f: (k) => `<button data-revoke="${esc(k.prefix)}">${esc(t("revoke"))}</button>` },
    ], keys.keys);
  } catch {}
  const o = location.origin;
  $("#api-doc").textContent = `# Every request needs:  Authorization: Bearer <your key>
curl -H "Authorization: Bearer $KEY" "${o}/api/book?symbol=BTC/USD&depth=10"
curl -H "Authorization: Bearer $KEY" "${o}/api/portfolio"

# place an order (type: market | limit | stop-loss | take-profit | stop-loss-limit | take-profit-limit)
curl -X POST -H "Authorization: Bearer $KEY" -H "Content-Type: application/json" \\
  -d '{"symbol":"BTC/USD","side":"buy","type":"limit","qty":"0.01","limit_price":"50000","time_in_force":"gtc","post_only":true,"note":"bot test"}' \\
  "${o}/api/orders"

GET    /api/orders                 open orders          GET /api/orders?status=closed   history
GET    /api/orders/{id}            one order            DELETE /api/orders/{id}         cancel
DELETE /api/orders                 cancel all           GET /api/fills                  your fills
GET    /api/tickers | /api/instruments | /api/trades?symbol= | /api/candles?symbol=&interval=1|5|15|60|240|1440

# Perpetual futures (symbols like BTC-PERP; a separate USD wallet; 1 contract = 1 coin)
POST /api/futures/transfer  {"amount":"1000","to":"futures"|"spot"}
POST /api/futures/settings  {"symbol":"BTC-PERP","margin_mode":"isolated"|"cross","leverage":10}
                            (leverage can change with a position open; isolated margin is reset to entry value / leverage)
GET  /api/futures/account   wallet, available margin, positions with liquidation prices
POST /api/orders            same as spot, with "symbol":"BTC-PERP" and optional "reduce_only":true
POST /api/futures/close     {"symbol":"BTC-PERP"}   market-close the whole position
POST /api/futures/margin    {"symbol":"BTC-PERP","amount":"50"}   add isolated margin (negative removes)
GET  /api/futures/positions | /api/futures/funding   closed positions, funding payments

# Live stream: connect to ${o.replace(/^http/, "ws")}/ws and send
#   {"op":"auth","token":"<your key>"}  then  {"op":"subscribe","symbol":"BTC/USD"}
# You receive: book, trade, tickers, plus your own order and fill events.
# Orders are accepted as "pending" and reach the book 50-200 ms later, like a real exchange.`;
}
$("#key-form").addEventListener("submit", async (e) => {
  e.preventDefault();
  try {
    const r = await api("/apikeys", { method: "POST", body: { label: e.target.label.value } });
    const el = $("#new-key"); el.textContent = r.key; el.classList.remove("hidden");
    e.target.reset(); loadAccount();
  } catch (ex) { toast(ex.message, "bad"); }
});
$("#keys-table").addEventListener("click", async (e) => {
  const p = e.target.dataset.revoke; if (!p) return;
  await api("/apikeys/" + encodeURIComponent(p), { method: "DELETE" }).catch(() => {});
  loadAccount();
});
$("#pw-form").addEventListener("submit", async (e) => {
  e.preventDefault();
  const m = $("#pw-msg");
  try {
    const r = await api("/password", { method: "POST", body: Object.fromEntries(new FormData(e.target)), keep: true });
    S.token = r.token; store.set("pt_token", r.token);
    m.textContent = t("saved"); m.className = "ok"; e.target.reset();
  } catch (ex) { m.textContent = ex.message; m.className = "err"; }
});

// admin
async function loadAdmin() {
  if (!S.me.is_admin) return;
  try {
    const [r, inv] = await Promise.all([api("/admin/users"), api("/admin/invites")]);
    $("#adm-season").textContent = S.cfg.season.name;
    $("#adm-invite").textContent = inv.shared_code || t("shared_off");
    S.invites = inv.invites;
    table($("#invites-table"), [
      { h: t("code"), c: "name", f: (i) => `<code>${esc(i.code)}</code>` }, { h: t("note_col"), f: (i) => esc(i.note || "") },
      { h: t("created"), f: (i) => fmtDT(i.created_ms) },
      { h: t("status"), f: (i) => i.used_by == null ? esc(t("unused")) : `${esc(t("used_by"))}: ${esc(i.used_by_name || "#" + i.used_by)} · ${fmtDT(i.used_ms)}` },
      { h: "", f: (i) => i.used_by == null ? `<button data-revoke-invite="${esc(i.code)}">${esc(t("invite_revoke"))}</button>` : "" },
    ], inv.invites);
    table($("#users-table"), [
      { h: "ID", f: (u) => u.id }, { h: t("nickname"), c: "name", f: (u) => esc(u.username) + (u.is_admin ? " ★" : "") },
      { h: t("equity"), f: (u) => "$" + fmtUsd(u.equity) }, { h: t("open_orders"), f: (u) => u.open_orders },
      { h: t("created"), f: (u) => fmtDT(u.created_ms) }, { h: t("status"), f: (u) => esc(u.disabled ? t("disabled") : t("active")) },
      { h: "", f: (u) => `<input type="password" placeholder="${esc(t("new_pw_ph"))}" autocomplete="new-password" aria-label="${esc(t("new_pw_ph"))}"> <button data-pw="${u.id}">${esc(t("reset_pw"))}</button> <button data-dis="${u.id}" data-to="${!u.disabled}">${esc(u.disabled ? t("enable") : t("disable"))}</button>` },
    ], r.users);
  } catch {}
}
$("#users-table").addEventListener("click", async (e) => {
  const d = e.target.dataset, msg = $("#users-msg");
  try {
    if (d.pw) {
      const pw = e.target.closest("td").querySelector("input").value;
      await api(`/admin/users/${d.pw}/password`, { method: "POST", body: { new_password: pw } });
      msg.textContent = t("saved"); msg.className = "ok";
    } else if (d.dis) {
      await api(`/admin/users/${d.dis}/disabled`, { method: "POST", body: { disabled: d.to === "true" } });
      msg.textContent = "";
    } else return;
    loadAdmin();
  } catch (ex) { msg.textContent = ex.message; msg.className = "err"; }
});
// show codes in a <pre> and copy them; the clipboard API only exists on HTTPS or localhost, so on plain
// LAN HTTP fall back to execCommand and finally to leaving the text selected for Ctrl+C
async function showInvites(codes) {
  const out = $("#invite-out"), msg = $("#invites-msg");
  out.textContent = codes.join("\n"); out.classList.remove("hidden");
  const sel = getSelection(); sel.selectAllChildren(out);
  let ok = false;
  try { await navigator.clipboard.writeText(out.textContent); ok = true; } catch { try { ok = document.execCommand("copy"); } catch {} }
  msg.textContent = t(ok ? "copied" : "copy_manually"); msg.className = "ok";
}
$("#invite-form").addEventListener("submit", async (e) => {
  e.preventDefault();
  const f = e.target.elements, msg = $("#invites-msg");
  try {
    const r = await api("/admin/invites", { method: "POST", body: { count: Number(f.count.value), note: f.note.value } });
    f.note.value = "";
    await loadAdmin();
    await showInvites(r.codes);
  } catch (ex) { msg.textContent = ex.message; msg.className = "err"; }
});
$("#copy-invites").addEventListener("click", () => {
  const codes = (S.invites || []).filter((i) => i.used_by == null).map((i) => i.code);
  if (codes.length) showInvites(codes); else { $("#invites-msg").textContent = t("nothing"); $("#invites-msg").className = "err"; }
});
$("#invites-table").addEventListener("click", async (e) => {
  const code = e.target.dataset.revokeInvite; if (!code) return;
  try { await api("/admin/invites/" + encodeURIComponent(code), { method: "DELETE" }); $("#invites-msg").textContent = ""; loadAdmin(); }
  catch (ex) { $("#invites-msg").textContent = ex.message; $("#invites-msg").className = "err"; }
});
let seasonArmed = 0;
$("#season-form").addEventListener("submit", async (e) => {
  e.preventDefault();
  const btn = e.target.querySelector("button"), msg = $("#season-msg");
  if (Date.now() - seasonArmed > 5000) { seasonArmed = Date.now(); btn.textContent = t("confirm_again"); setTimeout(() => (btn.textContent = t("start_season")), 5000); return; }
  seasonArmed = 0;
  try {
    await api("/admin/season", { method: "POST", body: { name: e.target.elements.name.value } });
    S.cfg = await api("/config");
    $("#season-pill").textContent = S.cfg.season.name;
    S.board = null; S.boardSel = null;
    e.target.reset(); msg.textContent = t("saved"); msg.className = "ok";
    loadAdmin(); refresh();
  } catch (ex) { msg.textContent = ex.message; msg.className = "err"; }
  btn.textContent = t("start_season");
});
$("#reconcile").addEventListener("click", async () => {
  const out = $("#reconcile-out"); out.classList.remove("hidden");
  try { out.textContent = JSON.stringify(await api("/admin/reconcile"), null, 2); } catch (ex) { out.textContent = ex.message; }
});

// ---------------------------------------------------------------- boot
(async function boot() {
  try { S.cfg = await api("/config"); } catch { document.body.textContent = "Server unavailable. Reload in a moment."; return; }
  document.title = S.cfg.club_name + " · Paper Exchange";
  $$(".club-name").forEach((el) => (el.textContent = S.cfg.club_name));
  applyLang();
  if (S.token) { try { await startApp(); return; } catch {} }
  showAuth();
})();
