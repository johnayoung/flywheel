// flywheel hub: read-only share links and session recordings, in the app.
// The header's `share` pill (and the palette) opens the Sharing dialog on
// the attached session: mint a scoped, expiring link (POST /api/shares;
// shown once, copy it), see every live link with its viewers and revoke
// it, and the recordings (play in the replay player, download, delete).
// The `rec` pill starts and stops recording the attached session. This
// hub's own sessions only: a machine's are shared from that machine.
// app-base.js creates it with its helpers.
"use strict";

window.FwHubSharing = function (ctx) {
  const { $, el, rawApi, sessionById, toast, copyText, confirmDialog, span, refresh } = ctx;

  const dialog = $("#dlg-sharing");
  const ui = {
    session: null, // the session a new link would show
    seq: 0, // drops superseded list answers
    timer: 0, // re-reads the lists while the dialog is open (viewers, expiry)
  };
  const REFRESH_MS = 5000;

  const local = (session) => !!session && !session.machine;

  function size(bytes) {
    if (bytes < 1024) return `${bytes} B`;
    if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KiB`;
    if (bytes < 1024 * 1024 * 1024) return `${(bytes / 1024 / 1024).toFixed(1)} MiB`;
    return `${(bytes / 1024 / 1024 / 1024).toFixed(2)} GiB`;
  }

  function when(unix) {
    if (!unix) return "";
    const date = new Date(unix * 1000);
    const today = new Date().toDateString() === date.toDateString();
    return today
      ? date.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" })
      : date.toLocaleString([], { month: "short", day: "numeric", hour: "2-digit", minute: "2-digit" });
  }

  // --- the dialog ------------------------------------------------------

  function open(sessionId) {
    const session = sessionById(sessionId);
    ui.session = local(session) ? session.id : null;
    $("#share-new").hidden = !ui.session;
    if (ui.session) {
      $("#share-session").textContent = session.name;
      $("#share-result").hidden = true;
      $("#share-url").value = "";
      $("#share-error").textContent = "";
      $("#share-create").disabled = false;
    }
    if (!dialog.open) dialog.showModal();
    if (ui.session) $("#share-name").focus();
    reload();
    clearInterval(ui.timer);
    ui.timer = setInterval(() => {
      if (dialog.open) reload();
      else clearInterval(ui.timer);
    }, REFRESH_MS);
  }

  dialog.addEventListener("close", () => clearInterval(ui.timer));
  // Enter in the label field makes the link rather than closing.
  $("#sharing-form").addEventListener("submit", (event) => {
    if (event.submitter && event.submitter.value === "cancel") return;
    event.preventDefault();
    if (ui.session && $("#share-result").hidden) create();
  });

  async function create() {
    const session = sessionById(ui.session);
    if (!session) return;
    const button = $("#share-create");
    button.disabled = true;
    $("#share-error").textContent = "";
    const body = {
      session_id: session.id,
      expires_in: parseInt($("#share-expires").value, 10),
    };
    const name = $("#share-name").value.trim();
    if (name) body.name = name;
    try {
      const made = await rawApi("POST", "/api/shares", body);
      const url = `${window.location.origin}${made.path}`;
      $("#share-url").value = url;
      $("#share-result").hidden = false;
      $("#share-name").value = "";
      $("#share-url").select();
      const copied = await copyText(url);
      toast(copied ? "Share link copied: it opens a read-only view of this terminal." : "Share link made: copy it from the dialog.");
      reload();
    } catch (error) {
      $("#share-error").textContent = error.message;
    } finally {
      button.disabled = false;
    }
  }

  $("#share-create").addEventListener("click", create);
  $("#share-copy").addEventListener("click", async () => {
    const url = $("#share-url").value;
    if (!url) return;
    const copied = await copyText(url);
    toast(copied ? "Copied." : "Copy failed: select the link and copy it.", { tone: copied ? "" : "bad" });
  });

  async function reload() {
    const seq = ++ui.seq;
    let shares;
    let recordings;
    try {
      [shares, recordings] = await Promise.all([
        rawApi("GET", "/api/shares"),
        rawApi("GET", "/api/recordings"),
      ]);
    } catch (error) {
      if (seq === ui.seq) $("#share-list").replaceChildren(el("li", { className: "sh-empty bad" }, error.message));
      return;
    }
    if (seq !== ui.seq) return;
    renderShares(shares.shares || []);
    renderRecordings(recordings);
  }

  function renderShares(shares) {
    const list = $("#share-list");
    if (!shares.length) {
      list.replaceChildren(el("li", { className: "sh-empty" }, "No live links. Share a session to make one (fw hub share SESSION)."));
      return;
    }
    const now = Date.now() / 1000;
    list.replaceChildren(
      ...shares.map((share) =>
        el(
          "li",
          { className: "sh-row" },
          el(
            "div",
            { className: "sh-main" },
            el("span", { className: "sh-name" }, share.session || "removed session"),
            share.name ? el("span", { className: "sh-label" }, share.name) : null,
          ),
          el(
            "div",
            { className: "sh-meta" },
            `expires in ${span(Math.max(0, share.expires - now))}`,
            share.viewers ? el("b", { className: "sh-live" }, ` · ${share.viewers} watching`) : "",
            el("span", { className: "sh-id" }, ` · ${share.id}`),
          ),
          el(
            "div",
            { className: "sh-actions" },
            el("button", {
              type: "button",
              className: "slab ghost danger-ghost",
              textContent: "Revoke",
              onclick: () => revoke(share),
            }),
          ),
        ),
      ),
    );
  }

  async function revoke(share) {
    const what = share.name ? `${share.name} (${share.session || "removed session"})` : share.session || share.id;
    const watching = share.viewers ? ` ${share.viewers} viewer${share.viewers === 1 ? " is" : "s are"} watching: their view closes now.` : "";
    const ok = await confirmDialog({
      kicker: "share link",
      title: `Revoke ${what}?`,
      body: `The link stops working at once.${watching}`,
      ok: "Revoke",
    });
    if (!ok) return;
    try {
      await rawApi("DELETE", `/api/shares/${encodeURIComponent(share.id)}`);
      toast("Share link revoked.");
    } catch (error) {
      toast(`Revoke failed: ${error.message}`, { tone: "bad" });
    }
    reload();
  }

  function renderRecordings(answer) {
    const list = $("#rec-list");
    const recordings = answer.recordings || [];
    const limits = answer.limits || {};
    $("#rec-usage").textContent = limits.max_total_bytes
      ? `${size(answer.total_bytes || 0)} of ${size(limits.max_total_bytes)} (oldest go first) · ${size(limits.max_file_bytes)} per part`
      : "";
    if (!recordings.length) {
      list.replaceChildren(el("li", { className: "sh-empty" }, "No recordings. Start one with the rec pill (fw hub record start SESSION)."));
      return;
    }
    list.replaceChildren(
      ...recordings.map((rec) => {
        const path = `/api/recordings/${encodeURIComponent(rec.name)}`;
        return el(
          "li",
          { className: `sh-row${rec.active ? " active" : ""}` },
          el(
            "div",
            { className: "sh-main" },
            rec.active ? el("span", { className: "rec-dot", title: "recording now" }) : null,
            el("span", { className: "sh-name" }, rec.title || rec.name),
            rec.part > 1 ? el("span", { className: "sh-label" }, `part ${rec.part}`) : null,
          ),
          el(
            "div",
            { className: "sh-meta" },
            [when(rec.started), size(rec.size), rec.cols && rec.rows ? `${rec.cols}×${rec.rows}` : ""].filter(Boolean).join(" · "),
            el("span", { className: "sh-id" }, ` · ${rec.name}`),
          ),
          el(
            "div",
            { className: "sh-actions" },
            el("a", { className: "slab ghost", href: `/replay#${encodeURIComponent(rec.name)}`, target: "_blank", rel: "noopener", textContent: "Play" }),
            el("a", { className: "slab ghost", href: path, download: rec.name, textContent: "Download" }),
            rec.active
              ? el("button", { type: "button", className: "slab ghost", textContent: "Stop", onclick: () => stopRecording(rec.session_id) })
              : el("button", { type: "button", className: "slab ghost danger-ghost", textContent: "Delete", onclick: () => remove(rec) }),
          ),
        );
      }),
    );
  }

  async function remove(rec) {
    const ok = await confirmDialog({
      kicker: "recording",
      title: `Delete ${rec.title || rec.name}?`,
      body: `${rec.name} (${size(rec.size)}) is deleted from the hub's state dir.`,
      ok: "Delete",
    });
    if (!ok) return;
    try {
      await rawApi("DELETE", `/api/recordings/${encodeURIComponent(rec.name)}`);
    } catch (error) {
      toast(`Delete failed: ${error.message}`, { tone: "bad" });
    }
    reload();
  }

  // --- recording -------------------------------------------------------

  async function stopRecording(sessionId) {
    try {
      const stopped = await rawApi("POST", `/api/sessions/${encodeURIComponent(sessionId)}/record/stop`, {});
      const name = stopped.recording.name;
      toast(el("span", {}, "Recording saved: ", el("b", { textContent: name })), {
        actions: [
          el("button", {
            type: "button",
            textContent: "Play",
            onclick: () => window.open(`/replay#${encodeURIComponent(name)}`, "_blank", "noopener"),
          }),
        ],
      });
    } catch (error) {
      toast(`Stop failed: ${error.message}`, { tone: "bad" });
    }
    await refresh();
    if (dialog.open) reload();
  }

  async function toggleRecord(sessionId) {
    const session = sessionById(sessionId);
    if (!local(session)) return;
    if (session.recording) {
      await stopRecording(session.id);
      return;
    }
    try {
      await rawApi("POST", `/api/sessions/${encodeURIComponent(session.id)}/record/start`, {});
      toast(`Recording ${session.name}: output, timing, and resizes go to a .cast file until you stop it or it exits.`);
    } catch (error) {
      toast(`Record failed: ${error.message}`, { tone: "bad" });
    }
    await refresh();
    if (dialog.open) reload();
  }

  // --- the header ------------------------------------------------------

  function renderHeader(session) {
    const ok = local(session);
    const share = $("#share-toggle");
    share.hidden = !ok;
    const rec = $("#rec-toggle");
    const recording = ok && !!session.recording;
    rec.hidden = !ok || (!session.live && !recording);
    rec.classList.toggle("on", recording);
    rec.setAttribute("aria-pressed", recording ? "true" : "false");
    rec.title = recording
      ? `Recording to ${session.recording}: click to stop`
      : "Record this session (asciinema .cast: output, timing, resizes)";
    $("#rec-label").textContent = recording ? "recording" : "rec";
    const links = session && session.live ? session.live.link_viewers || 0 : 0;
    share.title = links
      ? `${links} watching through a share link · share a read-only view`
      : "Share a read-only live view (an expiring link)";
    $("#share-count").hidden = !links;
    $("#share-count").textContent = links ? String(links) : "";
  }

  // Commands for the palette.
  function paletteItems(attached) {
    const items = [];
    if (local(attached)) {
      items.push({ label: `Share ${attached.name} (read-only link)`, hint: "expiring, revocable", glyph: "send", run: () => open(attached.id) });
      if (attached.recording) items.push({ label: `Stop recording ${attached.name}`, hint: attached.recording, glyph: "stop", run: () => toggleRecord(attached.id) });
      else if (attached.live) items.push({ label: `Record ${attached.name}`, hint: "asciinema .cast", glyph: "redo", run: () => toggleRecord(attached.id) });
    }
    items.push({ label: "Sharing & recordings", hint: "share links, recordings, replay", glyph: "chev", run: () => open(null) });
    return items;
  }

  return { open, toggleRecord, renderHeader, paletteItems };
};
