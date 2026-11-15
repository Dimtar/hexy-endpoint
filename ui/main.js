const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const $ = (id) => document.getElementById(id);
const els = {
  status: $("status"),
  host: $("host"),
  username: $("username"),
  password: $("password"),
  modeAll: $("mode-all"),
  modePick: $("mode-pick"),
  shares: $("shares"),
  autostart: $("autostart"),
  autoUpdate: $("auto-update"),
  error: $("error"),
  save: $("save"),
  sync: $("sync"),
  signout: $("signout"),
  version: $("version"),
  update: $("update"),
  updateText: $("update-text"),
  updateBtn: $("update-btn"),
};

let state = null;
// Share names ticked in the picker. Kept here, not read from the server
// state, so a background refresh never undoes an unsaved click.
let picked = new Set();
let formLoaded = false;

function showError(message) {
  els.error.textContent = message || "";
  els.error.hidden = !message;
}

function statusText(s) {
  if (!s.config.host || !s.has_password) return "Not signed in";
  if (s.error) return s.error;
  if (s.syncing && !s.last_sync) return `Connecting to ${s.config.host}…`;
  const mounted = s.shares.filter((x) => x.mounted).length;
  return `${mounted} of ${s.shares.length} shares mounted from ${s.config.host}`;
}

function renderShares() {
  const pickMode = els.modePick.checked;
  els.shares.replaceChildren();
  if (!state.shares.length) {
    const li = document.createElement("li");
    li.className = "empty";
    li.textContent = state.has_password
      ? "No shares found yet."
      : "Sign in to see the shares on your server.";
    els.shares.append(li);
    return;
  }
  for (const share of state.shares) {
    const li = document.createElement("li");

    if (pickMode) {
      const box = document.createElement("input");
      box.type = "checkbox";
      box.checked = picked.has(share.name);
      box.setAttribute("aria-label", `Mount ${share.name}`);
      box.addEventListener("change", () => {
        if (box.checked) picked.add(share.name);
        else picked.delete(share.name);
      });
      li.append(box);
    }

    const dot = document.createElement("span");
    dot.className = "dot" + (share.error ? " err" : share.mounted ? " on" : "");
    li.append(dot);

    const name = document.createElement("div");
    name.className = "name";
    const title = document.createElement("b");
    title.textContent = share.name;
    const detail = document.createElement("small");
    if (share.error) {
      detail.className = "bad";
      detail.textContent = share.error;
    } else if (share.mounted) {
      detail.textContent = `Mounted at ${share.location}`;
    } else {
      detail.textContent = share.comment || "Not mounted";
    }
    name.append(title, detail);
    li.append(name);

    if (share.mounted) {
      const open = document.createElement("button");
      open.type = "button";
      open.className = "small";
      open.textContent = "Open";
      open.addEventListener("click", () =>
        invoke("open_share", { name: share.name }).catch(showError)
      );
      li.append(open);
    }
    els.shares.append(li);
  }
}

function render(next) {
  state = next;
  els.status.textContent = statusText(state);
  els.version.textContent = `v${state.version}`;
  els.sync.disabled = state.syncing || !state.has_password;
  els.signout.disabled = !state.has_password;
  els.password.placeholder = state.has_password ? "Saved; leave blank to keep" : "";

  els.update.hidden = !state.update;
  if (state.update) els.updateText.textContent = `Version ${state.update} is available.`;

  // Form fields are filled once; after that they belong to the user.
  if (!formLoaded) {
    formLoaded = true;
    els.host.value = state.config.host;
    els.username.value = state.config.username;
    els.modeAll.checked = state.config.map_all;
    els.modePick.checked = !state.config.map_all;
    els.autostart.checked = state.config.autostart;
    els.autoUpdate.checked = state.config.auto_update;
    picked = new Set(state.config.selected);
  }
  renderShares();
}

function switchMode() {
  // Moving from "everything" to "choose" starts with everything ticked.
  if (els.modePick.checked && picked.size === 0 && state) {
    picked = new Set(state.shares.map((s) => s.name));
  }
  renderShares();
}
els.modeAll.addEventListener("change", switchMode);
els.modePick.addEventListener("change", switchMode);

$("form").addEventListener("submit", async (event) => {
  event.preventDefault();
  showError("");
  els.save.disabled = true;
  try {
    const next = await invoke("save_settings", {
      input: {
        host: els.host.value,
        username: els.username.value,
        password: els.password.value || null,
        map_all: els.modeAll.checked,
        selected: [...picked],
        autostart: els.autostart.checked,
        auto_update: els.autoUpdate.checked,
      },
    });
    els.password.value = "";
    render(next);
  } catch (err) {
    showError(String(err));
  } finally {
    els.save.disabled = false;
  }
});

els.sync.addEventListener("click", () => invoke("sync_now"));

els.signout.addEventListener("click", async () => {
  showError("");
  picked = new Set();
  render(await invoke("sign_out"));
});

els.updateBtn.addEventListener("click", async () => {
  els.updateBtn.disabled = true;
  els.updateBtn.textContent = "Installing…";
  try {
    await invoke("install_update");
  } catch (err) {
    showError(`Update failed: ${err}`);
    els.updateBtn.disabled = false;
    els.updateBtn.textContent = "Install and restart";
  }
});

listen("state", (event) => render(event.payload));
invoke("get_state").then(render);
