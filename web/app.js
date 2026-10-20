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
  sort: { key: "return_pct", dir: -1 }, board: null, boardSel: null, authMode: "login", qtyUnit: store.get("pt_qty_unit") === "usd" ? "usd" : "coin", qtyExact: null, futAct: "open",
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
/// Option symbols follow Deribit: BTC-25OCT26-80000-C
const isOpt = (sym) => /^[A-Z]+-\d{1,2}[A-Z]{3}\d{2}-[\d.]+-[CP]$/.test(sym || "");
const signUsd = (x) => (x == null ? "–" : (+x > 0 ? "+" : "") + fmtUsd(x));
const position = (sym) => isPerp(sym)
  ? (S.fut ? S.fut.positions.find((p) => p.symbol === sym) : null)
  : (S.margin ? S.margin.positions.find((p) => p.symbol === sym) : null);
/// Spot margin order: the leverage picked in the form (1 = a plain cash order).
const spotLev = () => (isPerp(S.symbol) || isOpt(S.symbol) ? 1 : +form.mlev.value || 1);
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
  if (!rows.length) { el.innerHTML = `<tr><td class="empty">${opts.empty || esc(t("nothing"))}</td></tr>`; return; }
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
  if (!S.cfg.symbols.includes(S.symbol) && !isPerp(S.symbol) && !(isOpt(S.symbol) && (S.cfg.options || []).includes(S.symbol.split("-")[0]))) S.symbol = S.cfg.symbols[0];
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
      noteBuy(m.order); renderOrderLines();

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
    case "option_expiry":
      toast(`${t("opt_expired")}: ${m.symbol} × ${fmtQty(m.qty)} @ ${fmtUsd(m.settlement)} → $${fmtUsd(m.payout)}`, +m.payout > 0 ? "good" : "");
      refreshSoon();
      break;
    case "margin":
      S.margin = m.account; updateForm();
      refreshSoon();
      break;
    case "order": {
      const o = m.order;
      if (o.status === "rejected" || (o.status === "canceled" && o.reason && !/by user/.test(o.reason))) toast(`#${o.id} ${t("s_" + o.status)}: ${o.reason}`, "bad");
      // keep the chart's order lines current without waiting for the next refresh
      const live = ["pending", "open", "untriggered"].includes(o.status);
      noteBuy(o);
      setOrders([...(S.orders || []).filter((x) => x.id !== o.id), ...(live ? [o] : [])]);
      refreshSoon();
      break;
    }
  }
}

// ---------------------------------------------------------------- markets, book, tape
function renderMarkets() {
  const perp = isPerp(S.symbol), opt = isOpt(S.symbol), kind = opt ? "opt" : perp ? "perp" : "spot";
  const kinds = [["spot", "spot"], ...(S.cfg.perps.length ? [["perp", "perps"]] : []), ...((S.cfg.options || []).length ? [["opt", "options"]] : [])];
  const seg = kinds.length > 1
    ? `<div class="seg mkt-seg">${kinds.map(([k, l]) => `<button data-mk="${k}" class="${k === kind ? "on" : ""}">${esc(t(l))}</button>`).join("")}</div>`
    : "";
  if (opt) {
    const u = S.symbol.split("-")[0];
    $("#markets").innerHTML = seg + S.cfg.options.map((x) => {
      const tk = S.tickers[`${x}/${S.cfg.quote}`] || {};
      return `<div class="mkt ${x === u ? "on" : ""}" data-under="${esc(x)}"><b>${esc(x)} ${esc(t("options"))}</b><span>${fmtPrice(`${x}/${S.cfg.quote}`, tk.last)}</span></div>`;
    }).join("");
    return;
  }
  $("#markets").innerHTML = seg + (perp ? S.cfg.perps : S.cfg.symbols).map((s) => {
    const x = S.tickers[s] || {};
    const ch = x.change_pct == null ? null : +x.change_pct;
    return `<div class="mkt ${s === S.symbol ? "on" : ""}" data-sym="${esc(s)}"><b>${esc(s)}</b><span>${fmtPrice(s, x.last)}</span><span></span><span class="${cls(ch)}">${pct(ch)}</span></div>`;
  }).join("");
}
$("#markets").addEventListener("click", (e) => {
  const mk = e.target.dataset.mk;
  if (mk) {
    const cur = isOpt(S.symbol) ? "opt" : isPerp(S.symbol) ? "perp" : "spot";
    if (mk === cur) return;
    const last = store.get("pt_sym_" + mk);
    if (mk === "opt") return openOptions(last && isOpt(last) ? last : S.cfg.options[0]);
    const list = mk === "perp" ? S.cfg.perps : S.cfg.symbols;
    return selectSymbol(list.includes(last) ? last : list[0]);
  }
  const m = e.target.closest(".mkt"); if (!m) return;
  if (m.dataset.under) return openOptions(m.dataset.under);
  selectSymbol(m.dataset.sym);
});

