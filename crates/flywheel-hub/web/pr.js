// flywheel hub: pull requests and CI. A session whose branch has a pull
// request shows a badge on its row (number, checks); the Review drawer
// gets a PR section: state, review decision, mergeability, the checks
// with status, duration, and link, a failing check's log, and the
// actions -- ask the agent to fix CI (one tap: pasted at rest, else
// queued), re-run failed checks, mark ready, merge (confirmed; checks
// failing or a review missing: only with the PR number typed back).
//
// API: GET /api/sessions/{id}/pr[?refresh=1], GET .../pr/log?check=,
// POST .../pr/fix, POST .../pr/{rerun,ready,merge} ({dry_run} plans,
// {confirm, force?} runs). Changes arrive as session re-lists (the row's
// `pr` brief) and `pr.*` activity rows.
"use strict";

window.FwHubPr = function (ctx) {
  const { el, icon, api, sessionById, toast } = ctx;

  // A view older than this is read again when the section is shown.
  const STALE_MS = 60000;
  const panels = new Map(); // session id -> {node, data, error, loading, at, brief, logs, busy}

  // replaceChildren, skipping the null a conditional child gives.
  const fill = (node, ...children) => node.replaceChildren(...children.filter((c) => c !== null && c !== undefined && c !== false));
  const plural = (n, word) => `${n} ${word}${n === 1 ? "" : "s"}`;

  function duration(secs) {
    if (secs === undefined || secs === null) return "";
    if (secs < 60) return `${secs}s`;
    if (secs < 3600) return `${Math.floor(secs / 60)}m ${String(secs % 60).padStart(2, "0")}s`;
    return `${Math.floor(secs / 3600)}h ${String(Math.floor((secs % 3600) / 60)).padStart(2, "0")}m`;
  }

  // The brief's tone: merged, closed, fail, pending, pass, none.
  function toneOf(brief) {
    if (brief.state === "merged" || brief.state === "closed") return brief.state;
    const c = brief.checks || {};
    if (c.failing) return "fail";
    if (c.pending) return "pending";
    if (c.passing) return "pass";
    return "none";
  }

  function checksText(c) {
    if (!c || !c.total) return "no checks";
    const parts = [];
    if (c.failing) parts.push(`${c.failing} failing`);
    if (c.pending) parts.push(`${c.pending} running`);
    if (c.passing) parts.push(`${c.passing} passing`);
    if (c.skipped) parts.push(`${c.skipped} skipped`);
    return parts.join(", ");
  }

  const words = (w) => (w ? w.replace(/_/g, " ") : "");
  // Only a web address becomes a link (never a `javascript:` URL).
  const webLink = (url) => (typeof url === "string" && /^https?:\/\//i.test(url) ? url : null);
  const failingBucket = (c) => c.bucket === "fail" || c.bucket === "cancel";

  // --- the row badge ---------------------------------------------------------

  function rowChip(session) {
    const brief = session && session.pr;
    if (!brief) return null;
    const tone = toneOf(brief);
    const c = brief.checks || {};
    const mark = {
      fail: `✗${c.failing}`,
      pending: "●",
      pass: "✓",
      merged: "merged",
      closed: "closed",
      none: "",
    }[tone];
    const title = [
      `PR #${brief.number} ${brief.state}`,
      checksText(c),
      brief.review ? `review: ${words(brief.review)}` : "",
      brief.mergeable === "conflicting" ? "conflicts with its base" : "",
      brief.stale ? `stale: ${brief.stale}` : "",
    ].filter(Boolean).join(" · ");
    return el(
      "button",
      {
        type: "button",
        className: `pr-chip ${tone}${brief.state === "draft" ? " draft" : ""}${brief.stale ? " stale" : ""}`,
        title,
        "aria-label": title,
        onclick: async (event) => {
          event.stopPropagation();
          await ctx.openSession(session.id);
          ctx.openReview(session.id);
        },
      },
      el("span", { textContent: `#${brief.number}` }),
      brief.state === "draft" ? el("i", { className: "pr-chip-draft", textContent: "draft" }) : null,
      mark ? el("b", { textContent: mark }) : null,
    );
  }

  // --- the Review drawer section ---------------------------------------------

  function panelOf(id) {
    let panel = panels.get(id);
    if (!panel) {
      panel = {
        node: el("section", { className: "pr-panel", "aria-label": "Pull request" }),
        data: null,
        error: "",
        loading: false,
        at: 0,
        brief: undefined,
        logs: new Map(), // check name -> {open, loading, view, error, job, pre, bottom}
        dueAt: 0, // when the hub asks GitHub again on its own (ms)
        foot: null,
        busy: "",
      };
      panels.set(id, panel);
    }
    return panel;
  }

  // The section for `session` (re-read when stale or its badge moved).
  function panel(session) {
    if (!session) return null;
    const p = panelOf(session.id);
    const brief = JSON.stringify(session.pr || null);
    const changed = p.brief !== undefined && p.brief !== brief;
    p.brief = brief;
    if (!p.loading && (!p.data || changed || Date.now() - p.at > STALE_MS)) load(session.id, false);
    paint(session.id);
    return p.node;
  }

  async function load(id, refresh) {
    const p = panelOf(id);
    p.loading = true;
    paint(id);
    try {
      const data = await api("GET", `/api/sessions/${id}/pr${refresh ? "?refresh=1" : ""}`);
      p.data = data;
      p.error = "";
      p.dueAt = data.next_check_secs === null || data.next_check_secs === undefined ? 0 : Date.now() + data.next_check_secs * 1000;
      // A log stays open only under a check still failing in the same job.
      const failing = new Map((data.checks || []).filter(failingBucket).map((c) => [c.name, c.job_id || c.run_id || 0]));
      for (const [name, log] of [...p.logs]) {
        if (!failing.has(name) || (log.job !== undefined && log.job !== failing.get(name))) p.logs.delete(name);
      }
      if (refresh && data.throttled_secs !== undefined) {
        toast(`GitHub was asked ${data.throttled_secs}s ago: this is that answer.`, { timeout: 3500 });
      }
    } catch (error) {
      p.error = error.message;
    } finally {
      p.loading = false;
      p.at = Date.now();
    }
    paint(id);
  }

  // "checked 18s ago · next in 41s", as of now.
  function footText(p) {
    const data = p.data;
    if (!data) return "";
    const checked = data.checked_at ? `checked ${ctx.age(data.checked_at)} ago` : "";
    let next = "";
    if (p.dueAt) {
      const left = Math.round((p.dueAt - Date.now()) / 1000);
      next = left > 0 ? `next in ${duration(left)}` : "asking GitHub soon";
    }
    return [p.busy ? `${p.busy}...` : "", p.loading ? "asking GitHub..." : checked, next].filter(Boolean).join(" · ");
  }

  // Each second: the footers count down; one past its time re-reads the
  // hub's cache (the watcher asked GitHub by then).
  setInterval(() => {
    const now = Date.now();
    for (const [id, p] of panels) {
      if (!p.foot || !p.node.isConnected) continue;
      p.foot.textContent = footText(p);
      if (p.data && p.dueAt && !p.loading && !p.busy && now > p.dueAt + 3000 && now - p.at > 15000) load(id, false);
    }
  }, 1000);

  function button(label, opts) {
    return el("button", {
      type: "button",
      className: `slab${opts.ghost ? " ghost" : ""} pr-act`,
      textContent: label,
      title: opts.title || "",
      disabled: !!opts.disabled || !!opts.busy,
      dataset: { action: opts.action || "" },
      onclick: opts.onclick,
    });
  }

  function paint(id) {
    const p = panels.get(id);
    if (!p) return;
    const data = p.data;
    const refresh = el(
      "button",
      {
        type: "button",
        className: "icon pr-refresh",
        title: "Ask GitHub now",
        "aria-label": "Refresh the pull request",
        disabled: p.loading,
        onclick: () => load(id, true),
      },
      icon("redo"),
    );
    const label = el("span", { className: "pr-label" }, icon("branch"), "Pull request");
    if (!data) {
      p.node.dataset.tone = p.error ? "bad" : "none";
      fill(p.node, 
        el("div", { className: "pr-head" }, label, refresh),
        el("p", { className: `pr-note${p.error ? " bad" : ""}`, textContent: p.error || "Asking GitHub..." }),
      );
      return;
    }
    const pr = data.pr;
    const problem = data.problem;
    if (!pr) {
      p.node.dataset.tone = problem ? "bad" : "none";
      const why = problem ? problem.reason : data.note || `No pull request for ${data.branch} yet: push it and Open PR.`;
      fill(p.node, 
        el("div", { className: "pr-head" }, label, el("span", { className: "pr-quiet", textContent: data.branch }), refresh),
        el("p", { className: `pr-note${problem ? " bad" : ""}`, textContent: why }),
      );
      return;
    }
    const summary = data.summary || {};
    const shown = pr.state === "open" && pr.draft ? "draft" : pr.state;
    p.node.dataset.tone = toneOf({ state: shown, checks: summary });
    const failing = data.checks.filter(failingBucket);
    const open = pr.state === "open";
    const busy = p.busy;
    const noAgent = !data.agent;
    const head = el(
      "div",
      { className: "pr-head" },
      label,
      webLink(pr.url)
        ? el("a", { className: "pr-num", href: webLink(pr.url), target: "_blank", rel: "noopener noreferrer", textContent: `#${pr.number}`, title: "Open on GitHub" })
        : el("span", { className: "pr-num", textContent: `#${pr.number}` }),
      el("span", { className: `pr-state ${shown}`, textContent: shown }),
      pr.review ? el("span", { className: `pr-tag ${pr.review}`, textContent: words(pr.review) }) : null,
      open && pr.mergeable ? el("span", { className: `pr-tag ${pr.mergeable}`, title: pr.merge_state ? `merge state: ${words(pr.merge_state)}` : "", textContent: pr.mergeable === "mergeable" ? "no conflicts" : words(pr.mergeable) }) : null,
      refresh,
    );
    const actions = open
      ? el(
        "div",
        { className: "pr-actions" },
        failing.length
          ? button("Ask agent to fix CI", {
            action: "fix",
            busy,
            disabled: noAgent,
            title: noAgent ? "No agent session runs in this folder to ask" : `Paste ${failing[0].name}'s failure and its log's tail into ${data.agent.name} (queued if it is working)`,
            onclick: () => fix(id, null),
          })
          : null,
        failing.length
          ? button("Re-run failed…", { action: "rerun", ghost: true, busy, onclick: () => act(id, "rerun") })
          : null,
        pr.draft ? button("Ready for review…", { action: "ready", ghost: true, busy, onclick: () => act(id, "ready") }) : null,
        button("Merge PR…", {
          action: "merge",
          // A draft merges only once ready for review.
          ghost: pr.draft || !!failing.length || !!summary.pending,
          disabled: pr.draft,
          busy,
          title: pr.draft ? "A draft: mark it ready for review first" : `gh pr merge ${pr.number}, pinned to ${pr.head_oid.slice(0, 10)}`,
          onclick: () => act(id, "merge"),
        }),
      )
      : null;
    const rows = data.checks.map((check) => checkRow(id, check, open && !noAgent));
    p.foot = el("p", { className: "pr-note pr-foot", textContent: footText(p) });
    // Open logs keep where they were scrolled to; a new one starts at its
    // end (where the failure is).
    const scrolls = new Map();
    for (const [name, log] of p.logs) if (log.pre && log.pre.isConnected) scrolls.set(name, log.pre.scrollTop);
    fill(p.node, 
      head,
      el("p", { className: "pr-title", textContent: pr.title }),
      actions,
      el(
        "p",
        { className: "pr-sum" },
        el("b", { textContent: data.checks.length ? checksText(summary) : "no checks reported" }),
        el("span", { textContent: ` · ${pr.head} → ${pr.base}` }),
      ),
      rows.length ? el("ul", { className: "pr-checks" }, rows) : null,
      problem ? el("p", { className: "pr-note bad", textContent: `Stale: ${problem.reason}` }) : null,
      p.foot,
    );
    requestAnimationFrame(() => {
      for (const [name, log] of p.logs) {
        if (!log.pre || !log.pre.isConnected) continue;
        if (scrolls.has(name) && !log.bottom) log.pre.scrollTop = scrolls.get(name);
        else {
          log.pre.scrollTop = log.pre.scrollHeight;
          log.bottom = false;
        }
      }
    });
  }

  function checkRow(id, check, canAsk) {
    const p = panelOf(id);
    const failing = failingBucket(check);
    const log = p.logs.get(check.name);
    const link = webLink(check.link);
    if (log && log.open && log.view && !log.pre) {
      log.pre = el("pre", { className: "pr-log", textContent: log.view.text || "(empty)" });
      log.bottom = true;
    }
    const word = { pass: "pass", fail: "fail", pending: "running", skipping: "skipped", cancel: "cancelled" }[check.bucket] || check.bucket;
    const took = check.duration_secs !== undefined ? duration(check.duration_secs) : check.started_at && check.bucket === "pending" ? `${ctx.age(check.started_at)}` : "";
    return el(
      "li",
      { className: `pr-check ${check.bucket}` },
      el(
        "div",
        { className: "pr-check-line" },
        el("span", { className: "pr-bucket", textContent: word }),
        el("span", { className: "pr-name", textContent: check.name, title: check.description || check.name }),
        check.workflow ? el("span", { className: "pr-wf", textContent: check.workflow }) : null,
        took ? el("span", { className: "pr-dur", textContent: took }) : null,
        el(
          "span",
          { className: "pr-check-acts" },
          link ? el("a", { href: link, target: "_blank", rel: "noopener noreferrer", textContent: "details" }) : null,
          failing
            ? el("button", {
              type: "button",
              className: "text-btn",
              textContent: log && log.open ? "Hide log" : "View log",
              disabled: !check.run_id,
              title: check.run_id ? "gh run view --log-failed (tail, secrets redacted)" : "Not a GitHub Actions check: open its details",
              onclick: () => toggleLog(id, check.name),
            })
            : null,
          failing && canAsk
            ? el("button", { type: "button", className: "text-btn", textContent: "Ask agent", title: `Ask the agent to fix ${check.name}`, onclick: () => fix(id, check.name) })
            : null,
        ),
      ),
      log && log.open
        ? log.view
          ? el(
            "div",
            { className: "pr-log-wrap" },
            el("p", { className: "pr-note", textContent: `Last ${log.view.shown} of ${log.view.lines} lines · ${log.view.command}${log.view.truncated ? " · longer than the hub reads" : ""}` }),
            log.pre,
          )
          : el("p", { className: `pr-note${log.error ? " bad" : ""}`, textContent: log.error || "Reading the log..." })
        : null,
    );
  }

  async function toggleLog(id, name) {
    const p = panelOf(id);
    const log = p.logs.get(name) || { open: false };
    const check = p.data && (p.data.checks || []).find((c) => c.name === name);
    if (check) log.job = check.job_id || check.run_id || 0;
    log.open = !log.open;
    if (log.open) log.bottom = true;
    p.logs.set(name, log);
    paint(id);
    if (!log.open || log.view || log.loading) return;
    log.loading = true;
    try {
      log.view = await api("GET", `/api/sessions/${id}/pr/log?check=${encodeURIComponent(name)}`);
      log.error = "";
    } catch (error) {
      log.error = error.message;
    } finally {
      log.loading = false;
    }
    paint(id);
  }

  async function fix(id, check) {
    const p = panelOf(id);
    if (p.busy) return;
    p.busy = "asking the agent";
    paint(id);
    try {
      const report = await api("POST", `/api/sessions/${id}/pr/fix`, check ? { check } : {});
      const d = report.delivery || {};
      toast(
        d.delivery === "queued"
          ? `Queued for ${report.agent.name} (${d.why}): it goes once its turn is done.`
          : `Asked ${report.agent.name} to fix ${report.check}.`,
        { tone: "ok" },
      );
    } catch (error) {
      toast(`Not sent: ${error.message}`, { tone: "bad", timeout: 9000 });
    } finally {
      p.busy = "";
      paint(id);
    }
  }

  // A plan's text for a confirm dialog.
  function planText(plan) {
    return [
      ...(plan.details || []),
      ...(plan.commands || []).map((c) => `$ ${c}`),
      ...(plan.warnings || []).map((w) => `Note: ${w}`),
    ].join("\n");
  }

  async function act(id, action) {
    const p = panelOf(id);
    if (p.busy) return;
    p.busy = "planning";
    paint(id);
    try {
      const plan = await api("POST", `/api/sessions/${id}/pr/${action}`, { dry_run: true });
      let force = null;
      if (plan.refusal && !plan.forcible) {
        toast(plan.refusal, { tone: "bad", timeout: 9000 });
        return;
      }
      if (plan.refusal) {
        force = await typedConfirm({
          title: `Merge #${plan.force_word} anyway?`,
          body: `${plan.refusal}.\n\n${planText(plan)}${plan.force_note ? `\nNote: ${plan.force_note}` : ""}`,
          word: plan.force_word,
        });
        if (force === null) return;
      } else {
        const ok = await ctx.confirmDialog({
          kicker: "pull request",
          title: plan.summary,
          body: planText(plan),
          ok: { merge: "Merge", rerun: "Re-run", ready: "Mark ready" }[action],
        });
        if (!ok) return;
      }
      p.busy = { merge: "merging", rerun: "re-running", ready: "marking ready" }[action];
      paint(id);
      const outcome = await api("POST", `/api/sessions/${id}/pr/${action}`, { confirm: plan.fingerprint, force });
      toast(outcome.ok ? outcome.summary : `Failed: ${outcome.summary}`, { tone: outcome.ok ? "ok" : "bad", timeout: outcome.ok ? 4500 : 12000 });
    } catch (error) {
      toast(error.message, { tone: "bad", timeout: 9000 });
    } finally {
      p.busy = "";
      await load(id, false);
    }
  }

  // A confirm that needs `word` typed back; resolves to it, or null.
  let typed = null;
  function typedConfirm({ title, body, word }) {
    if (!typed) {
      const input = el("input", { type: "text", className: "pr-typed-input", autocomplete: "off", inputMode: "numeric", spellcheck: false });
      const ok = el("button", { type: "submit", className: "slab danger", value: "ok", textContent: "Merge anyway" });
      const form = el(
        "form",
        { method: "dialog", className: "pr-typed-form" },
        el("p", { className: "sheet-kicker", textContent: "forced merge" }),
        el("h2", { className: "pr-typed-title" }),
        el("pre", { className: "pr-typed-body" }),
        el("label", { className: "pr-typed-label" }, el("span"), input),
        el(
          "div",
          { className: "pr-typed-actions" },
          el("button", { type: "button", className: "slab ghost", value: "cancel", textContent: "Cancel", onclick: () => typed.dialog.close("cancel") }),
          ok,
        ),
      );
      const dialog = el("dialog", { className: "pr-typed", "aria-label": "Forced merge" }, form);
      document.body.append(dialog);
      input.addEventListener("input", () => {
        ok.disabled = input.value.trim() !== typed.word;
      });
      typed = { dialog, input, ok, form, word: "" };
    }
    typed.word = String(word);
    typed.form.querySelector(".pr-typed-title").textContent = title;
    typed.form.querySelector(".pr-typed-body").textContent = body;
    typed.form.querySelector(".pr-typed-label span").textContent = `Type ${word} (the pull request's number) to merge anyway`;
    typed.input.value = "";
    typed.ok.disabled = true;
    typed.dialog.returnValue = "";
    if (document.body.lastElementChild !== typed.dialog) document.body.append(typed.dialog);
    typed.dialog.showModal();
    typed.input.focus();
    return new Promise((resolve) => {
      typed.dialog.addEventListener(
        "close",
        () => resolve(typed.dialog.returnValue === "ok" && typed.input.value.trim() === typed.word ? typed.word : null),
        { once: true },
      );
    });
  }

  // --- events ----------------------------------------------------------------

  const ROW_TEXT = {
    "pr.checks_failed": ["CI failed", "bad"],
    "pr.checks_passed": ["CI passed", "ok"],
    "pr.review_changes_requested": ["Changes requested", "bad"],
    "pr.merged": ["Merged", "ok"],
  };

  // A journal row (this hub's or a machine's): a `pr.*` one re-reads its
  // section, toasts, and (alerts on, page in the background) notifies
  // this device.
  function onActivity(row) {
    if (!row || !ROW_TEXT[row.kind]) return;
    const [title, tone] = ROW_TEXT[row.kind];
    const session = row.session_id ? sessionById(row.session_id) : null;
    const id = session ? session.id : null;
    if (id && panels.has(id)) load(id, false);
    const name = row.session || (session && session.name) || "";
    const text = `${name ? `${name}: ` : ""}${row.text}`;
    toast(text, {
      tone,
      timeout: tone === "bad" ? 9000 : 4500,
      actions: id
        ? [el("button", { type: "button", className: "text-btn", textContent: "Review", onclick: async () => {
          await ctx.openSession(id);
          ctx.openReview(id);
        } })]
        : [],
    });
    if (ctx.notify && id) ctx.notify().alert(id, `${title}${name ? `: ${name}` : ""}`, row.text, tone === "bad");
  }

  function paletteItems(cmd) {
    const session = sessionById(ctx.attached());
    if (!session || !session.pr) return;
    const brief = session.pr;
    cmd(`Pull request #${brief.number}: ${session.name}`, `${brief.state} · ${checksText(brief.checks)}`, () => ctx.openReview(session.id), "branch");
    if (brief.checks && brief.checks.failing && brief.state !== "merged" && brief.state !== "closed") {
      cmd(`Ask ${session.name} to fix CI`, `PR #${brief.number}: ${plural(brief.checks.failing, "failing check")}`, () => fix(session.id, null), "send");
    }
  }

  return { rowChip, panel, onActivity, paletteItems };
};
