// flywheel hub: scheduled digests (crates/flywheel-hub/src/digest.rs).
//
// The digest card: per workspace, turns finished, agents blocked, fan-outs
// graded, PRs, autopilot, queue stalls, spend, and the top items that
// need you (each opens its session, gate, or the activity). It shows when
// the events socket says one is ready (`digest`), when this device has
// not seen the latest yet (on load, within 12 hours), and for a
// `#digest=<id>` link (a push's click). With alerts on (notify.js) and
// the page in the background, a digest is also a system notification.
// Settings > Digest edits hub.toml [digest] (PUT /api/digest/config),
// previews, and sends one now. app-base.js creates it.
"use strict";

window.FwHubDigest = function (ctx) {
  const { $, el, icon, rawApi, store, toast } = ctx;

  const SEEN = "digest.seen"; // the last digest id this device dismissed
  const FRESH_MS = 12 * 3600 * 1000;
  const card = $("#digest-card");
  let view = null; // last GET /api/digest
  let shown = null; // the report on the card

  const money = (n) => window.FwHubFmt.dollars(Number(n || 0));
  const local = (iso) => new Date(iso).toLocaleString([], { weekday: "short", hour: "2-digit", minute: "2-digit" });

  // One line of a share's nonzero counts (as the push says it).
  function countsLine(c) {
    const parts = [];
    const add = (n, one, many) => {
      if (n > 0) parts.push(`${n} ${n === 1 ? one : many}`);
    };
    add(c.turns_finished, "turn finished", "turns finished");
    add(c.blocked_now, "blocked", "blocked");
    add(c.fanouts_graded, "fan-out graded", "fan-outs graded");
    add(c.prs_red, "PR red", "PRs red");
    add(c.prs_green, "PR green", "PRs green");
    add(c.prs_merged, "PR merged", "PRs merged");
    add(c.autopilot_decisions, "autopilot decision", "autopilot decisions");
    add(c.gates_waiting, "gate waiting", "gates waiting");
    add(c.queue_stalls, "queue stall", "queue stalls");
    if (c.spend_today_usd > 0 || c.spend_week_usd > 0) parts.push(`${money(c.spend_today_usd)} today, ${money(c.spend_week_usd)} this week`);
    return parts.join(", ");
  }

  // Open what a link names: a session, an autopilot gate, a digest.
  function follow(hash) {
    const m = /^#(session|gate|digest)=([A-Za-z0-9_-]{1,128})$/.exec(hash || "");
    if (!m) return;
    if (m[1] === "session") {
      if (ctx.sessionById(m[2])) ctx.openSession(m[2]);
      else toast("That session is gone.", { tone: "bad" });
    } else if (m[1] === "gate") {
      ctx.openGate(m[2]);
    } else {
      show(m[2]);
    }
  }

  function stat(n, label, tone) {
    return el("div", { className: `away-stat${tone ? ` ${tone}` : ""}${n ? "" : " zero"}` }, el("b", { textContent: String(n) }), el("span", { textContent: label }));
  }

  function title(report) {
    if (report.trigger === "scheduled") return `${report.label} digest`;
    return report.trigger === "preview" ? "digest preview" : "digest";
  }

  function render(report) {
    shown = report;
    const t = report.totals || {};
    const needs = (report.needs_you || []).map((item) => el(
      "li",
      {},
      el("button", {
        type: "button",
        className: `away-link${item.kind === "pr_red" || item.kind === "queue_stalled" ? "" : " blocked"}`,
        onclick: () => {
          dismiss();
          follow(item.link && item.link.hash);
        },
      }, el("b", { textContent: item.title }), item.workspace ? ` in ${item.workspace}` : "", item.since ? ` since ${local(item.since)}` : ""),
    ));
    const spaces = (report.workspaces || []).map((w) => el(
      "li",
      { className: "digest-ws" },
      el("b", { textContent: w.workspace }),
      el("span", { textContent: ` ${countsLine(w) || "-"}` }),
      w.blocked && w.blocked.length ? el("span", { className: "digest-blocked", textContent: `; blocked: ${w.blocked.map((s) => s.session).join(", ")}` }) : null,
      ...(w.fanouts || []).map((f) => el("span", { textContent: `; fan-out ${f.name}: ${f.winner ? `winner ${f.winner}` : f.leader ? `${f.leader} leads` : "none passing"} (${f.passing}/${f.candidates})` })),
    ));
    const d = report.delivery;
    const note = d
      ? d.quiet
        ? "Not sent: nothing in it."
        : `Sent to ${d.channels.length ? d.channels.join(", ") : "nowhere"}${d.push_error ? `; push failed: ${d.push_error}` : ""}.`
      : "";
    card.replaceChildren(...[
      el(
        "header",
        { className: "away-head" },
        el("span", { className: "sheet-kicker", textContent: title(report) }),
        el("h3", { textContent: `since ${local(report.since)}` }),
        el("button", { type: "button", className: "icon", title: "Dismiss", "aria-label": "Dismiss", onclick: () => dismiss() }, icon("x")),
      ),
      el(
        "div",
        { className: "away-stats" },
        stat(t.turns_finished || 0, "turns finished"),
        stat((t.blocked_now || 0) + (t.gates_waiting || 0), "need you", "bad"),
        stat(t.prs_red || 0, "PRs red", "bad"),
        stat((t.prs_green || 0) + (t.prs_merged || 0), "PRs green / merged"),
        el("div", { className: `away-stat${t.spend_today_usd ? "" : " zero"}` }, el("b", { textContent: money(t.spend_today_usd) }), el("span", { textContent: `today (${money(t.spend_week_usd)} week)` })),
      ),
      report.empty ? el("p", { className: "away-note", textContent: "Nothing happened, and nothing waits on you." }) : null,
      needs.length ? el("ul", { className: "away-list" }, needs) : null,
      spaces.length ? el("ul", { className: "away-list digest-spaces" }, spaces) : null,
      note ? el("p", { className: "away-note", textContent: note }) : null,
      el(
        "div",
        { className: "away-foot" },
        el("button", { type: "button", className: "slab", textContent: "Open activity", onclick: () => {
          dismiss();
          ctx.openActivity(report.since);
        } }),
        el("button", { type: "button", className: "slab ghost", textContent: "Dismiss", onclick: () => dismiss() }),
      ),
    ].filter(Boolean));
    card.hidden = false;
  }

  function dismiss() {
    card.hidden = true;
    if (shown && shown.id !== "preview") store.set(SEEN, shown.id);
  }

  async function show(id) {
    try {
      render(await rawApi("GET", `/api/digests/${encodeURIComponent(id)}`));
    } catch (error) {
      toast(`Digest: ${error.message}`, { tone: "bad" });
    }
  }

  // A system notification for `report` (alerts on, page in the background).
  async function notifyDevice(report) {
    const on = "Notification" in window && Notification.permission === "granted" && ctx.prefs.get("notifications.alerts", false) === true;
    if (!on || (!document.hidden && document.hasFocus())) return;
    const lines = [countsLine(report.totals || {}) || "Nothing happened, and nothing waits on you."];
    for (const item of report.needs_you || []) lines.push(`- ${item.title}`);
    const options = {
      body: lines.join("\n"),
      tag: `fw-hub-digest-${report.id}`,
      icon: "/assets/icon-192.png",
      data: { digest: report.id },
    };
    try {
      const registration = await ctx.pwa.register();
      if (registration && registration.showNotification) {
        await registration.showNotification(`flywheel ${title(report)}`, options);
        return;
      }
      const note = new Notification(`flywheel ${title(report)}`, options);
      note.onclick = () => {
        window.focus();
        note.close();
        render(report);
      };
    } catch (_) {
      // Refused by this browser: the card still shows.
    }
  }

  // The events socket's `digest`: one was just sent.
  async function onPush(message) {
    if (typeof message.id !== "string") return;
    try {
      const report = await rawApi("GET", `/api/digests/${encodeURIComponent(message.id)}`);
      render(report);
      notifyDevice(report);
      if (view) load();
    } catch (_) {
      // Gone already (another hub took it): nothing to show.
    }
  }

  async function load() {
    try {
      view = await rawApi("GET", "/api/digest");
    } catch (_) {
      view = null;
    }
    return view;
  }

  // On load: the latest scheduled digest this device has not dismissed.
  async function start() {
    await load();
    const latest = view && (view.recent || []).find((r) => !r.quiet);
    if (!latest || latest.trigger !== "scheduled" || store.get(SEEN, null) === latest.id) return;
    if (Date.now() - Date.parse(latest.until) > FRESH_MS) return;
    show(latest.id);
  }

  async function preview(since) {
    try {
      const q = since ? `?since=${encodeURIComponent(since)}` : "";
      render(await rawApi("GET", `/api/digest/preview${q}`));
    } catch (error) {
      toast(`Digest preview: ${error.message}`, { tone: "bad" });
    }
  }

  async function sendNow() {
    try {
      const report = await rawApi("POST", "/api/digest/send", {});
      render(report);
      const d = report.delivery || { channels: [] };
      toast(d.push_error ? `Digest sent to ${d.channels.join(", ") || "nowhere"}; push failed: ${d.push_error}` : `Digest sent to ${d.channels.join(", ") || "nowhere"}.`, { tone: d.push_error ? "bad" : "" });
      await load();
    } catch (error) {
      toast(`Digest: ${error.message}`, { tone: "bad" });
    }
  }

  // --- Settings > Digest ------------------------------------------------------

  function row(label, help, control) {
    return el(
      "div",
      { className: "pref-row" },
      el("label", { className: "pref-label", htmlFor: control.id }, el("span", { textContent: label }), help ? el("span", { className: "field-hint", textContent: help }) : null),
      el("div", { className: "pref-control" }, control),
      el("div", { className: "pref-scope" }),
    );
  }

  // The section's nodes; `rerender` draws it again (after a load).
  function settingsSection(rerender) {
    if (!view) {
      load().then(() => rerender());
      return [el("p", { className: "sheet-body", textContent: "Loading…" })];
    }
    const zones = typeof Intl.supportedValuesOf === "function" ? Intl.supportedValuesOf("timeZone") : [];
    const times = el("input", { type: "text", id: "digest-times", value: (view.times || []).join(", "), placeholder: "off (e.g. 08:00, 18:00)", spellcheck: false, autocomplete: "off" });
    const zone = el("input", {
      type: "text",
      id: "digest-tz",
      value: view.timezone_source === "file" ? view.timezone : "",
      placeholder: `system: ${view.timezone}`,
      spellcheck: false,
      autocomplete: "off",
    });
    zone.setAttribute("list", "digest-zones");
    const channel = (name) => el("label", { className: "cmd-check" }, el("input", { type: "checkbox", value: name, checked: (view.channels || []).includes(name), dataset: { channel: name } }), ` ${name}`);
    const channels = el("div", { id: "digest-channels", className: "digest-channels" }, ["push", "browser", "plugin"].map(channel));
    const quiet = el("input", { type: "checkbox", id: "digest-quiet", checked: view.quiet_if_empty !== false });
    const error = el("p", { className: "form-error" });
    const save = async () => {
      const list = times.value.split(/[,\s]+/).map((t) => t.trim()).filter(Boolean);
      const picked = [...channels.querySelectorAll("input[data-channel]")].filter((c) => c.checked).map((c) => c.value);
      const body = { times: list, channels: picked, quiet_if_empty: quiet.checked };
      if (zone.value.trim()) body.timezone = zone.value.trim();
      try {
        view = await rawApi("PUT", "/api/digest/config", body);
        error.textContent = "";
        toast(view.enabled ? `Digests at ${view.times.join(", ")} (${view.timezone}).` : "Scheduled digests off.");
        rerender();
      } catch (e) {
        error.textContent = e.message;
      }
    };
    const last = (view.recent || [])[0];
    const status = view.enabled
      ? `Next: ${(view.next || []).map(local).join(", ")}.`
      : "Off: no times set.";
    const lastLine = last
      ? `Last: ${last.trigger === "scheduled" ? `${last.label} digest` : last.trigger} ending ${local(last.until)}, ${last.quiet ? "not sent (empty)" : `to ${last.channels.join(", ") || "nowhere"}`}${last.push_error ? `; push failed: ${last.push_error}` : ""}.`
      : "No digest yet.";
    return [
      el(
        "p",
        { className: "sheet-body settings-note" },
        "What happened since the last digest, at the times you set: per workspace, turns, blocked agents, fan-outs, PRs, autopilot, queue stalls, spend, and the top three items that need you. Saved to ",
        el("code", { textContent: "hub.toml [digest]" }),
        " (only that table is rewritten); a slot the hub was down for is sent once on start within ",
        `${Math.round((view.grace_secs || 7200) / 3600)} h.`,
      ),
      el("datalist", { id: "digest-zones" }, zones.map((z) => el("option", { value: z }))),
      row("Times", "local, 24-hour; empty turns digests off", times),
      row("Time zone", "IANA name; empty: the hub's system zone", zone),
      row("Channels", view.push ? `push goes to ${view.push}` : "push needs notify.toml (off on this hub)", channels),
      row("Skip empty ones", "a scheduled digest with nothing in it is not sent", quiet),
      error,
      el("p", { className: "plugin-meta", textContent: `${status} ${lastLine}` }),
      view.editable === false ? el("p", { className: "form-error", textContent: "This hub runs without hub.toml: read-only." }) : null,
      el(
        "div",
        { className: "digest-actions" },
        el("button", { type: "button", className: "slab", textContent: "Save", disabled: view.editable === false, onclick: save }),
        el("button", { type: "button", className: "slab ghost", textContent: "Preview", onclick: () => preview() }),
        el("button", { type: "button", className: "slab ghost", textContent: "Send now", onclick: () => sendNow() }),
      ),
    ].filter(Boolean);
  }

  function paletteItems(cmd) {
    cmd("Digest: preview", "since the last digest, nothing sent", () => preview(), "activity");
    cmd("Digest: send now", "over the configured channels", () => sendNow(), "send");
    if (view && view.recent && view.recent.length) cmd("Digest: show the last", view.recent[0].label, () => show(view.recent[0].id), "activity");
  }

  ctx.pwa.onMessage((data) => {
    if (data.type === "fw-hub-open" && typeof data.digest === "string") show(data.digest);
  });
  card.addEventListener("keydown", (event) => {
    if (event.key === "Escape") dismiss();
  });

  return { start, onPush, follow, settingsSection, paletteItems, reload: load };
};
