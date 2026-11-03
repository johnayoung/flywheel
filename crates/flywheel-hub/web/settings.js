// flywheel hub: Settings (palette "Settings", the sidebar's gear, Alt ,):
// preferences the hub keeps for you (GET/PUT /api/settings, so they follow
// you to every device) with per-device overrides (this browser's local
// storage), every keyboard shortcut rebindable with conflict checks (the
// server's: POST /api/settings/keys/check), plugins (enable, run, logs;
// never installed from here), and a read-only view of the hub's own
// settings file (GET /api/config: hub.toml with each value's source).
//
// The server owns the schema: each preference's type and default, and
// every shortcut with its default, come from GET /api/settings (cached
// here for the next page load). The keymap matching (match / matchPrefix)
// is what app-keyboard.js dispatches on.
"use strict";

window.FwHubSettings = function (ctx) {
  const { $, el, icon, store, rawApi, IS_MAC } = ctx;
  const toast = (...args) => ctx.toast(...args);

  const SERVER_KEY = "prefs.server"; // the last GET /api/settings
  const DEVICE_KEY = "prefs.device"; // id -> value (this device only)
  // Per hub (`@<hub id>`): another hub served later on this origin does
  // not inherit them.
  const DEVICE_KEYS = "keys.device"; // action -> [bindings] (this device only)
  const KEYS_SCOPE = "keys.scope"; // "user" | "device": where key edits go

  let server = store.get(SERVER_KEY, null) || { rev: 0, values: {}, keys: {}, keymap: {}, prefs: [], actions: [], rules: {} };
  let device = store.get(DEVICE_KEY, {}) || {};
  const perHub = (name) => `${name}@${server.hub_id || "?"}`;
  let deviceKeys = (server.hub_id && store.get(perHub(DEVICE_KEYS), {})) || {};
  let config = null; // last GET /api/config
  let plugins = null; // last GET /api/plugins
  let configError = ""; // the hub.toml error last toasted
  const listeners = new Set();
  let index = { global: new Map(), prefix: new Map() };

  // Per-browser choices made before Settings existed become this
  // device's overrides (nothing changes for them).
  if (!store.get("prefs.migrated", false)) {
    const legacy = [
      ["theme", "appearance.theme"],
      ["prefix", "keyboard.pane_prefix"],
      ["companion", "keyboard.companion"],
      ["font-size", "appearance.font_size"],
      ["keybar", "keyboard.key_bar"],
      ["notify", "notifications.alerts"],
    ];
    for (const [old, id] of legacy) {
      const value = store.get(old, undefined);
      if (value !== undefined && value !== null && !(id in device)) device[id] = value;
    }
    store.set(DEVICE_KEY, device);
    store.set("prefs.migrated", true);
  }

  const spec = (id) => (server.prefs || []).find((p) => p.id === id);
  // What hub.toml (or a flag) sets for `id` ([agents.*] share the
  // preferences' ids): what a launch naming none gets. null: unset.
  function hubDefault(id) {
    const entry = config && (config.values || []).find((v) => v.key === id);
    return entry && entry.value !== null && entry.value !== "" ? entry.value : null;
  }
  const action = (id) => (server.actions || []).find((a) => a.id === id);

  // The value in force: this device's, else the hub's, else the default.
  function get(id, fallback) {
    if (Object.prototype.hasOwnProperty.call(device, id)) return device[id];
    if (server.values && Object.prototype.hasOwnProperty.call(server.values, id)) return server.values[id];
    const s = spec(id);
    if (s && s.default !== null && s.default !== undefined) return s.default;
    return fallback;
  }
  const isDevice = (id) => Object.prototype.hasOwnProperty.call(device, id);

  // fn(ids): the preferences whose value in force may have changed.
  const onChange = (fn) => listeners.add(fn);
  function changed(ids) {
    for (const fn of listeners) {
      try {
        fn(ids);
      } catch (error) {
        console.error(error);
      }
    }
  }

  // --- the keymap ----------------------------------------------------------

  // Every action's bindings in force (device ones over the hub's).
  function keymap() {
    const map = { ...(server.keymap || {}) };
    for (const [id, keys] of Object.entries(deviceKeys)) if (action(id)) map[id] = keys;
    return map;
  }

  // Key hints in the page (index.html: data-key="<action>" shows the
  // action's first binding, hidden when unbound; data-key-title="<action>"
  // with data-title: the title plus the key).
  function renderHints() {
    for (const node of document.querySelectorAll("[data-key]")) {
      const text = keyOf(node.dataset.key);
      node.textContent = text;
      node.hidden = !text;
    }
    for (const row of document.querySelectorAll("[data-key-row]")) {
      row.hidden = ![...row.querySelectorAll("[data-key]")].some((k) => !k.hidden);
    }
    for (const node of document.querySelectorAll("[data-key-title]")) {
      const text = keyOf(node.dataset.keyTitle);
      const base = node.dataset.title || "";
      node.title = text ? `${base} (${text})` : base;
    }
    renderBadge();
  }

  function rebuild() {
    index = { global: new Map(), prefix: new Map() };
    for (const [id, keys] of Object.entries(keymap())) {
      const a = action(id);
      if (!a) continue;
      const table = index[a.scope] || index.global;
      for (const key of keys) {
        const range = /^(.*?)1\.\.9$/.exec(key);
        if (range) {
          for (let n = 1; n <= 9; n++) table.set(`${range[1]}${n}`, { id, index: n - 1 });
        } else {
          table.set(key, { id, index: 0 });
        }
      }
    }
    renderHints();
  }

  // The key name a binding uses for this physical key ("" for none).
  const NAMED = {
    ArrowLeft: "left", ArrowRight: "right", ArrowUp: "up", ArrowDown: "down",
    PageUp: "pageup", PageDown: "pagedown", Home: "home", End: "end", Insert: "insert",
    Delete: "delete", Backspace: "backspace", Tab: "tab", Enter: "enter", Escape: "escape", Space: "space",
    Backquote: "`", Minus: "-", Equal: "=", BracketLeft: "[", BracketRight: "]", Backslash: "\\",
    Semicolon: ";", Quote: "'", Comma: ",", Period: ".", Slash: "/",
  };
  function keyName(code) {
    let m = /^Key([A-Z])$/.exec(code);
    if (m) return m[1].toLowerCase();
    m = /^Digit([0-9])$/.exec(code);
    if (m) return m[1];
    m = /^F([0-9]{1,2})$/.exec(code);
    if (m) return `f${m[1]}`;
    return NAMED[code] || "";
  }
  const MODIFIER_KEYS = ["Shift", "Control", "Alt", "Meta", "CapsLock", "AltGraph", "OS"];

  // Whether Alt+key reports the key itself (Alt+7 -> "7"), not a
  // character the layout made of it (then it is the terminal's).
  function typesItself(event, name) {
    if (IS_MAC) return true;
    if (/^[a-z0-9]$/.test(name)) return event.key.length === 1 && event.key.toLowerCase() === name;
    if (name.length === 1 && !event.shiftKey) return event.key === name;
    return true;
  }

  // The bindings this event could be, most specific first. macOS: the
  // hub chord `alt` is Ctrl+Option (Option alone types characters).
  function candidates(event) {
    const name = keyName(event.code);
    if (!name) return [];
    const shift = event.shiftKey ? "shift+" : "";
    const out = [];
    const chordAlt = IS_MAC ? event.ctrlKey && event.altKey : event.altKey && !event.ctrlKey;
    if (event.metaKey && !IS_MAC) return [`meta+${shift}${name}`];
    if (chordAlt) {
      if (event.metaKey) return [];
      if (!typesItself(event, name)) return [];
      out.push(`alt+${shift}${name}`);
      return out;
    }
    if (IS_MAC && event.altKey) return []; // Option alone: a character
    const ctrl = event.ctrlKey;
    const cmd = IS_MAC && event.metaKey;
    if (ctrl && !cmd) out.push(`ctrl+${shift}${name}`);
    if (ctrl || cmd) out.push(`mod+${shift}${name}`);
    if (!ctrl && !cmd) out.push(`${shift}${name}`);
    return out;
  }

  // The global action this key event is bound to: {id, index} or null.
  function match(event) {
    for (const key of candidates(event)) {
      const hit = index.global.get(key);
      if (hit) return hit;
    }
    return null;
  }

  // The pane-prefix action for the key typed after the prefix.
  function matchPrefix(event) {
    const name = keyName(event.code);
    const tries = [];
    if (event.ctrlKey && !event.altKey && !event.metaKey && name) tries.push(`ctrl+${name}`);
    if (!event.ctrlKey && !event.metaKey) {
      if (event.key.length === 1) tries.push(event.key);
      if (name && name.length > 1) tries.push(name);
    }
    for (const key of tries) {
      const hit = index.prefix.get(key);
      if (hit) return hit;
    }
    return null;
  }

  // The canonical binding a key event captures as, or {error}.
  function capture(event, scope) {
    if (MODIFIER_KEYS.includes(event.key)) return null;
    const name = keyName(event.code);
    if (scope === "prefix") {
      if (event.ctrlKey && name) return { key: `ctrl+${name}` };
      if (event.key.length === 1 && event.key !== " ") return { key: event.key };
      if (name) return { key: name };
      return { error: "That key has no name the hub knows." };
    }
    if (!name) return { error: "That key has no name the hub knows (letters, digits, F-keys, arrows, punctuation)." };
    const mods = [];
    if (IS_MAC) {
      if (event.metaKey) mods.push("mod");
      if (event.ctrlKey && event.altKey) mods.push("alt");
      else if (event.ctrlKey) mods.push("ctrl");
      else if (event.altKey) return { error: "Option alone types characters on macOS: use Ctrl Option (the hub chord)." };
    } else {
      if (event.ctrlKey) mods.push("ctrl");
      if (event.altKey) mods.push("alt");
    }
    if (event.shiftKey) mods.push("shift");
    if (event.metaKey && !IS_MAC) mods.push("meta");
    return { key: [...mods, name].join("+") };
  }

  // "alt+shift+n" as the UI writes it ("Alt Shift N"; macOS "Ctrl Opt Shift N").
  const KEY_LABEL = {
    left: "←", right: "→", up: "↑", down: "↓", pageup: "PgUp", pagedown: "PgDn",
    home: "Home", end: "End", insert: "Ins", delete: "Del", backspace: "Bksp", tab: "Tab",
    enter: "Enter", escape: "Esc", space: "Space",
  };
  function label(binding) {
    if (!binding) return "";
    if (binding.length === 1) return binding;
    const parts = binding === "ctrl++" ? ["ctrl", "+"] : binding.split("+");
    return parts
      .map((p, i) => {
        if (i < parts.length - 1) {
          if (p === "mod") return IS_MAC ? "Cmd" : "Ctrl";
          if (p === "alt") return IS_MAC ? "Ctrl Opt" : "Alt";
          if (p === "ctrl") return "Ctrl";
          if (p === "shift") return "Shift";
          if (p === "meta") return IS_MAC ? "Cmd" : "Super";
          return p;
        }
        if (p === "1..9") return "1-9";
        if (KEY_LABEL[p]) return KEY_LABEL[p];
        return /^f[0-9]+$/.test(p) ? p.toUpperCase() : p.length === 1 ? p.toUpperCase() : p;
      })
      .join(" ");
  }
  // An action's first binding, labelled ("" when unbound).
  const keyOf = (id) => {
    const keys = keymap()[id] || [];
    return keys.length ? label(keys[0]) : "";
  };

  // --- loading and saving ----------------------------------------------------

  function adopt(view) {
    const before = { values: server.values || {}, keymap: JSON.stringify(keymap()) };
    const hubChanged = (view.hub_id || "") !== (server.hub_id || "");
    server = {
      hub_id: view.hub_id || "",
      rev: view.rev,
      values: view.values || {},
      keys: view.keys || {},
      keymap: view.keymap || {},
      prefs: view.prefs || [],
      actions: view.actions || [],
      rules: view.rules || {},
      load_error: view.load_error || null,
      file: view.file || "",
    };
    store.set(SERVER_KEY, server);
    if (hubChanged) deviceKeys = store.get(perHub(DEVICE_KEYS), {}) || {};
    rebuild();
    const ids = new Set([...Object.keys(before.values), ...Object.keys(server.values)]);
    for (const p of server.prefs) ids.add(p.id);
    changed([...ids]);
    if (before.keymap !== JSON.stringify(keymap())) changed(["keys"]);
  }

  async function load() {
    try {
      adopt(await rawApi("GET", "/api/settings"));
    } catch (error) {
      if (!server.prefs.length) console.error("settings:", error);
    }
  }

  // Save a patch on the hub; a stale write is re-applied once on the
  // fresh settings. Returns the answer, or throws (422: .errors).
  async function save(patch) {
    for (let attempt = 0; attempt < 2; attempt++) {
      try {
        const saved = await rawApi("PUT", "/api/settings", { rev: server.rev, ...patch });
        adopt(saved);
        return saved;
      } catch (error) {
        if (error.status === 409 && attempt === 0) {
          await load();
          continue;
        }
        throw error;
      }
    }
    return null;
  }

  // Set a preference: this device only (`device`, or device-only ones),
  // else on the hub for every device.
  async function set(id, value, opts = {}) {
    const s = spec(id);
    if (opts.device || (s && s.device_only) || isDevice(id)) {
      if (value === null || value === undefined) delete device[id];
      else device[id] = value;
      store.set(DEVICE_KEY, device);
      changed([id]);
      return true;
    }
    try {
      await save({ values: { [id]: value === undefined ? null : value } });
      return true;
    } catch (error) {
      toast(`Not saved: ${error.message}`, { tone: "bad" });
      return false;
    }
  }

  // Move a preference to this device (keeping its value) or back to
  // the hub's.
  function setDeviceScope(id, on) {
    if (on) device[id] = get(id);
    else delete device[id];
    store.set(DEVICE_KEY, device);
    changed([id]);
  }

  const keysScope = () => (store.get(perHub(KEYS_SCOPE), "user") === "device" ? "device" : "user");

  // Ask the hub whether a keymap (overrides) is valid.
  async function checkKeys(overrides) {
    return rawApi("POST", "/api/settings/keys/check", { keys: overrides });
  }

  // Overrides as they would be with `id` set to `keys` (null: default),
  // where key edits go now.
  function proposed(id, keys) {
    const user = { ...(server.keys || {}) };
    if (keysScope() === "device") {
      const merged = { ...user, ...deviceKeys };
      if (keys === null) delete merged[id];
      else merged[id] = keys;
      return merged;
    }
    if (keys === null) delete user[id];
    else user[id] = keys;
    // Device keys still override on this device: check what is in force.
    return { ...user, ...deviceKeys };
  }

  // Bind `id` to `keys` (null: its default). Returns {ok, errors, warnings}.
  async function bind(id, keys) {
    const overrides = proposed(id, keys);
    let check;
    try {
      check = await checkKeys(overrides);
    } catch (error) {
      return { ok: false, errors: [{ action: id, key: "", message: error.message }], warnings: [] };
    }
    if (check.errors.length) return { ok: false, errors: check.errors, warnings: check.warnings.filter((w) => w.action === id) };
    if (keysScope() === "device") {
      if (keys === null) delete deviceKeys[id];
      else deviceKeys[id] = check.keys[id] || [];
      store.set(perHub(DEVICE_KEYS), deviceKeys);
      rebuild();
      changed(["keys"]);
      return { ok: true, errors: [], warnings: check.warnings.filter((w) => w.action === id) };
    }
    try {
      const saved = await save({ keys: { [id]: keys } });
      return { ok: true, errors: [], warnings: ((saved && saved.warnings) || []).filter((w) => w.action === id) };
    } catch (error) {
      return { ok: false, errors: error.errors || [{ action: id, key: "", message: error.message }], warnings: [] };
    }
  }

  // --- server state: hub.toml and plugins --------------------------------------

  async function loadConfig() {
    try {
      config = await rawApi("GET", "/api/config");
    } catch (_) {
      return;
    }
    const error = (config.file && config.file.error) || "";
    if (error && error !== configError) {
      toast(`hub.toml is invalid (the last good settings stay): ${error}`, { tone: "bad", timeout: 12000 });
    } else if (!error && configError) {
      toast("hub.toml fixed and reloaded.");
    }
    configError = error;
    renderBadge();
    if (dialog.open && (view.section === "server" || view.section === "agents")) renderSection();
  }

  async function loadPlugins() {
    try {
      plugins = await rawApi("GET", "/api/plugins");
    } catch (_) {
      plugins = null;
    }
    if (dialog.open && view.section === "plugins") renderSection();
  }

  function renderBadge() {
    const gear = $("#settings-gear");
    if (!gear) return;
    gear.classList.toggle("warn", !!configError);
    gear.title = configError ? `Settings: hub.toml is invalid -- ${configError}` : `Settings (${keyOf("settings.open") || "palette"})`;
  }

  // An events push.
  function onPush(message) {
    if (message.type === "settings" && message.rev !== server.rev) load();
    else if (message.type === "config") loadConfig();
    else if (message.type === "plugins") loadPlugins();
    else if (message.type === "plugin_toast") {
      const tone = message.tone === "error" || message.tone === "warn" ? "bad" : "";
      toast(el("span", {}, el("b", { textContent: message.plugin }), `: ${message.message}`), { tone, timeout: 6000 });
    }
  }

  async function runPlugin(plugin, a) {
    const session = ctx.attachedLocal();
    if (a.context === "session" && !session) {
      toast(`${a.title}: attach a session on this hub first.`, { tone: "bad" });
      return;
    }
    try {
      const result = await rawApi("POST", `/api/plugins/${encodeURIComponent(plugin)}/actions/${encodeURIComponent(a.id)}`,
        session ? { session_id: session } : {});
      if (result.toast) {
        const tone = result.toast.tone === "error" || result.toast.tone === "warn" ? "bad" : "";
        toast(el("span", {}, el("b", { textContent: plugin }), `: ${result.toast.message}`), { tone, timeout: 6000 });
      }
      if (!result.ok) toast(`${plugin} ${a.id}: ${result.log.error || "failed"}`, { tone: "bad", timeout: 8000 });
      else if (!result.toast && result.paste) toast(`${plugin}: pasted into the session.`);
      else if (!result.toast) toast(`${plugin} ${a.id}: done.`);
    } catch (error) {
      toast(`${plugin} ${a.id}: ${error.message}`, { tone: "bad" });
    }
    if (dialog.open && view.section === "plugins") loadPlugins();
  }

  // --- the dialog -------------------------------------------------------------

  const SECTIONS = [
    ["appearance", "Appearance"],
    ["terminal", "Terminal"],
    ["keyboard", "Keyboard"],
    ["notifications", "Notifications"],
    ["agents", "Agents"],
    ["plugins", "Plugins"],
    ["server", "Server"],
  ];
  const dialog = $("#dlg-settings");
  const nav = $("#settings-nav");
  const body = $("#settings-body");
  const view = { section: "appearance", capturing: null };

  function renderNav() {
    nav.replaceChildren(
      ...SECTIONS.map(([id, name]) =>
        el("button", {
          type: "button",
          role: "tab",
          className: "settings-tab",
          "aria-selected": String(view.section === id),
          dataset: { section: id },
          textContent: name,
          onclick: (e) => {
            view.section = id;
            view.capturing = null;
            renderNav();
            renderSection();
            const tab = nav.querySelector(`[data-section="${e.currentTarget.dataset.section}"]`);
            if (tab) tab.scrollIntoView({ block: "nearest", inline: "nearest" });
          },
        }),
      ),
    );
  }

  const CHOICE_LABEL = {
    "": "CLI default", jetbrains: "JetBrains Mono", martian: "Martian Mono", system: "system monospace",
  };

  function control(s) {
    const value = get(s.id);
    const fromHub = s.section === "agents" ? hubDefault(s.id) : null;
    const commit = (next) => set(s.id, next).then(() => renderSection());
    if (s.type === "choice") {
      return el(
        "select",
        { id: `pref-${s.id}`, onchange: (e) => commit(e.target.value) },
        s.choices.map((c) => el("option", {
          value: c,
          textContent: c === "" && fromHub !== null ? `hub default (${fromHub}, hub.toml)` : CHOICE_LABEL[c] || c,
          selected: c === value,
        })),
      );
    }
    if (s.type === "bool") {
      return el("input", {
        type: "checkbox",
        id: `pref-${s.id}`,
        checked: value === true,
        onchange: (e) => commit(e.target.checked),
      });
    }
    if (s.type === "int") {
      return el("input", {
        type: "number",
        id: `pref-${s.id}`,
        min: s.min,
        max: s.max,
        step: 1,
        value: value === null || value === undefined ? "" : String(value),
        placeholder: s.default === null ? "auto" : String(s.default),
        onchange: (e) => {
          const raw = e.target.value.trim();
          if (!raw) return commit(null);
          const n = Number(raw);
          if (!Number.isInteger(n) || n < s.min || n > s.max) {
            toast(`${s.label}: a whole number ${s.min}-${s.max}.`, { tone: "bad" });
            renderSection();
            return null;
          }
          return commit(n);
        },
      });
    }
    return el("input", {
      type: "text",
      id: `pref-${s.id}`,
      value: value || "",
      spellcheck: false,
      autocomplete: "off",
      placeholder: fromHub !== null ? `hub default: ${fromHub} (hub.toml)` : "CLI default",
      onchange: (e) => commit(e.target.value.trim() || null),
    });
  }

  function prefRow(s) {
    const local = s.device_only || isDevice(s.id);
    const set_ = Object.prototype.hasOwnProperty.call(server.values || {}, s.id) || isDevice(s.id);
    return el(
      "div",
      { className: "pref-row", dataset: { pref: s.id } },
      el(
        "label",
        { className: "pref-label", htmlFor: `pref-${s.id}` },
        el("span", { textContent: s.label }),
        s.help ? el("span", { className: "field-hint", textContent: s.help }) : null,
      ),
      el("div", { className: "pref-control" }, control(s)),
      el(
        "div",
        { className: "pref-scope" },
        s.device_only
          ? el("span", { className: "pref-tag", title: "Kept on this device only", textContent: "this device" })
          : el(
            "label",
            { className: "pref-device", title: "Keep this setting on this device only (others keep the hub's)" },
            el("input", {
              type: "checkbox",
              checked: local,
              onchange: (e) => {
                setDeviceScope(s.id, e.target.checked);
                renderSection();
              },
            }),
            " this device",
          ),
        set_
          ? el("button", {
            type: "button",
            className: "text-btn",
            title: "Back to the default",
            textContent: "reset",
            onclick: async () => {
              if (isDevice(s.id)) setDeviceScope(s.id, false);
              if (Object.prototype.hasOwnProperty.call(server.values || {}, s.id)) await set(s.id, null);
              renderSection();
            },
          })
          : null,
      ),
    );
  }

  function prefsSection(section) {
    const rows = (server.prefs || []).filter((p) => p.section === section).map(prefRow);
    const note = {
      appearance: "Saved on the hub for all your devices; tick \"this device\" to keep a choice here only.",
      terminal: "Applies to every pane at once.",
      notifications: "Push notifications to a phone with every browser closed are the hub's: notify.toml (see Server).",
      agents: "Pre-filled in the new-session form. Launches that name none also get hub.toml's [agents.*] defaults.",
    }[section];
    return [note ? el("p", { className: "sheet-body settings-note", textContent: note }) : null, ...rows];
  }

  function keysSection() {
    const map = keymap();
    const actions = server.actions || [];
    const scope = keysScope();
    const head = el(
      "div",
      { className: "keys-scope" },
      el("span", { className: "field-label", textContent: "Key changes apply to" }),
      ...[["user", "all my devices"], ["device", "this device only"]].map(([value, text]) =>
        el(
          "label",
          { className: "pref-device" },
          el("input", {
            type: "radio",
            name: "keys-scope",
            value,
            checked: scope === value,
            onchange: () => {
              store.set(perHub(KEYS_SCOPE), value);
              renderSection();
            },
          }),
          ` ${text}`,
        ),
      ),
      Object.keys(deviceKeys).length
        ? el("button", {
          type: "button",
          className: "text-btn",
          textContent: `drop this device's ${Object.keys(deviceKeys).length} override${Object.keys(deviceKeys).length === 1 ? "" : "s"}`,
          onclick: () => {
            deviceKeys = {};
            store.set(perHub(DEVICE_KEYS), deviceKeys);
            rebuild();
            changed(["keys"]);
            renderSection();
          },
        })
        : null,
    );
    const nodes = [
      el("p", {
        className: "sheet-body settings-note",
        textContent: `Click a key to change it, then press the new one (Esc cancels). Keys the browser or OS keep (Ctrl W, Ctrl T, ...) and keys a terminal needs (Ctrl C, plain letters) are refused; a key bound twice is a conflict. ${IS_MAC ? "Ctrl Opt is the hub chord here (Option alone types characters)." : ""}`,
      }),
      head,
    ];
    let group = null;
    let list = null;
    for (const a of actions) {
      if (a.group !== group) {
        group = a.group;
        list = el("div", { className: "keys-group" }, el("h3", { className: "field-label", textContent: group }));
        nodes.push(list);
      }
      list.append(keyRow(a, map[a.id] || []));
    }
    nodes.push(
      el("p", {
        className: "sheet-body settings-note",
        textContent: "Fixed (not rebindable): arrows / j k / Enter / Esc inside the palette, drawers, and search; F2 and Delete on a sidebar row; Ctrl Enter in Compose.",
      }),
    );
    return nodes;
  }

  function keyRow(a, keys) {
    const capturing = view.capturing && view.capturing.id === a.id;
    const changedHere = Object.prototype.hasOwnProperty.call(server.keys || {}, a.id) || Object.prototype.hasOwnProperty.call(deviceKeys, a.id);
    const chips = keys.map((k, i) =>
      el(
        "span",
        { className: "key-chip" },
        el("kbd", { textContent: label(k) }),
        el("button", {
          type: "button",
          className: "key-x",
          title: `Remove ${label(k)}`,
          "aria-label": `Remove ${label(k)} from ${a.label}`,
          textContent: "×",
          onclick: () => apply(a, keys.filter((_, j) => j !== i)),
        }),
      ),
    );
    const status = capturing ? view.capturing : null;
    return el(
      "div",
      { className: `key-row${capturing ? " capturing" : ""}`, dataset: { action: a.id } },
      el("span", { className: "key-label", textContent: a.label }),
      el(
        "span",
        { className: "key-chips" },
        chips.length ? chips : el("span", { className: "key-none", textContent: "unbound" }),
        capturing
          ? el("kbd", { className: "key-capture", textContent: status.pending ? label(status.pending) : "press a key…" })
          : null,
      ),
      el(
        "span",
        { className: "key-actions" },
        capturing
          ? el("button", { type: "button", className: "text-btn", textContent: "cancel", onclick: () => stopCapture() })
          : el("button", {
            type: "button",
            className: "text-btn",
            textContent: keys.length ? "add" : "set",
            title: a.indexed ? "Press a chord with a digit: it binds 1-9" : "Press the new key",
            onclick: () => startCapture(a),
          }),
        changedHere && !capturing
          ? el("button", { type: "button", className: "text-btn", textContent: "default", onclick: () => apply(a, null) })
          : null,
      ),
      status && (status.errors.length || status.warnings.length)
        ? el(
          "div",
          { className: "key-findings" },
          status.errors.map((f) => el("p", { className: "form-error", textContent: f.key ? `${label(f.key)}: ${f.message.replace(f.key, label(f.key))}` : f.message })),
          status.warnings.map((f) => el("p", { className: "key-warn", textContent: `${label(f.key)}: ${f.message}` })),
        )
        : null,
    );
  }

  async function apply(a, keys) {
    const result = await bind(a.id, keys);
    if (!result.ok) {
      view.capturing = { id: a.id, pending: null, errors: result.errors, warnings: [] };
    } else {
      view.capturing = null;
      if (result.warnings.length) toast(`${a.label}: ${result.warnings.map((w) => w.message).join("; ")}`, { timeout: 7000 });
    }
    renderSection();
  }

  function startCapture(a) {
    view.capturing = { id: a.id, scope: a.scope, indexed: a.indexed, pending: null, errors: [], warnings: [] };
    renderSection();
  }
  function stopCapture() {
    view.capturing = null;
    renderSection();
  }

  // While capturing, every key goes here first (before the hub's
  // shortcuts and the dialog's own Esc).
  window.addEventListener(
    "keydown",
    async (event) => {
      if (!view.capturing || !dialog.open) return;
      if (event.key === "Escape" && !event.ctrlKey && !event.altKey && !event.metaKey) {
        event.preventDefault();
        event.stopImmediatePropagation();
        stopCapture();
        return;
      }
      const got = capture(event, view.capturing.scope);
      if (!got) return;
      event.preventDefault();
      event.stopImmediatePropagation();
      if (got.error) {
        view.capturing.errors = [{ key: "", message: got.error }];
        renderSection();
        return;
      }
      let key = got.key;
      if (view.capturing.indexed) {
        const digit = /^(.*?)[1-9]$/.exec(key);
        if (!digit) {
          view.capturing.errors = [{ key, message: "this action takes a chord with a digit (it binds 1-9)" }];
          renderSection();
          return;
        }
        key = `${digit[1]}1..9`;
      }
      const a = action(view.capturing.id);
      const keys = [...(keymap()[a.id] || []).filter((k) => k !== key), key].slice(-4);
      view.capturing.pending = key;
      renderSection();
      await apply(a, keys);
    },
    true,
  );

  function pluginsSection() {
    if (!plugins) {
      loadPlugins();
      return [el("p", { className: "sheet-body", textContent: "Loading plugins…" })];
    }
    const nodes = [
      el(
        "p",
        { className: "sheet-body settings-note" },
        "Plugins are programs in ",
        el("code", { textContent: plugins.dir || "<state-dir>/plugins" }),
        " (one directory each, with a plugin.toml) that run on hub events and as palette actions. They run as you, with no sandbox. This page can switch them on and off and run them; it never installs one -- copy a directory there yourself, after reading it.",
      ),
    ];
    if (plugins.enabled === false) {
      nodes.push(el("p", { className: "form-error", textContent: "Plugins are off for this hub (hub.toml plugins.enabled = false)." }));
    }
    if (!plugins.plugins.length) nodes.push(el("p", { className: "sheet-body", textContent: "No plugins found." }));
    for (const p of plugins.plugins) nodes.push(pluginCard(p));
    return nodes;
  }

  function pluginCard(p) {
    const lastError = p.last_error;
    const card = el(
      "div",
      { className: `plugin-card${p.valid ? "" : " invalid"}`, dataset: { plugin: p.name } },
      el(
        "div",
        { className: "plugin-head" },
        el("b", { textContent: p.name }),
        p.version ? el("span", { className: "field-hint", textContent: ` v${p.version}` }) : null,
        el(
          "label",
          { className: "pref-device plugin-toggle" },
          el("input", {
            type: "checkbox",
            checked: p.enabled,
            "aria-label": `Enable ${p.name}`,
            onchange: async (e) => {
              try {
                await rawApi("PUT", `/api/plugins/${encodeURIComponent(p.name)}`, { enabled: e.target.checked });
              } catch (error) {
                toast(`${p.name}: ${error.message}`, { tone: "bad" });
              }
              loadPlugins();
            },
          }),
          " enabled",
        ),
      ),
      p.description ? el("p", { className: "sheet-body", textContent: p.description }) : null,
      p.error ? el("p", { className: "form-error", textContent: `plugin.toml: ${p.error}` }) : null,
      p.events && p.events.length
        ? el("p", {
          className: "plugin-meta",
          textContent: `on: ${p.events.map((e) => (e.statuses && e.statuses.length ? `${e.on} (${e.statuses.join(", ")})` : e.on)).join(", ")}`,
        })
        : null,
      p.actions && p.actions.length
        ? el(
          "div",
          { className: "plugin-actions" },
          p.actions.map((a) =>
            el("button", {
              type: "button",
              className: "slab ghost",
              disabled: !p.enabled || !p.valid || (a.context === "session" && !ctx.attachedLocal()),
              title: a.context === "session" ? (ctx.attachedLocal() ? "Runs for the attached session" : "Needs a session: attach one on this hub first") : "",
              textContent: a.title,
              onclick: () => runPlugin(p.name, a),
            }),
          ),
        )
        : null,
      p.last_run
        ? el("p", {
          className: "plugin-meta",
          textContent: `last run ${new Date(p.last_run.at).toLocaleString()} (${p.last_run.trigger}): ${p.last_run.ok ? "ok" : p.last_run.error}`,
        })
        : null,
      lastError && (!p.last_run || !p.last_run.ok || lastError.at !== p.last_run.at)
        ? el("p", { className: "form-error", textContent: `last error ${new Date(lastError.at).toLocaleString()} (${lastError.trigger}): ${lastError.error}` })
        : null,
      el("button", {
        type: "button",
        className: "text-btn",
        textContent: "logs",
        onclick: (e) => showLogs(p.name, e.target.closest(".plugin-card")),
      }),
    );
    return card;
  }

  async function showLogs(name, card) {
    let runs;
    try {
      runs = (await rawApi("GET", `/api/plugins/${encodeURIComponent(name)}/logs?limit=10`)).runs;
    } catch (error) {
      toast(error.message, { tone: "bad" });
      return;
    }
    const old = card.querySelector(".plugin-logs");
    if (old) old.remove();
    card.append(
      el(
        "ol",
        { className: "plugin-logs" },
        runs.length
          ? runs.map((r) =>
            el(
              "li",
              { className: r.ok ? "" : "bad" },
              el("span", { textContent: `${new Date(r.at).toLocaleTimeString()} ${r.trigger} ${r.timed_out ? "timed out" : r.exit_code !== null ? `exit ${r.exit_code}` : "not run"} ${r.duration_ms} ms` }),
              r.error ? el("span", { className: "form-error", textContent: ` ${r.error}` }) : null,
              r.stderr ? el("pre", { textContent: r.stderr }) : null,
            ),
          )
          : el("li", { textContent: "No runs yet." }),
      ),
    );
  }

  function serverSection() {
    if (!config) {
      loadConfig();
      return [el("p", { className: "sheet-body", textContent: "Loading…" })];
    }
    const file = config.file || {};
    const nodes = [
      el(
        "p",
        { className: "sheet-body settings-note" },
        "Read-only: the hub's own settings. Edit ",
        el("code", { textContent: file.path || "hub.toml" }),
        " on the hub's machine (re-read within a second; ",
        el("code", { textContent: "fw hub config check" }),
        " validates it). A flag given to fw hub serve beats the file, the file beats the default. ",
        el("a", { href: "https://github.com/johnayoung/flywheel/blob/main/docs/hub.md#settings-and-hubtoml", target: "_blank", rel: "noopener", textContent: "Reference" }),
      ),
    ];
    if (file.error) nodes.push(el("p", { className: "form-error", textContent: `Invalid, not in force (the last good settings stay): ${file.error}` }));
    else nodes.push(el("p", { className: "plugin-meta", textContent: file.exists ? "hub.toml loaded" : "no hub.toml: defaults and flags" }));
    if (config.pending_restart && config.pending_restart.length) {
      nodes.push(el("p", { className: "key-warn", textContent: `Waiting for a restart: ${config.pending_restart.join(", ")}` }));
    }
    const show = (v) => (v === null ? "(unset)" : typeof v === "string" ? (v === "" ? '""' : v) : JSON.stringify(v));
    nodes.push(
      el(
        "table",
        { className: "config-table" },
        el("thead", {}, el("tr", {}, ["key", "value", "from", "applies"].map((h) => el("th", { textContent: h })))),
        el(
          "tbody",
          {},
          (config.values || []).map((e) =>
            el(
              "tr",
              { dataset: { key: e.key } },
              el("td", {}, el("code", {}, ...e.key.split(".").flatMap((part, i) => (i ? [el("wbr"), `.${part}`] : [part])))),
              el("td", { textContent: show(e.value) }),
              el("td", { className: `src-${e.source}`, textContent: e.source }),
              el("td", { textContent: e.apply === "live" ? "live" : "at restart" }),
            ),
          ),
        ),
      ),
    );
    if (server.load_error) nodes.push(el("p", { className: "form-error", textContent: `settings.json: ${server.load_error}` }));
    return nodes;
  }

  function renderSection() {
    if (!dialog.open) return;
    const section = view.section;
    let nodes;
    if (section === "keyboard") nodes = [...prefsSection("keyboard"), ...keysSection()];
    else if (section === "plugins") nodes = pluginsSection();
    else if (section === "server") nodes = serverSection();
    else nodes = prefsSection(section);
    const scroll = body.scrollTop;
    body.replaceChildren(...nodes.filter(Boolean));
    body.scrollTop = scroll;
  }

  async function open(section) {
    if (section) view.section = section;
    view.capturing = null;
    renderNav();
    loadPlugins();
    loadConfig();
    if (!dialog.open) dialog.showModal();
    renderSection();
    load().then(() => renderSection());
  }

  dialog.addEventListener("close", () => {
    view.capturing = null;
  });

  // --- the keys reference (#dlg-keys), from the keymap in force -------------------

  function renderKeysHelp() {
    const list = $("#keys-list");
    if (!list) return;
    const map = keymap();
    const rows = [];
    for (const a of server.actions || []) {
      const keys = map[a.id] || [];
      if (!keys.length) continue;
      const prefixKey = a.scope === "prefix" ? `${label((map["prefix.arm"] || ["ctrl+b"])[0])}, then ` : "";
      rows.push(
        el(
          "div",
          {},
          el("dt", {}, keys.map((k) => el("kbd", { textContent: `${prefixKey}${label(k)}` }))),
          el("dd", { textContent: a.label + (a.scope === "prefix" ? " (pane prefix on)" : "") }),
        ),
      );
    }
    for (const [keys, text] of [
      ["drag a session", "onto a pane's edge: split that way; its middle: show it there. Right-click a pane's title bar: its menu"],
      ["F2 / double-click", "rename the focused sidebar session (Tab to it)"],
      ["Delete", "close the focused sidebar session"],
    ]) {
      rows.push(el("div", {}, el("dt", { textContent: keys }), el("dd", { textContent: text })));
    }
    list.replaceChildren(...rows);
  }

  function showKeys() {
    renderKeysHelp();
    $("#dlg-keys").showModal();
  }

  function paletteItems(cmd) {
    cmd("Settings", keyOf("settings.open") || "appearance, terminal, keys, plugins", () => open(), "gear");
    for (const [id, name] of SECTIONS) cmd(`Settings: ${name}`, "", () => open(id), "gear");
    for (const p of (plugins && plugins.plugins) || []) {
      if (!p.enabled || !p.valid) continue;
      for (const a of p.actions || []) {
        if (a.context === "session" && !ctx.attachedLocal()) continue;
        cmd(`Plugin: ${a.title}`, `${p.name}${a.context === "session" ? " · this session" : ""}`, () => runPlugin(p.name, a), "chev");
      }
    }
  }

  rebuild();
  return {
    get, set, isDevice, onChange, keymap, match, matchPrefix, label, keyOf, open, onPush, paletteItems, showKeys, hubDefault,
    start: async () => {
      await load();
      loadConfig();
      loadPlugins();
    },
    isCapturing: () => !!(view.capturing && dialog.open),
  };
};
