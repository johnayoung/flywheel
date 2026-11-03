// flywheel hub: the agent launcher and its cost meter. The new-session
// form's agent fields (first prompt, model, permission mode / Codex
// approval and sandbox), each workspace's saved launch templates (pick,
// save, delete; POST/DELETE /api/workspaces/{id}/templates), quick launch
// from a template (palette and the sidebar's "From template"), and token
// usage: a session's tokens and cost in the header and its sidebar row,
// each workspace's today cost, and the hub's today / this week strip
// (GET /api/usage). app-base.js creates it with its helpers.
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
  const tplNote = $("#tpl-note");
  const tplDelete = $("#tpl-delete");
  const tplSaveGo = $("#tpl-save-go");

  const options = (select, list) =>
    select.replaceChildren(...list.map(([value, label]) => el("option", { value, textContent: label })));
  options(modeSelect, CLAUDE_MODES);
  options(approvalSelect, CODEX_APPROVALS);
  options(sandboxSelect, CODEX_SANDBOXES);

  let kind = "shell";
  let workspaceId = null;

  // --- formatting -----------------------------------------------------------

  const { tokens, dollars } = window.FwHubFmt;

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
    lines.push("  at API list prices (a subscription plan bills differently)");
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

  // A model id as the header shows it ("claude-sonnet-5-5" -> "sonnet-5-5").
  const shortModel = (id) => String(id || "").replace(/^claude-/, "").replace(/-\d{8}$/, "");

  // "30k tok · $0.08 · sonnet-5-5": the model the transcript says was
  // billed (which can differ from the one asked for), or "+N" for more.
  function renderHeader(session) {
    const meter = $("#meter-usage");
    const usage = session && session.usage;
    meter.hidden = !usage;
    if (!usage) return;
    const models = [...new Set((usage.models || []).map((m) => m.model).filter(Boolean))];
    const primary = (usage.last_turn && usage.last_turn.model) || models[0];
    const model = primary ? `${shortModel(primary)}${models.length > 1 ? ` +${models.length - 1}` : ""}` : "";
    meter.replaceChildren(
      el("span", { className: "usage-tok", textContent: `${tokens(usage.total_tokens)} tok · ` }),
      el("span", { textContent: costText(usage) }),
      model ? el("span", { className: "usage-model", textContent: ` · ${model}` }) : "",
    );
    meter.title = usageTitle(usage);
    meter.setAttribute("aria-label", `usage: ${meter.textContent}. Show the breakdown`);
  }

  // Touch screens have no tooltips: a tap shows the breakdown.
  $("#meter-usage").addEventListener("click", () => {
    const session = ctx.sessionById(state.attached);
    if (!session || !session.usage) return;
    toast(el("div", { className: "toast-pre", textContent: usageTitle(session.usage) }), { timeout: 9000 });
  });

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
      const [local, remote] = await Promise.all([
        api("GET", "/api/usage"),
        ctx.remoteUsage ? ctx.remoteUsage() : [],
      ]);
      // This hub's spend strip; every machine's workspace chips.
      rollups = { ...local, workspaces: [...(local.workspaces || []), ...remote] };
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
    // With machines, the strip is this hub's spend only (each machine's
    // workspaces carry their own chips): it says so.
    const here = ctx.localLabel ? ctx.localLabel() : "";
    strip.replaceChildren(
      here ? el("span", { className: "spend-k spend-here", textContent: here }) : "",
      el("span", { className: "spend-k", textContent: "today" }),
      el("b", { textContent: costText(hub.today) }),
      el("span", { className: "spend-k", textContent: "week" }),
      el("b", { textContent: costText(hub.week) }),
    );
    strip.title = [
      here ? `${here} only: other machines' spend is on their workspaces` : "",
      costTitle(`today (${hub.day})`, hub.today),
      costTitle(`this week (since ${hub.week_start})`, hub.week),
      costTitle("all time", hub.total),
      `prices: ${hub.price_source}`,
    ].filter(Boolean).join("\n");
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
    // A template starts an agent: a shell has nothing to save.
    $("#tpl-save-toggle").hidden = !agent;
    if (!agent) tplSave.hidden = true;
    models.replaceChildren(...(MODELS[kind] || []).map((m) => el("option", { value: m })));
    modelInput.placeholder = kind === "codex" ? "default: gpt-5-codex..." : "default: opus, sonnet...";
    prefill();
    updateWarning();
  }

  // Settings' agent defaults for this kind go into fields the user has
  // not touched since they were last filled.
  let filled = null;
  const fields = () => ({
    model: modelInput.value,
    permission_mode: modeSelect.value,
    approval: approvalSelect.value,
    sandbox: sandboxSelect.value,
  });
  function prefill() {
    if (!ctx.agentDefaults || (kind !== "claude" && kind !== "codex")) return;
    const now = fields();
    const blank = { model: "", permission_mode: "", approval: "", sandbox: "" };
    const untouched = JSON.stringify(now) === JSON.stringify(filled || blank);
    if (!untouched) return;
    const d = ctx.agentDefaults(kind);
    modelInput.value = d.model;
    if ([...modeSelect.options].some((o) => o.value === d.permission_mode)) modeSelect.value = d.permission_mode;
    if ([...approvalSelect.options].some((o) => o.value === d.approval)) approvalSelect.value = d.approval;
    if ([...sandboxSelect.options].some((o) => o.value === d.sandbox)) sandboxSelect.value = d.sandbox;
    filled = fields();
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
    const shown = !warn.hidden;
    warn.textContent = text;
    warn.hidden = !text;
    // Below the fold of a tall form: bring it into view as it appears.
    if (text && !shown) warn.scrollIntoView({ block: "nearest" });
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
    filled = null;
    prefill();
    tplSave.hidden = true;
    tplName.value = "";
    tplError.textContent = "";
    note("");
    renderTemplates();
    updateWarning();
  }

  // What a save or delete did, said inside the dialog (a toast sits
  // under the modal's backdrop).
  function note(text) {
    tplNote.textContent = text;
  }

  // A click that needs a second one: the first arms `button` (its text
  // `armedText`) for a few seconds; returns whether this click confirms.
  function armed(button, armedText) {
    if (button.dataset.armed === "1") {
      disarm(button);
      return true;
    }
    button.dataset.label = button.dataset.label || button.textContent;
    button.dataset.armed = "1";
    button.textContent = armedText;
    button.classList.add("armed");
    clearTimeout(button._disarm);
    button._disarm = setTimeout(() => disarm(button), 5000);
    return false;
  }

  function disarm(button) {
    clearTimeout(button._disarm);
    if (button.dataset.armed !== "1") return;
    button.dataset.armed = "";
    button.textContent = button.dataset.label;
    button.classList.remove("armed");
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
    disarm(tplDelete);
    tplDelete.hidden = !tplSelect.value;
  }

  // Fill the form from template `name`.
  function apply(name) {
    const template = templatesOf(workspaceId).find((t) => t.name === name);
    disarm(tplDelete);
    tplDelete.hidden = !template;
    note("");
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
    const kindNow = ctx.selectedKind();
    if (kindNow !== "claude" && kindNow !== "codex") {
      tplError.textContent = "A template starts an agent: pick Claude or Codex.";
      return;
    }
    // Saving over one of the same name replaces all of it: say so first.
    const exists = templatesOf(workspaceId).some((t) => t.name === name);
    if (exists && !armed(tplSaveGo, `Replace ${name}`)) {
      note(`A template named ${name} exists. Click Replace to overwrite every option it has with this form's.`);
      return;
    }
    const body = { name, kind: kindNow, worktree: $("#wt-on").checked, replace: exists, ...request() };
    try {
      await api("POST", `/api/workspaces/${workspaceId}/templates`, body);
      await ctx.refresh();
      renderTemplates(name);
      tplSave.hidden = true;
      note(`${exists ? "Replaced" : "Saved"} template ${name}.`);
    } catch (error) {
      tplError.textContent = error.message;
    }
  }

  async function deleteTemplate() {
    const name = tplSelect.value;
    if (!name) return;
    if (!armed(tplDelete, `delete ${name}? click again`)) return;
    try {
      await api("DELETE", `/api/workspaces/${workspaceId}/templates/${encodeURIComponent(name)}`);
      await ctx.refresh();
      renderTemplates();
      note(`Deleted template ${name}.`);
    } catch (error) {
      tplError.textContent = error.message;
    }
  }

  $("#tpl-save-toggle").addEventListener("click", () => {
    tplSave.hidden = !tplSave.hidden;
    tplError.textContent = "";
    note("");
    disarm(tplSaveGo);
    if (!tplSave.hidden) {
      tplName.value = tplSelect.value || "";
      tplName.focus();
    }
  });
  $("#tpl-save-go").addEventListener("click", saveTemplate);
  tplName.addEventListener("input", () => disarm(tplSaveGo));
  tplName.addEventListener("keydown", (event) => {
    if (event.key === "Enter") {
      event.preventDefault();
      event.stopPropagation();
      saveTemplate();
    }
  });
  tplDelete.addEventListener("click", deleteTemplate);

  // --- quick launch -------------------------------------------------------------

  // Launch `template` of `workspace` as is: its folder the last picked
  // (a git one when it runs in a worktree).
  // Whether `template` starts its agent with the safety checks off.
  const unchecked = (t) => t.unchecked === true || t.permission_mode === "bypassPermissions" ||
    (t.approval === "never" && t.sandbox === "danger-full-access");

  async function quickLaunch(workspace, template) {
    if (unchecked(template)) {
      const ok = await ctx.confirmDialog({
        kicker: "no permission checks",
        title: `Launch ${template.name} unchecked?`,
        body: template.kind === "codex"
          ? "This template runs Codex with no sandbox and no approvals: it runs anything, anywhere, without asking."
          : "This template runs Claude with bypassPermissions: every tool -- shell commands, file edits, network -- runs without asking.",
        ok: "Launch unchecked",
      });
      if (!ok) return;
    }
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

  // Every template, with its workspace and a short line of its options
  // (an unchecked one says so, first).
  function eachTemplate(each) {
    for (const workspace of state.workspaces) {
      for (const template of workspace.templates || []) {
        if (template.kind !== "claude" && template.kind !== "codex") continue;
        const opts = [template.kind, template.model, template.worktree ? "worktree" : null].filter(Boolean);
        const hint = `${unchecked(template) ? "NO CHECKS · " : ""}${workspace.name} · ${opts.join(" · ")}`;
        each(workspace, template, hint);
      }
    }
  }

  function paletteItems(cmd) {
    eachTemplate((workspace, template, hint) => {
      cmd(`${PALETTE_PREFIX}: ${template.name}`, hint, () => quickLaunch(workspace, template), "plus", { warn: unchecked(template) });
    });
  }

  // The quick-launch list ("From template"): items named by the template
  // alone (a phone has no room for a prefix); unchecked ones last, so the
  // highlighted first row is never one.
  function templateItems() {
    const items = [];
    eachTemplate((workspace, template, hint) => {
      items.push({
        group: "Templates", glyph: "plus", label: template.name, hint,
        warn: unchecked(template), run: () => quickLaunch(workspace, template),
      });
    });
    return items.sort((a, b) => Number(a.warn) - Number(b.warn));
  }

  const anyTemplates = () => state.workspaces.some((w) => (w.templates || []).length);

  // With the sidebar: its From template button.
  function render() {
    $("#from-template").hidden = !anyTemplates();
    // Machines came or went: the strip's label follows.
    if (rollups) renderSpend();
  }

  // An events-socket usage push: the session's usage changed.
  function onUsage(message) {
    const session = ctx.sessionById(message.session_id);
    if (session) session.usage = message.usage;
    ctx.onSessionUsage(message.session_id);
    rollupsSoon();
  }

  return {
    setKind, reset, request, renderHeader, rowChip, workspaceChip, paletteItems, templateItems, render, onUsage,
    loadRollups, rollupsSoon, PALETTE_PREFIX, tokens, costText, taskFrom,
  };
};