function renderSymHead() {
  if (isOpt(S.symbol)) return renderOptHead();
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
  const opt = isOpt(sym);
  S.symbol = sym; store.set("pt_symbol", sym); store.set("pt_sym_" + (opt ? "opt" : isPerp(sym) ? "perp" : "spot"), sym);
  document.body.classList.toggle("opt-mode", opt);
  $("#opt-chain").classList.toggle("hidden", !opt);
  $("#opt-panel").classList.toggle("hidden", !opt);
  if (opt) {
    optInst(sym);
    // the chain is priced off the underlying's spot book
    const spot = `${sym.split("-")[0]}/${S.cfg.quote}`;
    if (S.ws && S.ws.readyState === 1) S.ws.send(JSON.stringify({ op: "subscribe", symbol: spot }));
    S.optQuote = null;
    renderMarkets(); renderSymHead(); renderBanner(); updateForm(); renderOrderLines(); loadOptions();
    return;
  }
  S.book = { bids: [], asks: [] }; S.tape = [];
  if (S.ws && S.ws.readyState === 1) S.ws.send(JSON.stringify({ op: "subscribe", symbol: sym }));
  S.lastBuy = null;
  renderMarkets(); renderSymHead(); renderBanner(); renderBook(); renderTape(); updateForm(); loadCandles(); renderOrderLines();
  loadLastBuy();
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

// ---------------------------------------------------------------- options
// The server prices options (Black-Scholes on the Kraken spot, Kraken's 30-day volatility) and
// quotes a bid and ask around that; the chain replaces the chart, the contract card the order book.
/// Trading rules of an option contract, in the same shape as a spot instrument.
function optInst(sym) {
  if (S.inst[sym]) return S.inst[sym];
  const step = sym.startsWith("BTC-") ? "0.01" : "0.1";
  return (S.inst[sym] = { symbol: sym, kind: "option", base: sym, quote: S.cfg.quote, price_increment: "0.1", qty_increment: step, qty_min: step, price_precision: 1, qty_precision: step.length - 2 });
}
/// Open the options market on an underlying ("BTC") or a contract.
function openOptions(x) {
  if (isOpt(x)) return selectSymbol(x);
  S.optUnder = x; S.optExpiry = null;
  // pick the at-the-money call of the nearest expiry once the chain is in
  api(`/options/chain?underlying=${encodeURIComponent(x)}`).then((c) => {
    const atm = c.rows.reduce((a, r) => (Math.abs(+r.strike - +c.spot) < Math.abs(+a.strike - +c.spot) ? r : a), c.rows[0]);
    S.optChain = c; selectSymbol(atm.call.symbol);
  }).catch((ex) => toast(ex.message, "bad"));
}
async function loadOptions() {
  const sym = S.symbol; if (!isOpt(sym)) return;
  const [u, code] = sym.split("-");
  try {
    let c = S.optChain;
    const want = S.optExpiry;
    if (!c || c.underlying !== u || (want && c.expiry_ms !== want) || (!want && !c.expiries.some((e) => e.code === code && e.expiry_ms === c.expiry_ms))) {
      const e = want || (c && c.underlying === u ? (c.expiries.find((x) => x.code === code) || {}).expiry_ms : null);
      c = await api(`/options/chain?underlying=${u}` + (e ? `&expiry=${e}` : ""));
    } else c = await api(`/options/chain?underlying=${u}&expiry=${c.expiry_ms}`);
    if (sym !== S.symbol) return;
    S.optChain = c; S.optExpiry = c.expiry_ms;
    const q = await api(`/options/quote?symbol=${encodeURIComponent(sym)}`);
    if (sym !== S.symbol) return;
    S.optQuote = q;
  } catch (ex) {
    // an expired or unknown contract: back to the underlying's nearest expiry
    if (/expired|unknown/.test(ex.message)) return openOptions(u);
  }
  renderChain(); renderOptPanel(); renderOptHead(); updateEstimate(); if (!document.activeElement || document.activeElement === document.body) updateForm();
}
setInterval(() => { if (S.me && S.view === "trade" && isOpt(S.symbol) && !document.hidden) loadOptions(); }, 2000);
const fmtLeft = (ms) => {
  const m = Math.max(0, Math.floor(ms / 60000)), dd = Math.floor(m / 1440), h = Math.floor(m / 60) % 24;
  return dd ? `${dd}d ${h}h` : `${h}h ${pad(m % 60)}m`;
};
function renderChain() {
  const c = S.optChain; if (!c) return;
  $("#opt-exp").innerHTML = c.expiries.map((e) => `<button data-exp="${e.expiry_ms}" class="${e.expiry_ms === c.expiry_ms ? "on" : ""}">${esc(e.code)}</button>`).join("");
  $("#opt-meta").textContent = `${t("underlying")} ${fmtUsd(c.spot)} · ${t("volatility")} ${(c.vol * 100).toFixed(1)}% · ${t("expires_in")} ${fmtLeft(c.expiry_ms - Date.now())}`;
  const spot = +c.spot, step = c.rows.length > 1 ? +c.rows[1].strike - +c.rows[0].strike : 1;
  const cell = (o, k, itm) => `<td class="q${itm ? " itm" : ""}${o.symbol === S.symbol ? " sel" : ""}" data-osym="${esc(o.symbol)}" data-oside="${k === "bid" ? "sell" : "buy"}">${o[k] == null ? "–" : num(o[k], 1, 1)}</td>`;
  const head = `<tr><th>Δ</th><th>${esc(t("bid"))}</th><th>${esc(t("ask"))}</th><th class="k">${esc(t("strike"))}</th><th>${esc(t("bid"))}</th><th>${esc(t("ask"))}</th><th>Δ</th></tr>`;
  const sub = `<tr><th colspan="3" class="up">${esc(t("calls"))}</th><th></th><th colspan="3" class="down">${esc(t("puts"))}</th></tr>`;
  $("#opt-table").innerHTML = sub + head + c.rows.map((r) => {
    const k = +r.strike, citm = k < spot, pitm = k > spot;
    return `<tr class="${Math.abs(k - spot) < step / 2 ? "atm" : ""}"><td class="${citm ? "itm" : ""}">${r.call.delta ?? "–"}</td>${cell(r.call, "bid", citm)}${cell(r.call, "ask", citm)}<td class="k">${num(k, 0, 2)}</td>${cell(r.put, "bid", pitm)}${cell(r.put, "ask", pitm)}<td class="${pitm ? "itm" : ""}">${r.put.delta ?? "–"}</td></tr>`;
  }).join("");
}
$("#opt-exp").addEventListener("click", (e) => {
  const ex = +e.target.dataset.exp; if (!ex || !S.optChain) return;
  S.optExpiry = ex;
  api(`/options/chain?underlying=${S.optChain.underlying}&expiry=${ex}`).then((c) => {
    S.optChain = c;
    // keep the same strike and type on the new expiry when it is listed there
    const [, , k, cp] = S.symbol.split("-");
    const row = c.rows.find((r) => r.strike === k) || c.rows[Math.floor(c.rows.length / 2)];
    selectSymbol(cp === "P" ? row.put.symbol : row.call.symbol);
  }).catch((ex2) => toast(ex2.message, "bad"));
});
$("#opt-table").addEventListener("click", (e) => {
  const td = e.target.closest("td[data-osym]"); if (!td) return;
  S.side = td.dataset.oside;
  if (td.dataset.osym !== S.symbol) selectSymbol(td.dataset.osym); else { updateForm(); renderChain(); }
});
function renderOptHead() {
  const q = S.optQuote, sym = S.symbol;
  $("#sym-name").textContent = sym;
  $("#sym-last").textContent = q && q.theo != null ? num(q.theo, 1, 1) : "–";
  $("#sym-chg").textContent = ""; $("#sym-chg").className = "";
  $("#sym-stats").textContent = q ? `${t("theo_price")} · ${t(q.type)} ${num(q.strike, 0, 2)} · ${t("underlying")} ${fmtUsd(q.spot)}` : "";
}
function renderOptPanel() {
  const q = S.optQuote; if (!q) { $("#opt-stats").innerHTML = ""; return; }
  const k = +q.strike, ask = +q.ask, held = +((S.portfolio && S.portfolio.assets.find((a) => a.asset === q.symbol)) || { total: 0 }).total;
  const be = q.type === "call" ? k + ask : k - ask;
  $("#opt-stats").innerHTML = [
    [t("theo_price"), num(q.theo, 1, 1)], [t("bid") + " / " + t("ask"), `<span class="up">${num(q.bid, 1, 1)}</span> / <span class="down">${num(q.ask, 1, 1)}</span>`],
    [t("underlying"), fmtUsd(q.spot)], [t("strike"), num(k, 0, 2)],
    ["Delta", q.delta == null ? "–" : (+q.delta).toFixed(3)], [t("volatility"), (q.vol * 100).toFixed(1) + "%"],
    [t("expiry"), `${new Date(q.expiry_ms).toLocaleString()}`], [t("expires_in"), fmtLeft(q.expiry_ms - Date.now())],
    [t("breakeven"), fmtUsd(be)], [t("held"), fmtQty(held)],
  ].map(([a, b]) => `<div><span>${esc(a)}</span><b>${b}</b></div>`).join("");
}

// ---------------------------------------------------------------- chart
const chartOpts = () => ({
  layout: { background: { color: "transparent" }, textColor: "#8492a3" },
  grid: { vertLines: { color: "#1f2630" }, horzLines: { color: "#1f2630" } },
  timeScale: { timeVisible: true, secondsVisible: false, borderColor: "#2a323d" },
  rightPriceScale: { borderColor: "#2a323d" }, crosshair: { mode: 0 }, autoSize: true,
});
/// The user's open orders; the chart draws a line for each one on the shown market.
function setOrders(list) {
  S.orders = list;
  renderOrderLines();
}
function renderOrderLines() {
  if (!S.series) return;
  for (const l of S.orderLines || []) S.series.removePriceLine(l);
  S.orderLines = [];
  if (isOpt(S.symbol)) return; // option premiums don't belong on the underlying's chart
  const lb = S.lastBuy && S.lastBuy.symbol === S.symbol ? S.lastBuy : null;
  if (lb) S.orderLines.push(S.series.createPriceLine({
    price: +lb.price, color: "#5b9cf6", lineWidth: 1, lineStyle: 1, axisLabelVisible: true,
    title: `${t("last_buy")} ${fmtQty(lb.qty)} · ${fmtDT(lb.ts)}`,
  }));
  const stopName = { "stop-loss": "t_stop", "stop-loss-limit": "t_stop_limit", "take-profit": "t_tp", "take-profit-limit": "t_tp_limit" };
  for (const o of S.orders || []) {
    if (o.symbol !== S.symbol) continue;
    const color = o.side === "buy" ? "#2fbf8f" : "#f0616d";
    const left = fmtQty(+o.qty - +o.filled);
    const add = (price, dashed, label) => S.orderLines.push(S.series.createPriceLine({
      price: +price, color, lineWidth: 1, lineStyle: dashed ? 2 : 0, axisLabelVisible: true, title: label,
    }));
    if (o.trigger_price && o.status === "untriggered") {
      // a stop waits at its trigger; a stop-limit also shows where it will rest once triggered
      add(o.trigger_price, true, `${t(stopName[o.type] || "trigger")} ${t(o.side)} ${left}`);
      if (o.limit_price) add(o.limit_price, true, `${t("limit_price")} ${t(o.side)} ${left}`);
    } else if (o.limit_price) {
      add(o.limit_price, false, `${t(o.side)} ${left}`);
    }
  }
  // the order picked in the orders table, highlighted
  const sel = S.selOrder, px = selOrderPrice();
  if (sel && sel.symbol === S.symbol && px) {
    S.orderLines.push(S.series.createPriceLine({
      price: px, color: "#e8b339", lineWidth: 2, lineStyle: 0, axisLabelVisible: true,
      title: `#${sel.id} ${t(sel.side)} ${fmtQty(sel.qty)}`,
    }));
  }
}
/// Where the selected order sits on the chart: its fill price once filled, else its limit or trigger.
function selOrderPrice() {
  if (isOpt(S.symbol)) return null;
  const o = S.selOrder;
  return o ? +(o.avg_price || o.limit_price || o.trigger_price || 0) || null : null;
}
/// Pick (or un-pick) an order row; its line stays until the orders table is left.
function selectOrder(o) {
  S.selOrder = o && (!S.selOrder || S.selOrder.id !== o.id) ? o : null;
  if (S.selOrder && S.selOrder.symbol !== S.symbol) selectSymbol(S.selOrder.symbol);
  renderTab(); renderOrderLines();
  if (S.series) S.chart.priceScale("right").applyOptions({ autoScale: true });
  if (!focusSelOrder()) showLatest();
}
function clearSelOrder() {
  if (!S.selOrder) return;
  S.selOrder = null;
  renderOrderLines();
  focusSelOrder(); showLatest();
}
/// Scroll the chart to the selected order's lifetime and mark when it was placed and when it ended.
/// Returns false (and clears the markers) when there is nothing to show.
function focusSelOrder() {
  const o = S.selOrder, c = S.candles || [];
  if (!S.series) return false;
  S.selMarks = [];
  if (!o || o.symbol !== S.symbol || !c.length) { renderMarkers(); return false; }
  const span = S.interval * 60, bar = (ms) => Math.floor(ms / 1000 / span) * span + TZ;
  // index of the candle holding a timestamp (clamped to the loaded range)
  const idx = (ms) => { const b = bar(ms), i = c.findIndex((k) => k.time >= b); return i < 0 ? c.length - 1 : i; };
  const done = !["pending", "open", "untriggered"].includes(o.status);
  const i0 = idx(o.created_ms), i1 = done ? idx(o.updated_ms || o.created_ms) : c.length - 1;
  const color = o.side === "buy" ? "#2fbf8f" : "#f0616d", below = o.side === "buy";
  const marks = [{ time: c[i0].time, position: below ? "belowBar" : "aboveBar", color: "#e8b339", shape: below ? "arrowUp" : "arrowDown", text: `#${o.id}` }];
  if (done && +o.filled > 0 && i1 !== i0) marks.push({ time: c[i1].time, position: below ? "belowBar" : "aboveBar", color, shape: "circle", text: t("s_" + o.status) });
  S.selMarks = marks; renderMarkers();
  // centre the order's lifetime, but don't scroll past the newest candle
  const half = Math.max(70, (i1 - i0) / 2 + 20), to = Math.min((i0 + i1) / 2 + half, c.length + 4);
  S.chart.timeScale().setVisibleLogicalRange({ from: to - 2 * half, to });
  return true;
}
/// The candle (by time) holding a timestamp, or null when it is outside the loaded candles.
function candleAt(ms) {
  const c = S.candles || [], span = S.interval * 60, b = Math.floor(ms / 1000 / span) * span + TZ;
  if (!c.length || b < c[0].time) return null;
  const k = c.find((x) => x.time >= b);
  return k ? k.time : c[c.length - 1].time;
}
/// Chart markers: the selected order's, plus where the latest buy on this market happened.
function renderMarkers() {
  if (!S.series) return;
  const marks = [...(S.selMarks || [])];
  const lb = S.lastBuy && S.lastBuy.symbol === S.symbol ? S.lastBuy : null;
  const when = lb && candleAt(lb.ts);
  if (when) marks.push({ time: when, position: "belowBar", color: "#5b9cf6", shape: "arrowUp", text: `${t("last_buy")} ${fmtQty(lb.qty)}` });
  S.series.setMarkers(marks.sort((a, b) => a.time - b.time));
}
/// A finished buy order as the latest buy: its average price, the quantity filled and when it finished.
const asLastBuy = (o) => ({ symbol: o.symbol, price: o.avg_price, qty: o.filled, ts: o.updated_ms });
/// A buy order that just finished (filled, or cancelled after a partial fill) becomes the latest buy.
function noteBuy(o) {
  if (!o || ["pending", "open", "untriggered"].includes(o.status) || o.side !== "buy" || !(+o.filled > 0) || o.symbol !== S.symbol) return;
  S.lastBuy = asLastBuy(o); renderMarkers();
}
/// The user's most recent buy order (filled at least in part) on the shown market.
async function loadLastBuy() {
  const sym = S.symbol;
  if (isOpt(sym)) return;
  try {
    const r = await api(`/orders?status=closed&limit=200`);
    if (sym !== S.symbol) return;
    const o = r.orders.find((x) => x.symbol === sym && x.side === "buy" && +x.filled > 0);
    S.lastBuy = o ? asLastBuy(o) : null;
  } catch { return; }
  renderOrderLines(); renderMarkers();
}
function showLatest() {
  const n = (S.candles || []).length;
  if (S.series && n) S.chart.timeScale().setVisibleLogicalRange({ from: Math.max(0, n - 140), to: n + 4 });
}

function initChart() {
  S.chart = LightweightCharts.createChart($("#chart"), chartOpts());
  S.series = S.chart.addCandlestickSeries({
    upColor: "#2fbf8f", downColor: "#f0616d", borderVisible: false, wickUpColor: "#2fbf8f", wickDownColor: "#f0616d",
    // stretch the price scale so every open order's line stays in view
    autoscaleInfoProvider: (base) => {
      const r = base();
      if (!r || !r.priceRange) return r;
      const sel = S.selOrder && S.selOrder.symbol === S.symbol ? [{ symbol: S.symbol, limit_price: selOrderPrice() }] : [];
      if (S.lastBuy && S.lastBuy.symbol === S.symbol) sel.push({ symbol: S.symbol, limit_price: S.lastBuy.price });
      for (const o of [...(S.orders || []), ...sel]) {
        if (o.symbol !== S.symbol) continue;
        for (const p of [o.limit_price, o.trigger_price]) {
          if (!p) continue;
          r.priceRange.minValue = Math.min(r.priceRange.minValue, +p);
          r.priceRange.maxValue = Math.max(r.priceRange.maxValue, +p);
        }
      }
      return r;
    },
  });
}
async function loadCandles() {
  const sym = S.symbol, iv = S.interval, inst = S.inst[sym];
  if (!inst || !S.series || isOpt(sym)) return;
  let r;
  try { r = await api(`/candles?symbol=${encodeURIComponent(sym)}&interval=${iv}`); } catch { return; }
  if (sym !== S.symbol || iv !== S.interval) return;
  S.series.applyOptions({ priceFormat: { type: "price", precision: inst.price_precision, minMove: +inst.price_increment } });
  S.candles = r.candles.map((c) => ({ time: c[0] + TZ, open: +c[1], high: +c[2], low: +c[3], close: +c[4] }));
  S.series.setData(S.candles);
  if (!focusSelOrder()) showLatest();
  renderMarkers();
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
  const sym = S.symbol, perp = isPerp(sym), opt = isOpt(sym);
  // options take market and limit orders only
  for (const o of form.type.options) o.hidden = opt && !["market", "limit"].includes(o.value);
  if (opt && !["market", "limit"].includes(form.type.value)) form.type.value = "limit";
  const type = form.type.value;
  const hasLimit = type.includes("limit"), isStop = type !== "limit" && type !== "market";
  $("#row-price").classList.toggle("hidden", !hasLimit);
  $("#row-trigger").classList.toggle("hidden", !isStop);
  $("#row-tif").classList.toggle("hidden", type !== "limit");
  const [coinBtn, usdBtn] = $$("#qty-unit button");
  coinBtn.textContent = base(sym); usdBtn.textContent = S.cfg.quote;
  coinBtn.title = t("qty_coin"); usdBtn.title = t("qty_in_usd");
  coinBtn.classList.toggle("on", !qtyInUsd()); usdBtn.classList.toggle("on", qtyInUsd());
  $("#row-margin").classList.toggle("hidden", !perp || S.futAct === "close");
  $("#fut-panel").classList.toggle("hidden", !perp);
  const btn = $("#submit-order");
  // spot: 1x is a cash order, 2x and up a margin order (Kraken's leverage for the pair)
  const rule = !perp && !opt && S.margin && S.margin.rules[sym];
  const maxLev = rule ? Math.max(+rule.max_long, +rule.max_short) : 1;
  const levs = ["1", ...Array.from({ length: Math.max(0, maxLev - 1) }, (_, i) => String(i + 2))];
  const optsKey = sym + levs.length + S.lang;
  if (form.mlev.dataset.k !== optsKey) {
    const cur = form.mlev.value || store.get("pt_mlev") || "1";
    form.mlev.innerHTML = levs.map((l) => `<option value="${l}">${l === "1" ? esc(t("cash_1x")) : l + esc(t("margin_x"))}</option>`).join("");
    form.mlev.value = levs.includes(cur) ? cur : "1";
    form.mlev.dataset.k = optsKey;
  }
  $("#mlev-hint").textContent = maxLev > 1 ? t("margin_hint") : "";
  const margin = !perp && !opt && +form.mlev.value > 1;
  // perps and margin: an Open / Close tab. Open is Long / Short; Close is Close long / Close short and reduce-only
  const posMode = perp || margin;
  const closing = posMode && S.futAct === "close";
  form.reduce_only.checked = closing;
  $("#row-mlev").classList.toggle("hidden", perp || maxLev < 2 || closing);
  $("#act-seg").classList.toggle("hidden", !posMode);
  $$("#act-seg button").forEach((b) => b.classList.toggle("on", b.dataset.act === (closing ? "close" : "open")));
  $("#side-seg").classList.toggle("closing", closing); // Close long (a sell) goes first
  $("#row-reduce").classList.add("hidden");
  const [buyLbl, sellLbl] = !posMode ? [t("buy"), t("sell")] : closing ? [t("close_short"), t("close_long")] : [t("open_long"), t("open_short")];
  $('#side-seg [data-side="buy"]').textContent = buyLbl;
  $('#side-seg [data-side="sell"]').textContent = sellLbl;
  const reduce = closing;
  btn.textContent = posMode
    ? `${t(reduce ? (S.side === "buy" ? "close_short" : "close_long") : S.side === "buy" ? "open_long" : "open_short")} ${base(sym)}`
    : opt ? `${t(S.side)} ${sym}` : `${t(S.side)} ${base(sym)}`;
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
    const pos = closablePos();
    $("#avail").textContent = closing
      ? `${t("closable")}: ${pos ? fmtQty(pos.size) : 0} ${base(sym)}`
      : `${t("fut_avail")}: ${S.fut ? fmtUsd(S.fut.available) : "–"} ${S.cfg.quote}`;
    renderFutPanel();
  } else if (opt) {
    const asset = S.side === "buy" ? S.cfg.quote : sym;
    $("#avail").textContent = S.side === "buy"
      ? `${t("avail_lbl")}: ${num(assetAvail(asset), 0, 2)} ${asset}`
      : `${t("held")}: ${fmtQty(assetAvail(sym))} ${t("contracts")}`;
  } else if (margin) {
    const pos = closablePos(), m = S.margin, lvl = m && m.margin_level != null ? +m.margin_level : null;
    const level = lvl == null ? "" : ` · ${t("margin_level")} ${lvl.toFixed(0)}%`;
    $("#avail").innerHTML = esc(closing
      ? `${t("closable")}: ${pos ? fmtQty(pos.size) : 0} ${base(sym)}`
      : `${t("margin_avail")}: ${num(assetAvail(S.cfg.quote), 0, 2)} ${S.cfg.quote}`) +
      (level ? `<span class="${lvl < +rule.call_level * 100 ? "down" : ""}">${esc(level)}</span>` : "");
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
  if (isOpt(S.symbol)) {
    if (type === "limit") return +form.limit_price.value || null;
    const q = S.optQuote;
    return q ? +(S.side === "buy" ? q.ask : q.bid) || null : null;
  }
  if (type.includes("limit")) return +form.limit_price.value || null;
  if (type !== "market") return +form.trigger_price.value || null;
  const lv = S.side === "buy" ? S.book.asks : S.book.bids;
  return lv.length ? +lv[0][0] : null;
}
// ---- quantity in coins or in USD: a USD amount is turned into a coin quantity at the expected price
const qtyInUsd = () => S.qtyUnit === "usd" && !isOpt(S.symbol);
const qtyDp = () => Math.min((S.inst[S.symbol] || {}).qty_precision ?? 8, 8);
/// Coins that `usd` buys (or sells) at the expected fill: walking the book for market orders, else the limit / trigger price.
function usdToQty(usd) {
  if (!(usd > 0)) return 0;
  if (form.type.value === "market") {
    const lv = S.side === "buy" ? S.book.asks : S.book.bids;
    let left = usd, q = 0;
    for (const [p, sz] of lv) { const take = Math.min(left, +p * +sz); q += take / +p; left -= take; if (left <= 1e-9) break; }
    if (left > 1e-9) return null; // deeper than the visible book
    return floorTo(q, qtyDp());
  }
  const px = refPrice();
  return px ? floorTo(usd / px, qtyDp()) : null;
}
/// The coin quantity the form will send, as a string (null when it can't be worked out yet).
function orderQty() {
  const v = form.qty.value.trim();
  if (!qtyInUsd()) return v;
  const x = S.qtyExact;
  if (x && x.usd === v && x.symbol === S.symbol && x.side === S.side && x.type === form.type.value) return x.qty; // set by a % button: exact, dust included
  const q = usdToQty(+v);
  return q == null ? null : String(q);
}
$("#qty-unit").addEventListener("click", (e) => {
  const u = e.target.dataset.u; if (!u || u === S.qtyUnit) return;
  e.preventDefault();
  // carry the current amount over into the other unit
  const v = form.qty.value.trim(), coins = orderQty();
  S.qtyUnit = u; store.set("pt_qty_unit", u);
  if (u === "coin") { form.qty.value = +v > 0 ? coins ?? "" : v; S.qtyExact = null; updateForm(); }
  else { updateForm(); if (+v > 0) setPctQty(v); }
});
function updateEstimate() {
  const q = orderQty(), qty = +q, type = form.type.value, el = $("#est");
  const posMode = isPerp(S.symbol) || spotLev() > 1;
  if (posMode && S.futAct === "close" && !closablePos()) { el.textContent = t("no_pos_to_close"); return; }
  const minQ = +((S.inst[S.symbol] || {}).qty_min || 0);
  if (qtyInUsd() && +form.qty.value > 0 && q != null && !(qty > 0 && qty >= minQ)) { el.textContent = `${t("qty_too_small")} (${fmtQty(minQ)} ${base(S.symbol)})`; return; }
  if (!(qty > 0)) { el.textContent = ""; return; }
  if (isOpt(S.symbol)) {
    const px = refPrice(), q = S.optQuote;
    if (!px || !q) { el.textContent = ""; return; }
    const cost = qty * px, fee = Math.min(+q.spot * +(S.optChain ? S.optChain.fee : 0.0003), px * +(S.optChain ? S.optChain.fee_cap : 0.125)) * qty;
    const be = q.type === "call" ? +q.strike + px : +q.strike - px;
    el.textContent = `${t("premium")} $${fmtUsd(cost)} · ${t("est_fee")} ≤ $${fmtUsd(fee)}` + (S.side === "buy" ? ` · ${t("breakeven")} ${fmtUsd(be)}` : "");
    return;
  }
  const px = type === "market" ? walkBook(qty) : refPrice();
  if (!px) { el.textContent = type === "market" ? "–" : ""; return; }
  const cost = qty * px;
  const maker = type === "limit" && form.post_only.checked;
  const slip = (type === "market" ? ` · ${t("est_slip")} ${fmtPrice(S.symbol, px)}` : "") + (qtyInUsd() ? ` · ≈ ${fmtQty(q)} ${base(S.symbol)}` : "");
  if (isPerp(S.symbol)) {
    const fee = cost * +(maker ? S.cfg.fut_maker_fee : S.cfg.fut_taker_fee);
    const margin = form.reduce_only.checked ? "" : ` · ${t("est_margin")} ≈ $${fmtUsd(cost / Math.max(1, +form.leverage.value || 1))}`;
    el.textContent = `${t("value")} $${fmtUsd(cost)}${margin} · ${t("est_fee")} ≤ $${fmtUsd(fee)}` + slip;
    return;
  }
  let fee = cost * +(maker ? S.cfg.maker_fee : S.cfg.taker_fee);
  if (spotLev() > 1) {
    // margin: the opening fee is charged on what opens a position
    const closing = form.reduce_only.checked;
    if (!closing) fee += cost * marginRate();
    const margin = closing ? "" : ` · ${t("est_margin")} ≈ $${fmtUsd(cost / spotLev())}`;
    el.textContent = `${t("value")} $${fmtUsd(cost)}${margin} · ${t("est_fee")} ≤ $${fmtUsd(fee)}` + slip;
    return;
  }
  el.textContent = `${t("est_cost")} $${fmtUsd(cost)} · ${t("est_fee")} ≤ $${fmtUsd(fee)}` + slip;
}
form.addEventListener("input", (e) => {
  if (e.target !== form.qty) S.qtyExact = null;
  if (e.target === form.mlev) store.set("pt_mlev", form.mlev.value);
  updateForm();
});
$("#side-seg").addEventListener("click", (e) => {
  if (!e.target.dataset.side) return;
  S.side = e.target.dataset.side;
  if ((isPerp(S.symbol) || spotLev() > 1) && S.futAct === "close") return fillClose();
  updateForm();
});
/// The position the Close tab would reduce: a long for Close long (a sell), a short for Close short.
function closablePos() {
  const p = position(S.symbol);
  return p && p.side === (S.side === "sell" ? "long" : "short") ? p : null;
}
/// Spot margin opening / rollover fee rate: the borrowed currency's (USD for a long, the coin for a short).
function marginRate() {
  const r = S.margin && S.margin.rules[S.symbol];
  return r ? +(S.side === "buy" ? r.long_rate : r.short_rate) : 0;
}
/// Close tab: pick the side that closes the open position and fill in its whole size.
function fillClose() {
  updateForm();
  const p = closablePos();
  if (p) setPctQty(p.size); else { form.qty.value = ""; S.qtyExact = null; updateEstimate(); }
}
$("#act-seg").addEventListener("click", (e) => {
  const a = e.target.dataset.act; if (!a || a === S.futAct) return;
  S.futAct = a;
  if (a === "close") {
    const p = position(S.symbol);
    if (p) S.side = p.side === "long" ? "sell" : "buy";
    return fillClose();
  }
  form.qty.value = ""; S.qtyExact = null;
  updateForm();
});
$("#pcts").addEventListener("click", (e) => {
  const p = +e.target.dataset.p; if (!p) return;
  const inst = S.inst[S.symbol]; if (!inst) return;
  const dp = Math.min(inst.qty_precision, 6);
  let q;
  if (isPerp(S.symbol)) {
    const pos = position(S.symbol);
    if (form.reduce_only.checked) { const c = closablePos(); q = c ? +c.size * p : 0; if (c && p === 1) return setPctQty(c.size); }
    else {
      let px = refPrice(); if (!px || !S.fut) return;
      if (form.type.value === "market") px *= 1.003;
      const lev = Math.max(1, +form.leverage.value || 1);
      q = (+S.fut.available * p) / (px * (1 / lev + +S.cfg.fut_taker_fee)) * 0.999;
    }
    return setPctQty(String(floorTo(q, dp)));
  }
  if (isOpt(S.symbol)) {
    if (S.side === "sell") return setPctQty(String(floorTo(assetAvail(S.symbol) * p, dp)));
    const px = refPrice(); if (!px) return;
    const cap = +(S.optChain ? S.optChain.fee_cap : 0.125);
    return setPctQty(String(floorTo(assetAvail(S.cfg.quote) * p / (px * (1 + cap)), dp)));
  }
  if (spotLev() > 1) {
    if (form.reduce_only.checked) { const c = closablePos(); if (c && p === 1) return setPctQty(c.size); q = c ? +c.size * p : 0; }
    else {
      let px = refPrice(); if (!px) return;
      if (form.type.value === "market") px *= 1.003;
      const fee = Math.max(+S.cfg.taker_fee, +S.cfg.maker_fee);
      q = assetAvail(S.cfg.quote) * p / (px * (1 / spotLev() + fee + marginRate())) * 0.999;
    }
    return setPctQty(String(floorTo(q, dp)));
  }
  if (S.side === "sell") q = assetAvail(base(S.symbol)) * p;
  else {
    let px = refPrice(); if (!px) return;
    if (form.type.value === "market") px *= 1.003; // room for slippage
    q = assetAvail(S.cfg.quote) * p / (px * (1 + Math.max(+S.cfg.taker_fee, +S.cfg.maker_fee)) * 1.0001);
  }
  // selling 100% must sell the exact balance, dust included
  setPctQty(S.side === "sell" && p === 1 ? String(assetAvail(base(S.symbol))) : String(floorTo(q, dp)));
});
/// Put a %-button quantity in the box, shown in USD when that is the unit (the exact coins are kept for the order).
function setPctQty(q) {
  S.qtyExact = null;
  if (!qtyInUsd()) form.qty.value = q;
  else {
    // no limit / trigger price typed yet: show the value at the last price; the exact coins still go out
    const px = (form.type.value === "market" ? walkBook(+q) : refPrice()) || +(S.tickers[S.symbol] || {}).last || null;
    form.qty.value = px ? String(floorTo(+q * px, 2)) : "";
    S.qtyExact = { usd: form.qty.value, qty: q, symbol: S.symbol, side: S.side, type: form.type.value };
  }
  updateEstimate();
}
form.addEventListener("submit", async (e) => {
  e.preventDefault();
  const f = Object.fromEntries(new FormData(form));
  const err = $("#order-err"); err.textContent = "";
  const qty = orderQty();
  if (qtyInUsd() && !(+qty > 0)) { err.textContent = qty == null ? t("est_unavailable") : t("qty_too_small"); return; }
  const type = f.type, body = { symbol: S.symbol, side: S.side, type, qty, note: f.note };
  if (type.includes("limit")) body.limit_price = f.limit_price.trim();
  if (type !== "limit" && type !== "market") body.trigger_price = f.trigger_price.trim();
  if (type === "limit") { body.time_in_force = f.time_in_force; body.post_only = !!f.post_only; }
  if (isPerp(S.symbol)) body.reduce_only = !!f.reduce_only;
  if (spotLev() > 1) { body.leverage = spotLev(); body.reduce_only = !!f.reduce_only; }
  try {
    const r = await api("/orders", { method: "POST", body });
    toast(`${t("order_sent")} #${r.order.id}`);
    form.qty.value = ""; form.note.value = ""; S.qtyExact = null;
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
  // the page behind is out of reach (mouse, Tab, Enter, screen readers) until the dialog closes
  M.returnFocus = document.activeElement;
  for (const id of ["#app", "#auth"]) $(id).inert = true;
  $("#modal").classList.remove("hidden");
  (input ? $("#modal-amount") : $("#modal-ok")).focus();
}
function closeModal(cancelled) {
  if ($("#modal").classList.contains("hidden")) return;
  if (cancelled && M.busy) return; // the request is already on its way; wait for its answer
  $("#modal").classList.add("hidden");
  for (const id of ["#app", "#auth"]) $(id).inert = false;
  if (M.returnFocus && M.returnFocus.isConnected && M.returnFocus !== document.body) M.returnFocus.focus();
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
// Enter in the leverage box applies the leverage; it must not also submit the order form
form.leverage.addEventListener("keydown", (e) => {
  if (e.key !== "Enter") return;
  e.preventDefault();
  saveFutSettings();
});

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
    { h: t("type"), f: (o) => esc(typeName(o.type)) + (o.post_only ? " · PO" : "") + (o.leverage ? ` · ${esc(o.leverage)}x` : "") },
    { h: t("price"), f: (o) => fmtPrice(o.symbol, o.limit_price) }, { h: t("trigger"), f: (o) => fmtPrice(o.symbol, o.trigger_price) },
    { h: t("qty"), f: (o) => fmtQty(o.qty) }, { h: t("filled"), f: (o) => fmtQty(o.filled) },
    { h: t("queue"), f: (o) => (o.status === "open" && o.queue_ahead != null ? fmtQty(o.queue_ahead) : "–") },
    { h: t("status"), f: (o) => esc(t("s_" + o.status)) },
    { h: "", f: (o) => `<button data-cancel="${o.id}">${esc(t("cancel"))}</button>` },
  ] },
  history: { load: () => api("/orders?status=closed&limit=200").then((r) => r.orders), cols: () => [
    { h: t("time"), f: (o) => fmtDT(o.created_ms) }, { h: t("market"), f: (o) => esc(o.symbol) }, { h: t("side"), f: sideCell },
    { h: t("type"), f: (o) => esc(typeName(o.type)) + (o.leverage ? ` · ${esc(o.leverage)}x` : "") }, { h: t("price"), f: (o) => fmtPrice(o.symbol, o.limit_price || o.trigger_price) },
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
  // perp positions and spot margin positions together
  positions: { load: async () => {
    const [f, m] = await Promise.all([S.cfg.perps.length ? api("/futures/account") : null, api("/margin")]);
    if (f) S.fut = f; S.margin = m;
    // options held: a long position at the average price paid, valued at the theoretical price
    const opts = (S.portfolio ? S.portfolio.assets : []).filter((a) => isOpt(a.asset) && +a.total > 0).map((a) => ({
      symbol: a.asset, side: "long", size: a.total, entry: a.avg_cost, mark: a.price, liq_price: null, margin: a.value,
      margin_mode: "option", leverage: "1", unrealized_pnl: a.unrealized_pnl || "0",
      roe_pct: a.avg_cost && +a.avg_cost > 0 ? String((+a.price / +a.avg_cost - 1) * 100) : "0", funding: "0",
    }));
    return [...(f ? f.positions : []), ...m.positions.map((p) => ({ ...p, margin_mode: "margin", funding: String(-p.rollover) })), ...opts];
  }, cols: () => [
    { h: t("market"), c: "name", f: (p) => `<a href="#" data-go="${esc(p.symbol)}">${esc(p.symbol)}</a>` },
    { h: t("side"), f: (p) => `<span class="${p.side === "long" ? "up" : "down"}">${esc(t(p.side))}</span>` },
    { h: t("size"), f: (p) => fmtQty(p.size) }, { h: t("entry_price"), f: (p) => fmtPrice(p.symbol, p.entry) },
    { h: t("mark_price"), f: (p) => fmtPrice(p.symbol, p.mark) },
    { h: t("liq_price"), f: (p) => (p.liq_price ? `<span class="down">${fmtPrice(p.symbol, p.liq_price)}</span>` : "–") },
    { h: t("margin"), f: (p) => `$${fmtUsd(p.margin)} <span class="muted">${esc(t(p.margin_mode === "margin" ? "spot_margin" : p.margin_mode === "option" ? "option_value" : p.margin_mode))}${p.margin_mode === "option" ? "" : " " + esc(p.leverage) + "x"}</span>` +
      (p.margin_mode === "isolated" ? `<button class="mini" data-addm="${esc(p.symbol)}" title="${esc(t("add_margin"))}">+</button><button class="mini" data-subm="${esc(p.symbol)}" title="${esc(t("remove_margin"))}">−</button>` : "") },
    { h: t("pnl"), f: (p) => `<span class="${cls(+p.unrealized_pnl)}">${signUsd(p.unrealized_pnl)} (${pct(+p.roe_pct)})</span>` },
    { h: t("funding_rollover"), f: (p) => `<span class="${cls(+p.funding)}">${signUsd(p.funding)}</span>` },
    { h: "", f: (p) => `<button data-close="${esc(p.symbol)}">${esc(t("close_pos"))}</button>` },
  ] },
  pos_history: { load: async () => {
    const [f, m] = await Promise.all([S.cfg.perps.length ? api("/futures/positions?limit=200") : { positions: [] }, api("/margin/positions?limit=200")]);
    const margin = m.positions.map((p) => ({ ...p, net: p.net_pnl, funding: String(-p.rollover), margin_mode: "margin", reason: p.close_reason }));
    return [...f.positions, ...margin].sort((a, b) => b.closed_ms - a.closed_ms);
  }, cols: () => [
    { h: t("time"), f: (p) => fmtDT(p.closed_ms) }, { h: t("market"), f: (p) => esc(p.symbol) },
    { h: t("side"), f: (p) => `<span class="${p.side === "long" ? "up" : "down"}">${esc(t(p.side))}</span>` },
    { h: t("size"), f: (p) => fmtQty(p.size) }, { h: t("entry_price"), f: (p) => fmtPrice(p.symbol, p.entry) },
    { h: t("exit_price"), f: (p) => fmtPrice(p.symbol, p.exit) },
    { h: t("net_pnl"), f: (p) => `<b class="${cls(+p.net)}">${signUsd(p.net)}</b>` }, { h: t("realized"), f: (p) => signUsd(p.realized) },
    { h: t("fee"), f: (p) => fmtUsd(p.fees) }, { h: t("funding_rollover"), f: (p) => signUsd(p.funding) },
    { h: t("margin"), f: (p) => `${esc(t(p.margin_mode === "margin" ? "spot_margin" : p.margin_mode))} ${esc(p.leverage)}x` },
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
  if (tab === "open") setOrders(tabRows);
  else api("/orders").then((r) => { setOrders(r.orders); $("#open-count").textContent = r.orders.length || ""; }).catch(() => {});
  const nopt = S.portfolio ? S.portfolio.assets.filter((a) => isOpt(a.asset) && +a.total > 0).length : 0;
  const npos = (S.fut ? S.fut.positions.length : 0) + (S.margin ? S.margin.positions.length : 0) + nopt;
  $("#pos-count").textContent = npos || "";
}
function renderTab() {
  const orderTab = S.tab === "open" || S.tab === "history";
  table($("#tab-table"), TABS[S.tab].cols(), tabRows, orderTab ? {
    rowId: (o) => o.id,
    rowClass: (o) => "clickable" + (S.selOrder && S.selOrder.id === o.id ? " sel" : ""),
  } : S.tab === "positions" ? {
    // coins bought on spot are holdings, not positions (as on Kraken / Binance): point to where they are
    empty: `${esc(t("no_positions"))} <a href="#" data-gotab="balances">${esc(t("see_balances"))}</a>`,
  } : {});
  if (S.tab === "open") $("#open-count").textContent = tabRows.length || "";
  $("#cancel-all").classList.toggle("hidden", S.tab !== "open" || !tabRows.length);
}
$("#tabs").addEventListener("click", (e) => {
  const b = e.target.closest("[data-tab]"); if (!b) return;
  if (b.dataset.tab !== S.tab) clearSelOrder();
  S.tab = b.dataset.tab; tabRows = [];
  $$("#tabs [data-tab]").forEach((x) => x.classList.toggle("on", x === b));
  renderTab(); loadTab();
});
let closeArmed = null;
$("#tab-table").addEventListener("click", async (e) => {
  if ((S.tab === "open" || S.tab === "history") && !e.target.closest("button, a")) {
    const tr = e.target.closest("tr[data-id]");
    const o = tr && tabRows.find((x) => String(x.id) === tr.dataset.id);
    if (o) return selectOrder(o);
  }
  const go = e.target.dataset.go;
  if (go) { e.preventDefault(); return selectSymbol(go); }
  if (e.target.dataset.gotab) { e.preventDefault(); return $(`#tabs [data-tab="${e.target.dataset.gotab}"]`).click(); }
  if (e.target.dataset.addm) return marginDialog(e.target.dataset.addm, true);
  if (e.target.dataset.subm) return marginDialog(e.target.dataset.subm, false);
  const sym = e.target.dataset.close;
  if (sym) {
    // closing at market is not undoable: ask for a second click
    if (closeArmed !== sym) { closeArmed = sym; e.target.textContent = t("confirm_close"); setTimeout(() => { if (closeArmed === sym) { closeArmed = null; renderTab(); } }, 3000); return; }
    closeArmed = null;
    try {
      const r = isOpt(sym)
        ? await api("/orders", { method: "POST", body: { symbol: sym, side: "sell", type: "market", qty: String(assetAvail(sym)), note: "close position" } })
        : await api(isPerp(sym) ? "/futures/close" : "/margin/close", { method: "POST", body: { symbol: sym } });
      toast(`${t("order_sent")} #${r.order.id}`);
    } catch (ex) { toast(ex.message, "bad"); }
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
    const [p, f, m] = await Promise.all([api("/portfolio"), S.cfg.perps.length ? api("/futures/account") : null, api("/margin")]);
    S.portfolio = p; if (f) S.fut = f; S.margin = m;
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
  if (v !== "trade") clearSelOrder();
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

# Spot margin (Kraken-style): add "leverage" to a spot order to open a position; a margin sell is a short
POST /api/orders            {"symbol":"BTC/USD","side":"sell","type":"market","qty":"0.01","leverage":2}
                            optional "reduce_only":true; margin and unrealised losses are held back from spot USD
GET  /api/margin            positions, margin level, Kraken's leverage limits and fee rates
POST /api/margin/close      {"symbol":"BTC/USD"}   market-close the whole position
GET  /api/margin/positions  closed positions

# Options on BTC and ETH, priced by this server (Black-Scholes, Kraken spot and 30-day volatility)
GET  /api/options/chain?underlying=BTC[&expiry=<ms>]   strikes with our bid / ask, delta
GET  /api/options/quote?symbol=BTC-30OCT26-80000-C      one contract
POST /api/orders            {"symbol":"BTC-30OCT26-80000-C","side":"buy","type":"market","qty":"0.1"}
                            sell only what you hold; cash-settled at expiry (08:00 UTC)

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
