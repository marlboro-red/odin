# Walkthrough: wiring a GitHub webhook end-to-end

This is the soup-to-nuts version of [`daemon.md`'s webhook reference](daemon.md#webhook-triggers):
how to take a workflow with a `github_webhook` trigger and actually make GitHub drive it — the two
steps the reference assumes you've solved (**exposing the local daemon** and **creating the webhook
on GitHub**), plus the full open-a-PR → review → approve → comment loop.

The worked example is the dogfood itself — [`examples/adversarial-review.yaml`](../examples/adversarial-review.yaml),
which has Odin review pull requests on its own repo — but the steps are the same for any
webhook-triggered workflow (e.g. [`issue-to-pr.yaml`](../examples/issue-to-pr.yaml)).

```
   GitHub (PR opened)
        │  pull_request.opened  (HMAC-signed delivery)
        ▼
   public URL ──tunnel──▶ odind  POST /webhook
        verify signature → match event+repo → map params → dispatch
        ▼
   workflow run  (fetch → reviewers → synthesize → approval gate ⏸)
        │  POST /approve  (signed)        resumes inline
        ▼
   gh pr comment  → the review lands on the PR
```

## Prerequisites

- **`odind` built**: `cargo build -p odin-daemon --release` (binary at `target/release/odind`).
- **The CLIs the workflow shells out to**, on `PATH`: `gh` (authenticated, with the `repo` scope so
  it can read diffs and post comments), and the agent CLIs your provider steps use
  (`claude` / `codex` / `copilot`).
