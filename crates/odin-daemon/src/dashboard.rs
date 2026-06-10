//! The built-in web status dashboard: a single embedded HTML page. Reads are unauthenticated
//! (like `/metrics`); the page's approve/reject buttons HMAC-sign in the browser and call the
//! existing signed `/approve`, so the server takes no new mutating route. The JSON the page polls
//! is the shared [`odin_core::RunView`] projection (so the CLI's `odin status --json` and this API
//! agree); the axum handlers + routing live in `webhook.rs`.

/// The dashboard single-page app, served at `GET /`. No build step, no external assets, no
/// third-party JS. Approve/reject sign in-browser with the Web Crypto API (available on
/// `localhost` and over HTTPS), so the webhook secret never leaves the operator's browser.
///
/// The render is **stateful**: cards are keyed by `run_id` and reconciled in place each poll, so a
/// half-typed approval note, an open diff, focus, and scroll all survive the 3s refresh. Click
/// handling is delegated on `#runs` (a reconciled card keeps working without per-element rebinding).
pub(crate) const HTML: &str = r##"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Odin</title>
<style>
  :root {
    color-scheme: dark;
    --bg: #0a0e14; --bg-card: #13171f; --bg-sunken: #0d1017;
    --border: #232b38; --border-hi: #38424f;
    --text: #e6eaf0; --text-dim: #99a1b3; --text-mut: #a6adba;
    --accent: #60a5fa; --ok: #34d399; --warn: #fbbf24; --bad: #f87171;
    --s1: 4px; --s2: 8px; --s3: 12px; --s4: 16px; --s5: 20px; --s6: 24px;
    --r-sm: 6px; --r-md: 8px; --r-lg: 12px;
    --shadow: 0 1px 2px rgba(0,0,0,.22), 0 1px 3px rgba(0,0,0,.14);
    --font: -apple-system, BlinkMacSystemFont, system-ui, "Segoe UI", Roboto, sans-serif;
    --mono: ui-monospace, SFMono-Regular, "SF Mono", Menlo, Consolas, monospace;
  }
  * { box-sizing: border-box; }
  body { margin: 0; font: 14px/1.55 var(--font); background: var(--bg); color: var(--text);
         -webkit-font-smoothing: antialiased; }
  :focus-visible { outline: 2px solid var(--accent); outline-offset: 2px; border-radius: 3px; }

  header { position: sticky; top: 0; z-index: 5; background: var(--bg-card);
           border-bottom: 1px solid var(--border); padding: var(--s3) var(--s4);
           display: flex; gap: var(--s4); align-items: center; flex-wrap: wrap; }
  header h1 { font-size: 17px; font-weight: 700; margin: 0; letter-spacing: -.01em; }
  header h1 b { color: var(--accent); }
  .chips { display: flex; gap: var(--s2); flex-wrap: wrap; align-items: center; }
  .chip { font-size: 12px; line-height: 1.4; padding: 4px 10px; border-radius: 999px; cursor: pointer;
          background: var(--bg-sunken); border: 1px solid var(--border); color: var(--text-dim); font: inherit; }
  .chip:hover { border-color: var(--border-hi); color: var(--text); }
  .chip.active { background: #0f263e; border-color: var(--accent); color: var(--text); }
  .chip .n { color: var(--text-mut); margin-left: 5px; font-variant-numeric: tabular-nums; }
  .chip.active .n { color: var(--accent); }
  .spacer { flex: 1; }
  .cfg { display: flex; gap: var(--s2); }
  .cfg input { background: var(--bg-sunken); border: 1px solid var(--border-hi); color: var(--text);
               border-radius: var(--r-sm); padding: var(--s2) var(--s3); font: inherit; }
  .cfg input::placeholder { color: var(--text-mut); }
  #tick { color: var(--text-mut); font-size: 12px; min-width: 92px; text-align: right; }

  main { padding: var(--s4); max-width: 1080px; margin: 0 auto;
         display: flex; flex-direction: column; gap: var(--s3); }
  .run { background: var(--bg-card); border: 1px solid var(--border); border-radius: var(--r-lg);
         padding: var(--s3) var(--s4); box-shadow: var(--shadow); }
  .run.await { border-color: var(--warn); box-shadow: var(--shadow), 0 0 0 1px rgba(251,191,36,.28); }
  .row1 { display: flex; align-items: center; gap: var(--s3); }
  .wf { font-weight: 600; font-size: 15px; }
  .id { color: var(--text-mut); font-family: var(--mono); font-size: 12px; }
  .dur { color: var(--text-mut); font-size: 12px; margin-left: auto; white-space: nowrap; font-variant-numeric: tabular-nums; }
  .ago { color: var(--text-mut); font-size: 12px; margin-left: 8px; white-space: nowrap; }
  .badge { font-size: 11px; font-weight: 600; padding: 3px 9px; border-radius: 999px;
           text-transform: uppercase; letter-spacing: .04em; }
  .b-succeeded { background: #10381f; color: var(--ok); }
  .b-failed, .b-cancelled { background: #431515; color: var(--bad); }
  .b-running, .b-pending { background: #0f263e; color: var(--accent); }
  .b-awaiting_approval { background: #3d2f10; color: var(--warn); }
  .steps { display: flex; flex-wrap: wrap; gap: var(--s2); margin-top: var(--s3); }
  .step { font-size: 12px; padding: 3px 8px; border-radius: var(--r-md); background: var(--bg-sunken);
          border: 1px solid var(--border); color: var(--text-dim); font-family: var(--mono); }
  .step .g { margin-right: var(--s1); font-family: var(--font); }
  .g-passed { color: var(--ok); } .g-failed { color: var(--bad); } .g-skipped { color: var(--text-mut); }
  .g-running { color: var(--accent); } .g-awaiting_approval { color: var(--warn); } .g-pending { color: var(--text-mut); }
  .gate { margin-top: var(--s3); padding-top: var(--s3); border-top: 1px dashed var(--border-hi); }
  .gate .msg { color: var(--warn); margin-bottom: var(--s2); font-weight: 500; }
  .gate .ctl { display: flex; gap: var(--s2); flex-wrap: wrap; align-items: center; }
  .gate .note { flex: 1; min-width: 200px; background: var(--bg-sunken); border: 1px solid var(--border-hi);
                color: var(--text); border-radius: var(--r-sm); padding: var(--s2) var(--s3); font: inherit; }
  .gate .note::placeholder { color: var(--text-mut); }
  .gate label { font-size: 12px; color: var(--text-dim); display: inline-flex; align-items: center; gap: var(--s1); }
  button { font: inherit; border: 0; border-radius: var(--r-sm); padding: var(--s2) var(--s3);
           cursor: pointer; color: #06121f; font-weight: 600; }
  .approve { background: var(--ok); } .approve:hover { background: #2cc28a; }
  .reject { background: var(--bad); } .reject:hover { background: #f1606b; }
  .link { background: none; color: var(--accent); padding: var(--s1) 0; font-weight: 500;
          border-bottom: 1px solid transparent; }
  .link:hover { border-bottom-color: var(--accent); }
  pre.diff { margin: var(--s3) 0 0; padding: var(--s3); background: var(--bg-sunken);
             border: 1px solid var(--border); border-radius: var(--r-md); overflow: auto;
             max-height: 360px; font: 12px/1.5 var(--mono); white-space: pre; }
  pre.diff .add { color: var(--ok); } pre.diff .del { color: var(--bad); } pre.diff .hd { color: var(--accent); }
  .err { color: var(--bad); font-size: 12px; margin-top: var(--s2); white-space: pre-wrap; font-family: var(--mono); }
  .gate .err { margin-top: 0; }
  .sr-only { position: absolute; width: 1px; height: 1px; padding: 0; margin: -1px; overflow: hidden;
             clip: rect(0,0,0,0); white-space: nowrap; border: 0; }
  button[disabled] { opacity: .5; cursor: default; }
  #toast { position: fixed; bottom: var(--s5); left: 50%; transform: translateX(-50%) translateY(8px);
           background: var(--bg-card); color: var(--text); padding: var(--s3) var(--s4);
           border-radius: var(--r-md); box-shadow: var(--shadow); opacity: 0;
           transition: opacity .2s, transform .2s; pointer-events: none; border: 1px solid var(--border-hi);
           font-size: 13px; max-width: min(520px, 90vw); }
  #toast.show { opacity: 1; transform: translateX(-50%) translateY(0); }
  .empty { color: var(--text-mut); text-align: center; padding: var(--s6) var(--s4);
           display: flex; align-items: center; justify-content: center; gap: var(--s2); }
  .spinner { width: 14px; height: 14px; border: 2px solid var(--border-hi); border-top-color: var(--accent);
             border-radius: 50%; animation: spin .7s linear infinite; }
  @keyframes spin { to { transform: rotate(360deg); } }
  .refresh { background: var(--bg-sunken); color: var(--text-dim); border: 1px solid var(--border-hi);
             border-radius: var(--r-sm); padding: var(--s1) var(--s2); font-size: 14px; line-height: 1; }
  .refresh:hover { color: var(--text); }
  .banner { background: #3d2f10; color: var(--warn); text-align: center; padding: var(--s2) var(--s4);
            font-size: 13px; border-bottom: 1px solid rgba(251,191,36,.3); position: sticky; top: 0; z-index: 4; }
  main.dim { opacity: .45; transition: opacity .2s; }
  #scrim { position: fixed; inset: 0; background: rgba(0,0,0,.45); opacity: 0; pointer-events: none;
           transition: opacity .22s; z-index: 19; }
  #scrim.open { opacity: 1; pointer-events: auto; }
  #drawer { position: fixed; top: 0; right: 0; height: 100vh; width: min(560px, 92vw);
            background: var(--bg-card); border-left: 1px solid var(--border-hi);
            box-shadow: -8px 0 28px rgba(0,0,0,.32); transform: translateX(100%);
            transition: transform .22s ease; z-index: 20; display: flex; flex-direction: column; }
  #drawer.open { transform: translateX(0); }
  #drawer .dhead { display: flex; align-items: center; gap: var(--s3); padding: var(--s3) var(--s4);
                   border-bottom: 1px solid var(--border); }
  #drawer .dhead .x { margin-left: auto; background: none; color: var(--text-dim); font-size: 18px;
                      padding: 0 var(--s2); font-weight: 400; }
  #drawer .dhead .x:hover { color: var(--text); }
  #drawer .dbody { padding: var(--s4); overflow: auto; flex: 1; }
  #drawer pre.diff { max-height: none; margin-top: var(--s3); }
  .trunc { color: var(--text-mut); font-style: italic; }
  @media (prefers-reduced-motion: reduce) { .spinner { animation: none; } #toast, #drawer, #scrim { transition: none; } }
  @media (max-width: 640px) {
    header { padding: var(--s3); gap: var(--s2); row-gap: var(--s3); }
    header h1 { font-size: 16px; }
    .spacer { display: none; }
    #tick { display: none; }
    .cfg { order: 3; flex: 1 1 100%; }
    .cfg input { flex: 1; min-width: 0; }
    .chips { order: 2; flex: 1 1 100%; }
    main { padding: var(--s3); gap: var(--s2); }
    .run { padding: var(--s3); }
    .gate .note { min-width: 0; flex-basis: 100%; }
    #drawer { width: 100vw; }
  }
</style>
</head>
<body>
<header>
  <h1><b>Odin</b> dashboard</h1>
  <div class="chips" id="chips" role="group" aria-label="filter runs by status"></div>
  <div class="spacer"></div>
  <div class="cfg">
    <input id="approver" placeholder="your name" size="10" aria-label="your name (recorded as the approver)" title="recorded as the approver">
    <input id="secret" type="password" placeholder="webhook secret" size="16" aria-label="webhook secret" title="used to sign approve/reject in your browser; never sent except as a signature">
  </div>
  <button id="refresh" class="refresh" title="refresh now" aria-label="refresh now">↻</button>
  <span id="tick" aria-live="off"></span>
</header>
<div id="banner" class="banner" role="status" hidden></div>
<main id="runs" role="region" aria-label="workflow runs"><div class="empty"><span class="spinner"></span> loading…</div></main>
<div id="toast" role="status" aria-live="polite" aria-atomic="true"></div>
<div id="scrim"></div>
<aside id="drawer" aria-hidden="true" aria-label="run detail" tabindex="-1">
  <div class="dhead">
    <span class="badge" id="d-badge"></span><span class="wf" id="d-wf"></span><span class="id" id="d-id"></span>
    <button class="x" id="d-close" aria-label="close detail">✕</button>
  </div>
  <div class="dbody" id="d-body"></div>
</aside>
<script>
const $ = (s, r=document) => r.querySelector(s);
const enc = new TextEncoder();
const esc = s => String(s ?? "").replace(/[&<>"']/g, c => ({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}[c]));
let lastFetch = 0, polledOnce = false, runsById = {}, lastRuns = [], filter = "all";
const POLL_BASE = 3000, POLL_MAX = 30000;
let pollDelay = POLL_BASE, pollTimer = null;

// Persist the secret + approver locally so they survive a reload (never auto-sent to the server).
// Guarded: private-mode / disabled storage throws — degrade to in-session rather than break the page.
const store = {
  get(k) { try { return localStorage.getItem(k); } catch { return null; } },
  set(k, v) { try { localStorage.setItem(k, v); } catch (e) { if (!store._warned) { store._warned = 1; toast("storage unavailable — secret won't persist", false); } } },
};
for (const id of ["secret", "approver"]) {
  const el = $("#"+id);
  el.value = store.get("odin."+id) || "";
  el.addEventListener("input", () => store.set("odin."+id, el.value));
}
filter = store.get("odin.filter") || "all";   // remembered status filter (set by the chips below)

function toast(msg, ok=true) {
  const t = $("#toast");
  t.textContent = msg; t.style.borderColor = ok ? "#2e7d46" : "#7d2e34";
  t.classList.add("show"); clearTimeout(toast._t);
  toast._t = setTimeout(() => t.classList.remove("show"), 3200);
}

function ago(iso) {
  const s = Math.max(0, (Date.now() - new Date(iso)) / 1000);
  if (s < 60) return Math.floor(s) + "s ago";
  if (s < 3600) return Math.floor(s/60) + "m ago";
  if (s < 86400) return Math.floor(s/3600) + "h ago";
  return Math.floor(s/86400) + "d ago";
}

// Formats a wall-clock duration in ms; matches the CLI's `fmt_duration_ms` for the sub-minute case
// (`Nms` / `N.Ns`) and rolls over to minutes for longer runs. Empty string for absent/negative.
function dur(ms) {
  if (ms == null || ms < 0) return "";
  if (ms < 1000) return ms + "ms";
  const totalS = Math.floor(ms / 1000);
  if (totalS < 60) return Math.floor(ms/1000) + "." + Math.floor((ms%1000)/100) + "s";
  const m = Math.floor(totalS / 60), s = totalS % 60;
  return m + "m" + (s ? " " + s + "s" : "");
}

async function sign(secret, body) {
  if (!secret) throw new Error("set the webhook secret (top right) to approve or reject");
  if (!crypto.subtle) throw new Error("signing needs a secure context — use http://localhost or HTTPS");
  const key = await crypto.subtle.importKey("raw", enc.encode(secret), {name:"HMAC", hash:"SHA-256"}, false, ["sign"]);
  const mac = await crypto.subtle.sign("HMAC", key, enc.encode(body));
  return "sha256=" + [...new Uint8Array(mac)].map(b => b.toString(16).padStart(2,"0")).join("");
}

// Friendly copy for a resulting run status (the /approve response carries the live status).
const friendly = st => ({ succeeded: "completed", failed: "failed", awaiting_approval: "paused at next gate", running: "running" }[st] || (st ? st.replace("_"," ") : ""));

async function decide(runId, decision, card) {
  const noteEl = card.querySelector("[data-note]");
  const rerunEl = card.querySelector("[data-rerun]");
  const errEl = card.querySelector("[data-gate-err]");
  const btns = [...card.querySelectorAll("[data-act]")];
  // Inline error keeps the operator in context (and keeps the note filled), with a toast to match.
  const showErr = m => { if (errEl) { errEl.textContent = m; errEl.hidden = false; } toast(m, false); };
  if (errEl) errEl.hidden = true;
  const note = (noteEl?.value || "").trim();
  if (decision === "rejected" && !note) { showErr("a reject needs a note (the feedback)"); noteEl?.focus(); return; }
  const rerun = decision === "rejected" && !!rerunEl?.checked;
  const body = JSON.stringify({ run_id: runId, decision, approver: $("#approver").value.trim() || "dashboard", note, rerun });
  btns.forEach(b => b.disabled = true);   // no double-submit while the async sign+POST is in flight
  try {
    const sig = await sign($("#secret").value.trim(), body);
    const res = await fetch("/approve", { method: "POST", headers: {"Content-Type":"application/json","X-Hub-Signature-256":sig}, body });
    if (!res.ok) { showErr(decision + " failed: " + res.status + " " + (await res.text())); return; }
    // A plain decision returns a RunSummary (top-level `status`); a reject-with-rerun returns a
    // RerunOutcome { rejected, rerun } with NO top-level status — read the rerun's status instead.
    const out = await res.json().catch(() => ({}));
    if (rerun) toast("rejected — reran (" + friendly(out.rerun?.status) + ")");
    else toast((decision === "approved" ? "approved — " : "rejected — ") + friendly(out.status));
    if (noteEl) noteEl.value = "";          // clear interaction state so the next poll reconciles this card
    if (rerunEl) rerunEl.checked = false;
    btns.forEach(b => b.blur());
    poll();
  } catch (e) { showErr(e.message); }
  finally { btns.forEach(b => b.disabled = false); }
}

// Colorize a unified diff, capping huge diffs (first 500 + last 100 lines) so a 1MB diff can't
// blow up the DOM / memory. Truncation is applied to the line array before colorizing.
function renderDiff(text) {
  let lines = text.split("\n");
  let trunc = "";
  if (lines.length > 650) {
    const dropped = lines.length - 600;
    trunc = `<span class="trunc">… ${dropped} lines collapsed …</span>\n`;
    lines = [...lines.slice(0, 500), "__ODIN_TRUNC__", ...lines.slice(-100)];
  }
  const colored = lines.map(l =>
    l === "__ODIN_TRUNC__" ? trunc.trimEnd() :
    l.startsWith("+") ? '<span class="add">'+esc(l)+'</span>' :
    l.startsWith("-") ? '<span class="del">'+esc(l)+'</span>' :
    (l.startsWith("@@") || l.startsWith("diff ")) ? '<span class="hd">'+esc(l)+'</span>' : esc(l)).join("\n");
  return '<pre class="diff">' + colored + '</pre>';
}

let drawerRunId = null;
const drawer = $("#drawer"), scrim = $("#scrim");

function closeDrawer() {
  drawer.classList.remove("open"); scrim.classList.remove("open");
  drawer.setAttribute("aria-hidden", "true"); drawerRunId = null;
}

async function openDrawer(runId, run) {
  drawerRunId = runId;
  $("#d-badge").className = "badge b-" + (run?.status || "");
  $("#d-badge").textContent = (run?.status || "").replace("_"," ");
  $("#d-wf").textContent = run?.workflow || "";
  $("#d-id").textContent = runId.slice(0,8);
  $("#d-body").innerHTML = '<div class="empty"><span class="spinner"></span> loading…</div>';
  drawer.classList.add("open"); scrim.classList.add("open");
  drawer.setAttribute("aria-hidden", "false"); drawer.focus();
  try {
    const d = await (await fetch("/api/runs/" + encodeURIComponent(runId))).json();
    if (drawerRunId !== runId) return;   // a newer open won the race
    let html = "";
    if (d && d.error) html += `<div class="err">${esc(d.error)}</div>`;   // run-level error (detail-only)
    html += d && d.diff ? renderDiff(d.diff) : '<pre class="diff">(no diff captured)</pre>';
    $("#d-body").innerHTML = html;
  } catch (e) { if (drawerRunId === runId) $("#d-body").innerHTML = '<pre class="diff">failed to load detail</pre>'; }
}

$("#d-close").addEventListener("click", closeDrawer);
scrim.addEventListener("click", closeDrawer);
document.addEventListener("keydown", e => { if (e.key === "Escape" && drawerRunId) closeDrawer(); });

function glyph(s) { return {passed:"✓", failed:"✗", skipped:"⊘", running:"⏳", awaiting_approval:"⏸", pending:"·"}[s] || "·"; }

// A signature over only what a card RENDERS — excludes `updated_at`, so a poll that merely bumps
// the timestamp does not blow the card away (the relative time updates separately, see tick()).
function cardSig(r) {
  return JSON.stringify([r.status, r.workflow, r.gate ? r.gate.message : null, r.duration_ms ?? null,
    r.steps.map(s => [s.id, s.status, s.exit_code, s.error || null, s.duration_ms ?? null])]);
}

function cardHTML(r, sig) {
  // The glyph is decorative (aria-hidden); a visually-hidden span carries the status word so a
  // screen reader announces e.g. "passed build (0)" rather than just "build (0)".
  const steps = r.steps.map(s => {
    const d = dur(s.duration_ms);
    const title = esc(s.id) + (d ? " — " + d : "") + (s.error ? " — " + esc(s.error) : "");
    return `<span class="step" title="${title}"><span class="g g-${s.status}" aria-hidden="true">${glyph(s.status)}</span><span class="sr-only">${s.status.replace("_"," ")} </span>${esc(s.id)}${s.exit_code!=null?` (${s.exit_code})`:""}</span>`;
  }).join("");
  // Failed steps carry their error in the list payload — surface it inline (not just a hover title).
  const stepErrs = r.steps.filter(s => s.status === "failed" && s.error)
    .map(s => `<div class="err">${esc(s.id)}: ${esc(s.error)}</div>`).join("");
  let gate = "";
  if (r.gate) {
    gate = `<div class="gate"><div class="msg">⏸ ${esc(r.gate.message || "Awaiting approval")}</div>
      <div class="ctl">
        <input class="note" data-note placeholder="note / feedback (required to reject)">
        <label><input type="checkbox" data-rerun> rerun</label>
        <button class="approve" data-act="approved">✓ Approve</button>
        <button class="reject" data-act="rejected">✗ Reject</button>
      </div>
      <div class="err gate-err" data-gate-err hidden></div></div>`;
  }
  const hasDiff = r.steps.some(s => s.status === "passed" || s.status === "failed");
  return `<div class="run ${r.gate?"await":""}" data-run-id="${esc(r.run_id)}" data-sig="${esc(sig)}">
    <div class="row1">
      <span class="badge b-${r.status}" aria-label="status: ${r.status.replace("_"," ")}">${r.status.replace("_"," ")}</span>
      <span class="wf">${esc(r.workflow)}</span>
      <span class="id">${esc(r.run_id.slice(0,8))}</span>
      <span class="dur" title="${r.duration_ms!=null?"run duration":""}">${dur(r.duration_ms)}</span>
      <span class="ago" data-iso="${esc(r.updated_at)}" title="${esc(r.updated_at)}">${ago(r.updated_at)}</span>
    </div>
    <div class="steps">${steps}</div>
    ${stepErrs}
    ${gate}
    ${hasDiff ? `<button class="link" data-diff="${esc(r.run_id)}">details</button>` : ""}
  </div>`;
}

function nodeFrom(html) { const t = document.createElement("template"); t.innerHTML = html.trim(); return t.content.firstElementChild; }

// A card is "busy" when the operator is mid-interaction — don't replace its DOM (which would lose a
// half-typed note, the rerun choice, focus, or an open diff). Covers focus AND lingering state.
function isBusy(el) {
  if (el.contains(document.activeElement)) return true;
  const n = el.querySelector("[data-note]"); if (n && n.value.trim()) return true;
  const rr = el.querySelector("[data-rerun]"); if (rr && rr.checked) return true;
  return false;   // the diff now lives in a separate drawer, so an open detail no longer pins the card
}

// Stateful reconcile: key by run_id, patch only changed (non-busy) cards, keep order, never wipe
// the whole list. Clicks are handled by one delegated listener (below), so a replaced card's
// buttons keep working with no rebinding.
// One filter chip per status actually present, plus an always-on "all"; each carries a live count.
// The active chip survives reloads (persisted), but resets to "all" if its status drains away.
const CHIP_ORDER = ["awaiting_approval", "running", "succeeded", "failed", "cancelled", "pending"];
function renderChips(runs) {
  const counts = runs.reduce((a, r) => (a[r.status] = (a[r.status]||0)+1, a), {});
  const chips = [["all", "all", runs.length], ...CHIP_ORDER.filter(s => counts[s]).map(s => [s, s.replace("_"," "), counts[s]])];
  $("#chips").innerHTML = chips.map(([val, label, n]) =>
    `<button class="chip${filter===val?" active":""}" data-filter="${val}" aria-pressed="${filter===val}">${label}<span class="n">${n}</span></button>`).join("");
}

function render(runs) {
  runsById = {}; for (const r of runs) runsById[r.run_id] = r;
  lastRuns = runs;
  // If the filtered-on status drained away, fall back to "all" so the operator isn't stuck on an empty view.
  if (filter !== "all" && !runs.some(r => r.status === filter)) { filter = "all"; store.set("odin.filter", "all"); }
  renderChips(runs);
  const main = $("#runs");
  if (!runs.length) {
    main.innerHTML = polledOnce ? '<div class="empty">no runs yet</div>' : '<div class="empty"><span class="spinner"></span> loading…</div>';
    return;
  }
  const shown = filter === "all" ? runs : runs.filter(r => r.status === filter);
  if (!shown.length) {   // runs exist but none match the active chip
    const msg = "no " + filter.replace("_"," ") + " runs";
    const cur = main.querySelector(".empty");
    if (!cur || cur.textContent !== msg) main.innerHTML = `<div class="empty">${esc(msg)}</div>`;
    return;
  }
  const ids = new Set(shown.map(r => r.run_id));
  for (const el of [...main.children]) if (!el.dataset.runId || !ids.has(el.dataset.runId)) el.remove();
  // The one action that matters — a paused run awaiting approval — floats to the top; the rest keep
  // the server's newest-first order. `filter` is stable, so order within each group is preserved.
  const sorted = [...shown.filter(r => r.gate), ...shown.filter(r => !r.gate)];
  let prev = null;
  for (const r of sorted) {
    const sig = cardSig(r);
    let el = main.querySelector(`:scope > [data-run-id="${CSS.escape(r.run_id)}"]`);
    if (!el) {
      el = nodeFrom(cardHTML(r, sig));
      main.insertBefore(el, prev ? prev.nextSibling : main.firstChild);
    } else if (el.dataset.sig !== sig && !isBusy(el)) {
      const fresh = nodeFrom(cardHTML(r, sig));
      el.replaceWith(fresh); el = fresh;
    }
    const want = prev ? prev.nextSibling : main.firstChild;
    if (el !== want && !el.contains(document.activeElement)) main.insertBefore(el, want);
    prev = el;
  }
  // The drawer's diff/error is a snapshot, but keep its status badge live if the run advances.
  if (drawerRunId && runsById[drawerRunId]) {
    const r = runsById[drawerRunId];
    $("#d-badge").className = "badge b-" + r.status;
    $("#d-badge").textContent = r.status.replace("_"," ");
  }
}

$("#runs").addEventListener("click", e => {
  const act = e.target.closest("[data-act]");
  if (act) { const card = act.closest("[data-run-id]"); decide(card.dataset.runId, act.dataset.act, card); return; }
  const dl = e.target.closest("[data-diff]");
  if (dl) openDrawer(dl.dataset.diff, runsById[dl.dataset.diff]);
});

// Status filter chips — purely client-side over the last fetched runs, so toggling is instant and
// the choice is remembered. Re-render off `lastRuns` (no refetch).
$("#chips").addEventListener("click", e => {
  const c = e.target.closest("[data-filter]");
  if (!c) return;
  filter = c.dataset.filter;
  store.set("odin.filter", filter);
  render(lastRuns);
});

// Relative times tick on their own (no re-render). Card times are relative to the server's
// `updated_at`; the "updated …" indicator stays on the client clock to avoid clock-skew artifacts.
function tick() {
  for (const el of document.querySelectorAll(".ago[data-iso]")) el.textContent = ago(el.dataset.iso);
  $("#tick").textContent = lastFetch ? "updated " + ago(new Date(lastFetch).toISOString()) : "";
}

function setUnreachable(on) {
  const b = $("#banner");
  if (on) { b.textContent = "daemon unreachable — retrying in " + Math.round(pollDelay/1000) + "s…"; b.hidden = false; $("#runs").classList.add("dim"); }
  else { b.hidden = true; $("#runs").classList.remove("dim"); }
}

function schedule(delay) { clearTimeout(pollTimer); pollTimer = setTimeout(poll, delay); }

// Self-rescheduling poll: a 5s abort guard, exponential backoff (3→6→…→30s) with a distinct
// "unreachable" banner on failure (the last list stays visible but dimmed), and no polling while
// the tab is hidden. Recovers to the base interval on the first success.
async function poll() {
  if (document.hidden) { schedule(POLL_BASE); return; }
  const ctrl = new AbortController();
  const guard = setTimeout(() => ctrl.abort(), 5000);
  try {
    const runs = await (await fetch("/api/runs?limit=50", { signal: ctrl.signal })).json();
    render(runs); lastFetch = Date.now(); polledOnce = true;
    pollDelay = POLL_BASE; setUnreachable(false);
  } catch (e) {
    pollDelay = Math.min(pollDelay * 2, POLL_MAX); setUnreachable(true);
  } finally { clearTimeout(guard); schedule(pollDelay); }
}

$("#refresh").addEventListener("click", () => { clearTimeout(pollTimer); pollDelay = POLL_BASE; poll(); });
document.addEventListener("visibilitychange", () => { if (!document.hidden) { clearTimeout(pollTimer); pollDelay = POLL_BASE; poll(); } });

setInterval(tick, 1000);
poll();
</script>
</body>
</html>
"##;
