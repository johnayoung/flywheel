// flywheel hub: the agent launcher and its cost meter. The new-session
// form's agent fields (first prompt, model, permission mode / Codex
// approval and sandbox), each workspace's saved launch templates (pick,
// save, delete; POST/DELETE /api/workspaces/{id}/templates), quick launch
// from a template (palette and the sidebar's "From template"), and token
// usage: a session's tokens and cost in the header and its sidebar row,
// each workspace's today cost, and the hub's today / this week strip
// (GET /api/usage). app.js creates it with its helpers.
"use strict";

window.FwHubLauncher = function (ctx) {
  const { $, el, api, state, toast } = ctx;

  const CLAUDE_MODES = [
    ["", "CLI default"],
    ["manual", "manual: ask before edits and commands"],
    ["acceptEdits", "acceptEdits: edits without asking"],
    ["plan", "plan: read and plan, change nothing"],
    ["auto", "auto: a classifier approves safe actions"],
    ["dontAsk", "dontAsk: deny whatever would ask"],
    ["bypassPermissions", "bypassPermissions: no checks at all"],
  ];
  const CODEX_APPROVALS = [
    ["", "CLI default"],
    ["untrusted", "untrusted: ask for anything not known safe"],
    ["on-request", "on-request: the model decides when to ask"],
    ["on-failure", "on-failure: ask when a sandboxed run fails"],
    ["never", "never: never ask"],
  ];
  const CODEX_SANDBOXES = [
    ["", "CLI default"],
    ["read-only", "read-only"],
    ["workspace-write", "workspace-write"],
    ["danger-full-access", "danger-full-access: no sandbox"],
  ];
  const MODELS = {
    claude: ["opus", "sonnet", "haiku", "fable", "claude-opus-5-5", "claude-sonnet-5-5", "claude-haiku-5-5", "claude-fable-5-1"],
    codex: ["gpt-5-codex", "gpt-5", "gpt-5.1-codex", "gpt-5.1"],
  };

  const box = $("#agent-opts");
  const promptInput = $("#agent-prompt");
  const modelInput = $("#agent-model");
  const models = $("#agent-models");
  const modeSelect = $("#agent-mode");
  const approvalSelect = $("#agent-approval");
  const sandboxSelect = $("#agent-sandbox");
  const warn = $("#agent-warn");
  const tplPick = $("#tpl-pick");
  const tplSelect = $("#tpl-select");
  const tplSave = $("#tpl-save");
  const tplName = $("#tpl-name");
  const tplError = $("#tpl-error");

  const options = (select, list) =>
    select.replaceChildren(...list.map(([value, label]) => el("option", { value, textContent: label })));
  options(modeSelect, CLAUDE_MODES);
  options(approvalSelect, CODEX_APPROVALS);
  options(sandboxSelect, CODEX_SANDBOXES);

  let kind = "shell";
  let workspaceId = null;

  // --- formatting -----------------------------------------------------------

  function tokens(n) {
    n = n || 0;
    if (n < 1000) return String(n);
    if (n < 1e6) return `${(n / 1e3).toFixed(n < 1e4 ? 1 : 0)}k`;
    return `${(n / 1e6).toFixed(2)}M`;
  }

  function dollars(usd) {
    if (!usd) return "$0";
    if (usd < 0.01) return `$${usd.toFixed(4)}`;
    if (usd < 100) return `$${usd.toFixed(2)}`;
    return `$${Math.round(usd)}`;
  }

  // "$0.42", "$0.42+" (some tokens of a model without a price), or the
  // token count alone when nothing is priced -- never a guessed price.
  function costText(cost) {
    if (!cost) return "";
    if (cost.priced) return dollars(cost.cost_usd);
    return cost.cost_usd ? `${dollars(cost.cost_usd)}+` : `${tokens(cost.total_tokens)} tok`;
  }

  function costTitle(label, cost) {
    if (!cost) return "";
    const t = cost.tokens || {};
    const lines = [
      `${label}: ${tokens(cost.total_tokens)} tokens${cost.priced || cost.cost_usd ? `, ${dollars(cost.cost_usd)}` : ""}`,
      `  input ${tokens(t.input)} · output ${tokens(t.output)} · cache read ${tokens(t.cache_read)} · cache write ${tokens((t.cache_write_5m || 0) + (t.cache_write_1h || 0))}`,
    ];
    if (!cost.priced) lines.push(`  no price known for ${(cost.unpriced_models || []).join(", ")} (tokens only)`);
    return lines.join("\n");
  }

  // The tooltip of a session's usage: totals, today, by model, last call.
  function usageTitle(usage) {
    if (!usage) return "";
    const lines = [costTitle(`${usage.turns} API call${usage.turns === 1 ? "" : "s"}`, usage)];
    if (usage.today && usage.today.total_tokens !== usage.total_tokens) lines.push(costTitle("today", usage.today));
    for (const m of usage.models || []) {
      lines.push(`${m.label}: ${tokens(totalOf(m.tokens))} tok${m.cost_usd == null ? " (no price)" : `, ${dollars(m.cost_usd)}`}`);
    }
    if (usage.last_turn) {
      const last = usage.last_turn;
      lines.push(`last call: ${last.model}, ${tokens(totalOf(last.tokens))} tok${last.cost_usd == null ? "" : `, ${dollars(last.cost_usd)}`}`);
    }
    return lines.join("\n");
  }

  const totalOf = (t) => (t ? (t.input || 0) + (t.output || 0) + (t.cache_read || 0) + (t.cache_write_5m || 0) + (t.cache_write_1h || 0) : 0);

  // --- the session header and sidebar ---------------------------------------

  function renderHeader(session) {
    const meter = $("#meter-usage");
    const usage = session && session.usage;
    meter.hidden = !usage;
    if (!usage) return;
    meter.textContent = `${tokens(usage.total_tokens)} tok · ${costText(usage)}`;
    meter.title = usageTitle(usage);
  }

  // The cost chip of a sidebar row (agent sessions with usage).
  function rowChip(session) {
    const usage = session.usage;
    if (!usage) return null;
    return el("span", {
      className: `session-cost${usage.priced ? "" : " partial"}`,
      textContent: costText(usage),
      title: usageTitle(usage),
    });
  }

  // --- rollups ----------------------------------------------------------------

  let rollups = null;
  let rollupTimer = null;
  let rollupBusy = false;

  async function loadRollups() {
    if (rollupBusy) return;
    rollupBusy = true;
    try {
      rollups = await api("GET", "/api/usage");
      renderSpend();
      ctx.onRollups();
    } catch (_) {
      // An older hub, or offline: the strip stays as it was.
    } finally {
      rollupBusy = false;
    }
  }

  function rollupsSoon() {
    clearTimeout(rollupTimer);
    rollupTimer = setTimeout(loadRollups, 800);
  }

  function renderSpend() {
    const strip = $("#spend");
    const hub = rollups;
    const any = hub && hub.total && hub.total.total_tokens > 0;
    strip.hidden = !any;
    if (!any) return;
    strip.replaceChildren(
      el("span", { className: "spend-k", textContent: "today" }),
      el("b", { textContent: costText(hub.today) }),
      el("span", { className: "spend-k", textContent: "week" }),
      el("b", { textContent: costText(hub.week) }),
    );
    strip.title = [
      costTitle(`today (${hub.day})`, hub.today),
      costTitle(`this week (since ${hub.week_start})`, hub.week),
      costTitle("all time", hub.total),
      `prices: ${hub.price_source}`,
    ].join("\n");
  }

  // A workspace's today cost for its sidebar head (null when nothing).
  function workspaceChip(id) {
    const w = rollups && (rollups.workspaces || []).find((x) => x.workspace_id === id);
    if (!w || !w.total || !w.total.total_tokens) return null;
    return el("span", {
      className: "ws-cost",
      textContent: w.today.total_tokens ? costText(w.today) : "",
      title: [costTitle("today", w.today), costTitle("this week", w.week), costTitle("all time", w.total)].join("\n"),
    });
  }

  // --- the form's agent fields -------------------------------------------------

  function setKind(next) {
    kind = next;
    const agent = kind === "claude" || kind === "codex";
    box.hidden = !agent;
    box.dataset.kind = kind;
    models.replaceChildren(...(MODELS[kind] || []).map((m) => el("option", { value: m })));
    modelInput.placeholder = kind === "codex" ? "CLI default (e.g. gpt-5-codex)" : "CLI default (e.g. opus, sonnet)";
    updateWarning();
  }

  function updateWarning() {
    let text = "";
    if (kind === "claude" && modeSelect.value === "bypassPermissions") {
      text = "Bypass permissions: the agent runs every tool -- shell commands, file edits, network -- without asking. Use it only where a mistake costs nothing (a throwaway worktree or container).";
    } else if (kind === "codex" && sandboxSelect.value === "danger-full-access") {
      text = approvalSelect.value === "never"
        ? "No sandbox and no approvals: Codex runs anything, anywhere, without asking."
        : "No sandbox: Codex commands can touch anything your user can.";
    }
    warn.textContent = text;
    warn.hidden = !text;
  }
  modeSelect.addEventListener("change", updateWarning);
  approvalSelect.addEventListener("change", updateWarning);
  sandboxSelect.addEventListener("change", updateWarning);

  // The request fields for the agent options (none for shells).
  function request() {
    if (kind !== "claude" && kind !== "codex") return {};
    const body = {};
    const prompt = promptInput.value;
    if (prompt.trim()) body.prompt = prompt;
    const model = modelInput.value.trim();
    if (model) body.model = model;
    if (kind === "claude" && modeSelect.value) body.permission_mode = modeSelect.value;
    if (kind === "codex") {
      if (approvalSelect.value) body.approval = approvalSelect.value;
      if (sandboxSelect.value) body.sandbox = sandboxSelect.value;
    }
    return body;
  }

  // The form opened (or its workspace changed).
  function reset(id) {
    workspaceId = id;
    promptInput.value = "";
    modelInput.value = "";
    modeSelect.value = "";
    approvalSelect.value = "";
    sandboxSelect.value = "";
    tplSave.hidden = true;
    tplName.value = "";
    tplError.textContent = "";
    renderTemplates();
    updateWarning();
  }

  const templatesOf = (id) => (ctx.workspaceById(id)?.templates || []);

  function renderTemplates(selected) {
    const list = templatesOf(workspaceId);
    tplPick.hidden = !list.length;
    tplSelect.replaceChildren(
      el("option", { value: "", textContent: list.length ? "none: fill in below" : "none saved" }),
      ...list.map((t) => el("option", {
        value: t.name,
        textContent: `${t.name} · ${t.kind}${t.model ? ` ${t.model}` : ""}${t.worktree ? " · worktree" : ""}`,
        selected: t.name === selected,
      })),
    );
    $("#tpl-delete").hidden = !tplSelect.value;
  }

  // Fill the form from template `name`.
  function apply(name) {
    const template = templatesOf(workspaceId).find((t) => t.name === name);
    $("#tpl-delete").hidden = !template;
    if (!template) return;
    ctx.selectKind(template.kind);
    setKind(template.kind);
    promptInput.value = template.prompt || "";
    modelInput.value = template.model || "";
    modeSelect.value = template.permission_mode || "";
    approvalSelect.value = template.approval || "";
    sandboxSelect.value = template.sandbox || "";
    if (template.worktree && !ctx.worktrees.use(true, taskFrom(template.name))) {
      tplError.textContent = "This template runs in a worktree; pick a git folder.";
    } else {
      tplError.textContent = "";
      if (!template.worktree) ctx.worktrees.use(false);
    }
    updateWarning();
  }
  tplSelect.addEventListener("change", () => apply(tplSelect.value));

  // A worktree task for a launch from template `name`: branch-safe, with
  // a short random suffix.
  function taskFrom(name) {
    const slug = name.toLowerCase().replace(/[^a-z0-9]+/g, "-").replace(/^-+|-+$/g, "").slice(0, 40).replace(/-+$/, "");
    const suffix = Math.random().toString(16).slice(2, 6).padEnd(4, "0");
    return `${slug || "agent"}-${suffix}`;
  }

  async function saveTemplate() {
    const name = tplName.value.trim();
    tplError.textContent = "";
    if (!name) {
      tplName.focus();
      tplError.textContent = "Name the template.";
      return;
    }
    const body = { name, kind: ctx.selectedKind(), worktree: $("#wt-on").checked, ...request() };
    try {
      await api("POST", `/api/workspaces/${workspaceId}/templates`, body);
      await ctx.refresh();
      renderTemplates(name);
      tplSave.hidden = true;
      toast(`Saved template ${name}.`, { timeout: 2500 });
    } catch (error) {
      tplError.textContent = error.message;
    }
  }

  async function deleteTemplate() {
    const name = tplSelect.value;
    if (!name) return;
    try {
      await api("DELETE", `/api/workspaces/${workspaceId}/templates/${encodeURIComponent(name)}`);
      await ctx.refresh();
      renderTemplates();
      toast(`Deleted template ${name}.`, { timeout: 2500 });
    } catch (error) {
      tplError.textContent = error.message;
    }
  }

  $("#tpl-save-toggle").addEventListener("click", () => {
    tplSave.hidden = !tplSave.hidden;
    tplError.textContent = "";
    if (!tplSave.hidden) {
      tplName.value = tplSelect.value || "";
      tplName.focus();
    }
  });
  $("#tpl-save-go").addEventListener("click", saveTemplate);
  tplName.addEventListener("keydown", (event) => {
    if (event.key === "Enter") {
      event.preventDefault();
      event.stopPropagation();
      saveTemplate();
    }
  });
  $("#tpl-delete").addEventListener("click", deleteTemplate);

  // --- quick launch -------------------------------------------------------------

  // Launch `template` of `workspace` as is: its folder the last picked
  // (a git one when it runs in a worktree).
  async function quickLaunch(workspace, template) {
    const folders = workspace.folders.filter((f) => !f.missing);
    const last = folders.find((f) => f.path === state.repoChoice[workspace.id]);
    const folder = template.worktree
      ? (last && last.git ? last : folders.find((f) => f.git))
      : (last || folders[0]);
    if (!folder) {
      toast(template.worktree
        ? `${template.name} runs in a worktree, and ${workspace.name} has no git folder.`
        : `${workspace.name} has no folder to open.`, { tone: "bad", timeout: 5000 });
      return;
    }
    const body = { workspace_id: workspace.id, kind: template.kind, repo: folder.path };
    for (const key of ["prompt", "model", "permission_mode", "approval", "sandbox"]) {
      if (template[key]) body[key] = template[key];
    }
    if (template.worktree) body.worktree = { task: taskFrom(template.name) };
    try {
      await ctx.createSession(body, {});
      toast(`Started ${template.name}${template.worktree ? ` in worktree ${body.worktree.task}` : ""}.`, { timeout: 3000 });
    } catch (error) {
      toast(`${template.name}: ${error.message}`, { tone: "bad", timeout: 6000 });
    }
  }

  const PALETTE_PREFIX = "New agent from template";

  function paletteItems(cmd) {
    for (const workspace of state.workspaces) {
      for (const template of workspace.templates || []) {
        const opts = [template.kind, template.model, template.permission_mode, template.worktree ? "worktree" : null].filter(Boolean);
        cmd(`${PALETTE_PREFIX}: ${template.name}`, `${workspace.name} · ${opts.join(" · ")}`, () => quickLaunch(workspace, template), "plus");
      }
    }
  }

  const anyTemplates = () => state.workspaces.some((w) => (w.templates || []).length);

  // With the sidebar: its From template button.
  function render() {
    $("#from-template").hidden = !anyTemplates();
  }

  // An events-socket usage push: the session's usage changed.
  function onUsage(message) {
    const session = ctx.sessionById(message.session_id);
    if (session) session.usage = message.usage;
    ctx.onSessionUsage(message.session_id);
    rollupsSoon();
  }

  return {
    setKind, reset, request, renderHeader, rowChip, workspaceChip, paletteItems, render, onUsage,
    loadRollups, rollupsSoon, PALETTE_PREFIX, tokens, costText, taskFrom,
  };
};