- **A way to expose `127.0.0.1:9292` publicly** — GitHub can't reach your laptop's loopback. This
  guide uses a [cloudflared](https://github.com/cloudflare/cloudflared) quick tunnel
  (`brew install cloudflared`); [`ngrok`](https://ngrok.com) or a deployed host work the same way.
- **Admin on the target repo** (to add a webhook), or a `gh` token with the `repo` scope (which
  includes managing repo webhooks).

> **Tip — prove the wiring before spending on agents.** A `github_webhook` → `run: "echo …"`
> workflow lets you confirm signature/match/dispatch with a cheap echo before pointing a real,
> agent-and-comment workflow at it.

---

## 1. The workflow

Put the workflow in a directory `odind` will serve. Its `triggers:` block decides what fires it and
which payload fields become params:

```yaml
# examples/adversarial-review.yaml (excerpt)
triggers:
  - type: github_webhook
    events: ["pull_request.opened"]   # bare "pull_request" matches any action
    repo: marlboro-red/odin           # optional owner/repo filter (case-insensitive)
    params:
      pr: number                      # map payload fields → typed run params (dot-path)
      repo: repository.full_name
```

```sh
mkdir -p ~/odin-prod/workflows
cp examples/adversarial-review.yaml ~/odin-prod/workflows/
odin validate ~/odin-prod/workflows/adversarial-review.yaml   # sanity check
```

Reference the **typed params** (`params.pr`, `params.repo`) in your steps — not raw `trigger.*`.
A raw `trigger.*` value reaching a shell is flagged ([ODIN031](workflow-reference.md#odin031));
a param is not. **Caveat:** ODIN031 does *not* shell-escape a param — it only nudges you off
`trigger.*`. Keep any param you interpolate into a `run:`/`shell.exec` either **numeric** or pinned
by the trigger's `repo:` filter, so an attacker-controlled payload string can't inject.

## 2. A secret + the daemon

GitHub signs each delivery with a shared secret; `odind` verifies it. Generate one and start the
daemon (it **fails closed** — it won't start a webhook/approval endpoint without a secret unless you
pass `--webhook-allow-unsigned`):

```sh
export ODIN_WEBHOOK_SECRET=$(openssl rand -hex 32)

# --repo .   the git repo runs provision worktrees from
# --dashboard   serve the approve/status page at http://127.0.0.1:9292/
target/release/odind \
  --workflows ~/odin-prod/workflows \
  --repo . \
  --webhook-addr 127.0.0.1:9292 \
  --dashboard
```

You should see:

```
INFO odind: http server configured subscriptions=1 approvals=true
INFO odind: http server listening (/webhook, /approve, /metrics, /health) addr=127.0.0.1:9292
```

`subscriptions=1` confirms the `github_webhook` trigger was wired; `approvals=true` confirms the
`approval:` gate exposed `/approve`.

## 3. Expose it publicly

In a second terminal, open a tunnel to the daemon and copy the URL it prints:

```sh
cloudflared tunnel --url http://127.0.0.1:9292
#  …  https://maybe-conversations-properly-boulevard.trycloudflare.com
export TUNNEL=https://maybe-conversations-properly-boulevard.trycloudflare.com

curl -s -o /dev/null -w '%{http_code}\n' "$TUNNEL/health"   # → 200
```

> A cloudflared **quick tunnel** gets a fresh random URL each run and dies when you stop it — fine
> for a demo. For anything persistent see [Going to production](#going-to-production).

## 4. Create the webhook on GitHub

Either in the browser (**repo → Settings → Webhooks → Add webhook**: Payload URL = `$TUNNEL/webhook`,
Content type = `application/json`, Secret = your `$ODIN_WEBHOOK_SECRET`, events = "Pull requests"),
or with `gh`:

```sh
gh api repos/marlboro-red/odin/hooks -X POST \
  -f "config[url]=$TUNNEL/webhook" \
  -f "config[content_type]=json"   \
  -f "config[secret]=$ODIN_WEBHOOK_SECRET" \
  -f 'events[]=pull_request' \
  -F active=true
```

GitHub immediately sends a `ping`. `odind` verifies its signature and acks it — you'll see
`webhook: delivery accepted label=ping … matched=0` (no subscription matches `ping`, so nothing
runs). The hook is now live.

## 5. Trigger it

Open a pull request on the repo. The `pull_request.opened` delivery flows through:

```
INFO webhook: delivery accepted label=pull_request.opened … matched=1
INFO dispatch{…workflow=adversarial-review}: dispatching run
INFO …: run started
INFO …: step finished step=fetch status=Passed
INFO …: step finished step=review_security status=Passed     # the three reviewers,
INFO …: step finished step=review_correctness status=Passed  #   concurrent scratch worktrees
INFO …: step finished step=review_design status=Passed
INFO …: step finished step=synthesize status=Passed
INFO …: run paused for approval gate=Some(StepId("gate"))    # status=AwaitingApproval
```

(`matched=1` is the routing working; `run paused for approval` is the durable gate — the run is
checkpointed and survives a daemon restart until it's decided.)

## 6. Approve over HTTP

The gate holds until you decide. Approve it from the **`--dashboard` page** (the buttons sign in
your browser with the secret), or with the signed `/approve` endpoint — grab the `run_id` from the
`run paused` log line:

```sh
RUN=<run-id-from-the-log>
BODY=$(printf '{"run_id":"%s","decision":"approved","approver":"you"}' "$RUN")
SIG=$(printf '%s' "$BODY" | openssl dgst -sha256 -hmac "$ODIN_WEBHOOK_SECRET" | awk '{print $NF}')

curl -sS http://127.0.0.1:9292/approve \
  -H "X-Hub-Signature-256: sha256=$SIG" \
  -H "Content-Type: application/json"   \
  -d "$BODY"
```

`/approve` resumes the run **inline** and answers with the resulting
[`RunSummary`](cli.md#json-shapes). The `post` step runs `gh pr comment`, and its output is the
comment URL:

```
… step finished step=gate status=Passed
… step finished step=post status=Passed
… approve: decision applied run=<run-id> status=Succeeded
```

See [Approving a paused run over HTTP](daemon.md#approving-a-paused-run-over-http) for the full
request/response contract (reject, `note`, `rerun`, status codes).

## 7. Verify

```sh
gh pr view <pr-number> --json comments \
  --jq '.comments[] | {author: .author.login, url}'
```

The review is on the PR. 🎉

---

## Going to production

The quick tunnel and the foreground `odind` are demo scaffolding. For a setup that survives a
laptop reboot and reviews *every* PR:

- **A stable public URL** — a [named cloudflared tunnel](https://developers.cloudflare.com/cloudflare-one/connections/connect-networks/)
  bound to a DNS name, an `ngrok` reserved domain, or `odind` deployed on a small always-on host.
  Re-create the webhook against that URL (a stable URL means you set the webhook once).
- **TLS**: `odind` has no built-in TLS. cloudflared/ngrok terminate it for you; a self-hosted bind
  to a non-loopback address over plain HTTP logs a warning — put a TLS-terminating reverse proxy in
  front of it so signatures don't travel in cleartext.
- **Run it as a service** (systemd/launchd) with `ODIN_WEBHOOK_SECRET` in the unit's environment,
  `--max-concurrent-runs` tuned for your agent budget, and `--prune-interval`/`--prune-older-than`
  to bound the run store. See [`daemon.md`](daemon.md).
- **Approvals at scale**: keep `--dashboard` on so a human can approve from a page, or wire
  `/approve` into your own tooling.

## Teardown

```sh
gh api repos/marlboro-red/odin/hooks --jq '.[] | {id, url: .config.url}'   # find the id
gh api -X DELETE repos/marlboro-red/odin/hooks/<id>                        # delete the webhook
# Ctrl-C the cloudflared tunnel and odind (it drains in-flight runs first).
```

## Troubleshooting

| Symptom | Cause / fix |
|---|---|
| Webhook delivery shows **401** in GitHub's *Recent Deliveries* | Secret mismatch — the webhook's secret ≠ `$ODIN_WEBHOOK_SECRET`. Re-set one of them. |
| Delivery **202** but `matched=0`, nothing runs | The event/repo didn't match a subscription. Check the workflow's `events:` (`pull_request.opened` vs bare `pull_request`) and the `repo:` filter (case-insensitive `owner/repo`). |
| `odind` refuses to start: *"…exposes a network endpoint, but no secret is configured"* | A webhook trigger or approval gate needs a secret. Set `--webhook-secret`/`$ODIN_WEBHOOK_SECRET`, or pass `--webhook-allow-unsigned` for local-only testing. |
| Run sits at `AwaitingApproval` forever | It's a gated workflow — approve it (step 6) or from the dashboard. |
| GitHub can't reach the URL (delivery times out) | The tunnel died or the URL changed (quick tunnels rotate). Restart the tunnel and update the webhook URL. |
| A run fails param validation right after dispatch | A `params:` dot-path didn't resolve in the payload (the mapping is best-effort; an unresolved required param fails the run). Check the field path against an actual delivery payload. |

---

See also: [`daemon.md`](daemon.md) (the daemon + endpoint reference), the
[`adversarial-review`](../examples/adversarial-review.yaml) and
[`local-review`](../examples/local-review.yaml) examples, and the
[integration guide](integration-guide.md) for embedding the `WebhookServer` in your own binary.
