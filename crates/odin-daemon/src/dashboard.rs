//! The built-in web status dashboard: a single embedded HTML page. Reads are unauthenticated
//! (like `/metrics`); the page's approve/reject buttons HMAC-sign in the browser and call the
//! existing signed `/approve`, so the server takes no new mutating route. The JSON the page polls
//! is the shared [`odin_core::RunView`] projection (so the CLI's `odin status --json` and this API
//! agree); the axum handlers + routing live in `webhook.rs`.

/// The dashboard single-page app, served at `GET /`. No build step, no external assets, no
/// third-party JS. Approve/reject sign in-browser with the Web Crypto API (available on
/// `localhost` and over HTTPS), so the webhook secret never leaves the operator's browser.
pub(crate) const HTML: &str = r##"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Odin</title>
<style>
  :root { color-scheme: dark; }
  * { box-sizing: border-box; }
  body { margin: 0; font: 14px/1.5 -apple-system, system-ui, sans-serif; background: #0f1115; color: #d7dae0; }
  header { position: sticky; top: 0; background: #161922; border-bottom: 1px solid #262b36;
           padding: 12px 18px; display: flex; gap: 14px; align-items: center; flex-wrap: wrap; }
  header h1 { font-size: 16px; margin: 0; letter-spacing: .04em; }
  header h1 b { color: #6ea8fe; }
  .counts { color: #8b93a1; font-size: 13px; }
  .counts b { color: #d7dae0; }
  .spacer { flex: 1; }
  .cfg { display: flex; gap: 8px; align-items: center; }
  .cfg input { background: #0f1115; border: 1px solid #2a3140; color: #d7dae0; border-radius: 6px; padding: 5px 8px; font: inherit; }
  #tick { color: #6b7280; font-size: 12px; min-width: 86px; text-align: right; }
  main { padding: 16px 18px; max-width: 1100px; margin: 0 auto; display: flex; flex-direction: column; gap: 10px; }
  .run { background: #161922; border: 1px solid #262b36; border-radius: 10px; padding: 12px 14px; }
  .run.await { border-color: #b9852a; box-shadow: 0 0 0 1px #b9852a33; }
  .row1 { display: flex; align-items: center; gap: 10px; }
  .wf { font-weight: 600; }
  .id { color: #6b7280; font-family: ui-monospace, monospace; font-size: 12px; }
  .ago { color: #6b7280; font-size: 12px; margin-left: auto; }
  .badge { font-size: 11px; padding: 2px 8px; border-radius: 999px; text-transform: uppercase; letter-spacing: .04em; }
  .b-succeeded { background: #15331f; color: #58d68d; }
  .b-failed, .b-cancelled { background: #3a1b1b; color: #f1707a; }
  .b-running, .b-pending { background: #16263f; color: #6ea8fe; }
  .b-awaiting_approval { background: #3a2c12; color: #f0b24a; }
  .steps { display: flex; flex-wrap: wrap; gap: 6px; margin-top: 9px; }
  .step { font-size: 12px; padding: 2px 7px; border-radius: 6px; background: #0f1115; border: 1px solid #262b36; color: #aab2c0; }
  .step .g { margin-right: 5px; }
  .g-passed { color: #58d68d; } .g-failed { color: #f1707a; } .g-skipped { color: #6b7280; }
  .g-running { color: #6ea8fe; } .g-awaiting_approval { color: #f0b24a; } .g-pending { color: #6b7280; }
  .gate { margin-top: 11px; padding-top: 11px; border-top: 1px dashed #2a3140; }
  .gate .msg { color: #f0b24a; margin-bottom: 8px; }
  .gate .ctl { display: flex; gap: 8px; flex-wrap: wrap; align-items: center; }
  .gate input { flex: 1; min-width: 180px; background: #0f1115; border: 1px solid #2a3140; color: #d7dae0; border-radius: 6px; padding: 6px 8px; font: inherit; }
  button { font: inherit; border: 0; border-radius: 6px; padding: 6px 12px; cursor: pointer; color: #0f1115; font-weight: 600; }
  .approve { background: #58d68d; } .reject { background: #f1707a; }
  .link { background: none; color: #6ea8fe; padding: 2px 0; font-weight: 400; text-decoration: underline; }
  pre.diff { margin: 9px 0 0; padding: 10px; background: #0b0d11; border: 1px solid #262b36; border-radius: 8px;
             overflow: auto; max-height: 360px; font: 12px/1.45 ui-monospace, monospace; white-space: pre; }
  pre.diff .add { color: #58d68d; } pre.diff .del { color: #f1707a; } pre.diff .hd { color: #6ea8fe; }
  .err { color: #f1707a; font-size: 12px; margin-top: 6px; white-space: pre-wrap; }
  #toast { position: fixed; bottom: 18px; left: 50%; transform: translateX(-50%); background: #222; color: #fff;
           padding: 9px 16px; border-radius: 8px; opacity: 0; transition: opacity .2s; pointer-events: none; border: 1px solid #333; }
  #toast.show { opacity: 1; }
  .empty { color: #6b7280; text-align: center; padding: 40px; }
</style>
</head>
<body>
<header>
  <h1><b>Odin</b> dashboard</h1>
  <div class="counts" id="counts"></div>
  <div class="spacer"></div>
  <div class="cfg">
    <input id="approver" placeholder="your name" size="10" title="recorded as the approver">
    <input id="secret" type="password" placeholder="webhook secret" size="16" title="used to sign approve/reject in your browser; never sent except as a signature">
  </div>
  <span id="tick"></span>
</header>
<main id="runs"><div class="empty">loading…</div></main>
<div id="toast"></div>
<script>
const $ = (s, r=document) => r.querySelector(s);
const enc = new TextEncoder();
const esc = s => String(s ?? "").replace(/[&<>"']/g, c => ({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}[c]));
let lastFetch = 0;

// Persist the secret + approver locally so they survive a reload (never auto-sent to the server).
for (const id of ["secret", "approver"]) {
  const el = $("#"+id);
  el.value = localStorage.getItem("odin."+id) || (id === "approver" ? "" : "");
  el.addEventListener("input", () => localStorage.setItem("odin."+id, el.value));
}

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

async function sign(secret, body) {
  if (!secret) throw new Error("set the webhook secret (top right) to approve or reject");
  if (!crypto.subtle) throw new Error("signing needs a secure context — use http://localhost or HTTPS");
  const key = await crypto.subtle.importKey("raw", enc.encode(secret), {name:"HMAC", hash:"SHA-256"}, false, ["sign"]);
  const mac = await crypto.subtle.sign("HMAC", key, enc.encode(body));
  return "sha256=" + [...new Uint8Array(mac)].map(b => b.toString(16).padStart(2,"0")).join("");
}

async function decide(runId, decision, noteEl) {
  const note = (noteEl?.value || "").trim();
  if (decision === "rejected" && !note) { toast("a reject needs a note (the feedback)", false); noteEl?.focus(); return; }
  const rerun = decision === "rejected" && $("#rerun-"+CSS.escape(runId))?.checked;
  const body = JSON.stringify({ run_id: runId, decision, approver: $("#approver").value.trim() || "dashboard", note, rerun });
  try {
    const sig = await sign($("#secret").value.trim(), body);
    const res = await fetch("/approve", { method: "POST", headers: {"Content-Type":"application/json","X-Hub-Signature-256":sig}, body });
    if (!res.ok) { toast(decision + " failed: " + res.status + " " + (await res.text()), false); return; }
    toast(decision === "approved" ? "approved — resuming" : (rerun ? "rejected — rerunning with feedback" : "rejected"));
    poll();
  } catch (e) { toast(e.message, false); }
}

async function showDiff(runId, host) {
  if (host.dataset.open) { host.innerHTML = ""; delete host.dataset.open; return; }
  host.dataset.open = "1"; host.textContent = "loading…";
  try {
    const d = await (await fetch("/api/runs/" + encodeURIComponent(runId))).json();
    if (!d || !d.diff) { host.innerHTML = '<pre class="diff">(no diff captured)</pre>'; return; }
    const colored = esc(d.diff).split("\n").map(l =>
      l.startsWith("+") ? '<span class="add">'+l+'</span>' :
      l.startsWith("-") ? '<span class="del">'+l+'</span>' :
      (l.startsWith("@@") || l.startsWith("diff ")) ? '<span class="hd">'+l+'</span>' : l).join("\n");
    host.innerHTML = '<pre class="diff">' + colored + '</pre>';
  } catch (e) { host.innerHTML = '<pre class="diff">failed to load diff</pre>'; }
}

function glyph(s) { return {passed:"✓", failed:"✗", skipped:"⊘", running:"⏳", awaiting_approval:"⏸", pending:"·"}[s] || "·"; }

function render(runs) {
  const counts = runs.reduce((a, r) => (a[r.status] = (a[r.status]||0)+1, a), {});
  $("#counts").innerHTML = ["running","awaiting_approval","succeeded","failed"]
    .filter(s => counts[s]).map(s => `<b>${counts[s]}</b> ${s.replace("_"," ")}`).join(" · ") || "no runs yet";
  const main = $("#runs");
  if (!runs.length) { main.innerHTML = '<div class="empty">no runs yet</div>'; return; }
  main.innerHTML = runs.map(r => {
    const steps = r.steps.map(s =>
      `<span class="step" title="${esc(s.error||"")}"><span class="g g-${s.status}">${glyph(s.status)}</span>${esc(s.id)}${s.exit_code!=null?` (${s.exit_code})`:""}</span>`).join("");
    let gate = "";
    if (r.gate) {
      gate = `<div class="gate"><div class="msg">⏸ ${esc(r.gate.message || "Awaiting approval")}</div>
        <div class="ctl">
          <input id="note-${esc(r.run_id)}" placeholder="note / feedback (required to reject)">
          <label style="font-size:12px;color:#8b93a1"><input type="checkbox" id="rerun-${esc(r.run_id)}"> rerun</label>
          <button class="approve" data-act="approved" data-run="${esc(r.run_id)}">✓ Approve</button>
          <button class="reject" data-act="rejected" data-run="${esc(r.run_id)}">✗ Reject</button>
        </div></div>`;
    }
    const hasDiff = r.steps.some(s => s.status === "passed" || s.status === "failed");
    return `<div class="run ${r.gate?"await":""}">
      <div class="row1">
        <span class="badge b-${r.status}">${r.status.replace("_"," ")}</span>
        <span class="wf">${esc(r.workflow)}</span>
        <span class="id">${esc(r.run_id.slice(0,8))}</span>
        <span class="ago">${ago(r.updated_at)}</span>
      </div>
      <div class="steps">${steps}</div>
      ${gate}
      ${hasDiff ? `<button class="link" data-diff="${esc(r.run_id)}">diff</button><div class="diffhost"></div>` : ""}
    </div>`;
  }).join("");

  main.querySelectorAll("button[data-act]").forEach(b =>
    b.onclick = () => decide(b.dataset.run, b.dataset.act, $("#note-"+CSS.escape(b.dataset.run))));
  main.querySelectorAll("button[data-diff]").forEach(b =>
    b.onclick = () => showDiff(b.dataset.diff, b.parentElement.querySelector(".diffhost")));
}

async function poll() {
  try {
    const runs = await (await fetch("/api/runs?limit=50")).json();
    render(runs); lastFetch = Date.now();
  } catch (e) { $("#counts").textContent = "(daemon unreachable)"; }
}

setInterval(() => { $("#tick").textContent = lastFetch ? "updated " + ago(new Date(lastFetch).toISOString()) : ""; }, 1000);
setInterval(poll, 3000);
poll();
</script>
</body>
</html>
"##;
