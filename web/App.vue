<script setup lang="ts">
import { ref, computed, onMounted, onBeforeUnmount } from "vue";
import { useRoute, useRouter } from "vue-router";
import Button from "./components/ui/button/Button.vue";
import Screen from "./components/Screen.vue";
import { api, setCsrf, headers, rememberToken } from "./api";
const route = useRoute(),
  router = useRouter();
const section = computed(() => String(route.params.section ?? "computers"));
const logged = ref(false),
  pat = ref(""),
  error = ref(""),
  loading = ref(false),
  devices = ref<any[]>([]),
  events = ref<any[]>([]),
  plugins = ref<any[]>([]),
  extensions = ref<any[]>([]),
  tokens = ref<any[]>([]),
  diagnostics = ref<any>(),
  query = ref(""),
  modal = ref(false),
  secret = ref(""),
  username = ref("admin"),
  newToken = ref("");
const prefs = ref({
  revision: 0,
  sidebar: true,
  theme: "dark",
  selected: "",
  order: [] as string[],
  visible: [] as string[],
  tile_size: 400,
});
const editing = ref<any>({});
const driverModels: Record<string, string[]> = {
  "pikvm-v4": ["PiKVM v4", "PiKVM v4 Mini", "PiKVM v4 Plus"],
  jetkvm: ["JetKVM"],
  "glinet-comet": ["GL-RM1 (Comet)", "GL-RM10 (Comet Pro)"],
  simulator: ["Synthetic KVM"],
};
function changeDriver() {
  editing.value.model = driverModels[editing.value.driver_id][0];
  editing.value.transport =
    editing.value.driver_id === "simulator" ? "simulator" : "network";
  if (editing.value.transport === "network" && !editing.value.address)
    editing.value.address = "http://";
}
const caPem = ref(""),
  clearCredentials = ref(false),
  clearCa = ref(false);
const me = ref<any>();
const tokenName = ref("Codex"),
  tokenDevices = ref("*"),
  tokenScopes = ref(["devices:read", "video:read", "input:write"]);
const allScopes = [
  "devices:read",
  "devices:manage",
  "video:read",
  "input:write",
  "audio:read",
  "audio:write",
  "plugins:manage",
  "tokens:manage",
  "control:stop",
];
const pluginPath = ref(""),
  pluginDevices = ref("");
let timer: ReturnType<typeof setInterval>;
let controller: AbortController | undefined;
let saveTail = Promise.resolve();
const selected = computed(() =>
  devices.value.find((d) => d.device_id === prefs.value.selected),
);
const allOrdered = computed(() =>
  [...devices.value].sort((a, b) => {
    const x = prefs.value.order.indexOf(a.device_id),
      y = prefs.value.order.indexOf(b.device_id);
    return (x < 0 ? 999 : x) - (y < 0 ? 999 : y);
  }),
);
const ordered = computed(() =>
  allOrdered.value.filter((d) =>
    `${d.name} ${d.tags.join(" ")}`
      .toLowerCase()
      .includes(query.value.toLowerCase()),
  ),
);
const visible = computed(() =>
  ordered.value.filter((d) => prefs.value.visible.includes(d.device_id)),
);
const ready = computed(
  () =>
    devices.value.filter((d) => ["ready", "no_signal"].includes(d.status))
      .length,
);
const can = (s: string) => me.value?.token.scopes.includes(s);
async function run(fn: () => Promise<any>) {
  error.value = "";
  try {
    return await fn();
  } catch (e) {
    error.value = (e as Error).message;
  }
}
async function refresh() {
  devices.value = await api("/devices");
  extensions.value = await api("/ui-extensions");
}
async function load() {
  const data = await api("/me");
  me.value = data;
  setCsrf(data.csrf ?? "");
  prefs.value = await api("/preferences");
  document.documentElement.dataset.theme = prefs.value.theme;
  await refresh();
  logged.value = true;
  if (!prefs.value.selected && devices.value.length) {
    prefs.value.selected = devices.value[0].device_id;
    await save();
  }
  if (can("plugins:manage")) plugins.value = await api("/plugins");
  void listen();
}
async function login() {
  loading.value = true;
  await run(async () => {
    const data = await api("/login", "POST", { pat: pat.value });
    setCsrf(data.csrf);
    rememberToken(pat.value.trim());
    pat.value = "";
    await load();
  });
  loading.value = false;
}
function save() {
  saveTail = saveTail.then(async () => {
    try {
      const result = await api("/preferences", "PUT", prefs.value);
      prefs.value.revision = result.revision;
      document.documentElement.dataset.theme = prefs.value.theme;
    } catch (e) {
      prefs.value = await api("/preferences");
      error.value =
        "Einstellungen wurden in einem anderen Fenster geändert. Aktueller Stand geladen.";
    }
  });
  return saveTail;
}
function select(id: string) {
  prefs.value.selected = id;
  void save();
  void router.push("/computers");
}
function toggle(id: string) {
  prefs.value.visible = prefs.value.visible.includes(id)
    ? prefs.value.visible.filter((x) => x !== id)
    : [...prefs.value.visible, id];
  void save();
}
function reorder(id: string, delta: number, wall = false) {
  const ids = allOrdered.value.map((d) => d.device_id),
    n = ids.indexOf(id),
    candidates = (wall ? visible.value : allOrdered.value).map(
      (d) => d.device_id,
    ),
    target = candidates[candidates.indexOf(id) + delta],
    m = ids.indexOf(target);
  if (m < 0 || m >= ids.length) return;
  [ids[n], ids[m]] = [ids[m], ids[n]];
  prefs.value.order = ids;
  void save();
}
function edit(d?: any) {
  editing.value = d
    ? Object.fromEntries(
        [
          "device_id",
          "name",
          "driver_id",
          "model",
          "firmware",
          "address",
          "secret_ref",
          "tls",
          "tags",
          "enabled",
          "media_profile",
          "keyboard_layout",
          "minimum_action_latency_ms",
          "transport",
          "revision",
        ].map((k) => [k, d[k]]),
      )
    : {
        name: "",
        driver_id: "pikvm-v4",
        model: "PiKVM v4",
        firmware: "unknown",
        address: "https://",
        tls: {},
        tags: [],
        enabled: true,
        media_profile: "auto",
        keyboard_layout: "de",
        minimum_action_latency_ms: 30,
        transport: "network",
        revision: 0,
      };
  editing.value.minimum_action_latency_ms ??= 30;
  editing.value.tls = { ...editing.value.tls };
  secret.value = "";
  caPem.value = "";
  clearCredentials.value = false;
  clearCa.value = false;
  username.value = "admin";
  modal.value = true;
}
async function saveDevice() {
  await run(async () => {
    if (editing.value.driver_id === "simulator") {
      editing.value.transport = "simulator";
      delete editing.value.address;
      editing.value.model = "Synthetic KVM";
    }
    await api("/devices", "POST", {
      device: editing.value,
      ...(secret.value
        ? { secret: { username: username.value, password: secret.value } }
        : clearCredentials.value
          ? { secret: null }
          : {}),
      ...(caPem.value
        ? { ca: caPem.value }
        : clearCa.value
          ? { ca: null }
          : {}),
    });
    secret.value = "";
    modal.value = false;
    await refresh();
  });
}
async function rememberCertificate() {
  await run(async () => {
    const cert = await api("/certificates/inspect", "POST", {
      address: editing.value.address,
    });
    editing.value.tls.fingerprint = cert.fingerprint;
    editing.value.tls.insecure = false;
  });
}
async function remove(d: any) {
  if (
    !window.confirm(
      `„${d.name}“ entfernen? Aktive Sitzungen und Verbraucher werden beendet.`,
    )
  )
    return;
  await run(async () => {
    await api("/devices/" + d.device_id, "DELETE", {
      revision: d.revision,
      confirmed: true,
    });
    await refresh();
  });
}
async function stopAll() {
  await run(async () => {
    await Promise.all(
      devices.value.map((d) => api(`/devices/${d.device_id}/stop`, "POST", {})),
    );
    await refresh();
  });
}
async function page(name: string) {
  await router.push("/" + name);
  await run(async () => {
    if (name === "settings" && can("tokens:manage"))
      tokens.value = await api("/tokens");
    if (name === "plugins" && can("plugins:manage"))
      plugins.value = await api("/plugins");
  });
}
async function listen() {
  controller?.abort();
  controller = new AbortController();
  try {
    const response = await fetch("/api/v1/events", {
      headers: headers(),
      signal: controller.signal,
    });
    if (!response.ok) return;
    const reader = response.body!.getReader(),
      decoder = new TextDecoder();
    let buffer = "";
    while (true) {
      const { done, value } = await reader.read();
      if (done) break;
      buffer += decoder.decode(value, { stream: true });
      let n;
      while ((n = buffer.indexOf("\n\n")) >= 0) {
        const packet = buffer.slice(0, n);
        buffer = buffer.slice(n + 2);
        const line = packet.split("\n").find((x) => x.startsWith("data: "));
        if (!line) continue;
        const e = JSON.parse(line.slice(6));
        if (!events.value.some((x) => x.event_id === e.event_id))
          events.value = [e, ...events.value].slice(0, 256);
        if (e.type.startsWith("plugin.") && can("plugins:manage"))
          plugins.value = await api("/plugins");
      }
    }
  } catch {}
}
onMounted(async () => {
  try {
    await load();
  } catch {}
  timer = setInterval(() => {
    if (logged.value) void refresh().catch(() => {});
  }, 3000);
});
onBeforeUnmount(() => {
  clearInterval(timer);
  controller?.abort();
});
</script>
<template>
  <div v-if="!logged" class="login-shell">
    <div class="login-card">
      <h1>KVMHelm</h1>
      <form @submit.prevent="login">
        <label
          >Personal Access Token<input
            v-model="pat"
            type="password"
            autocomplete="off"
            placeholder="kh_…"
            required /></label
        ><Button :disabled="loading" class="login-button">{{
          loading ? "Verbinden …" : "Anmelden →"
        }}</Button>
      </form>
      <p v-if="error" class="error">{{ error }}</p>
    </div>
  </div>
  <div v-else class="app-shell" :class="{ collapsed: !prefs.sidebar }">
    <aside>
      <div class="brand">
        <strong v-if="prefs.sidebar">KVMHelm</strong
        ><Button
          variant="ghost"
          size="icon"
          @click="
            prefs.sidebar = !prefs.sidebar;
            save();
          "
          aria-label="Seitenleiste umschalten"
          >☰</Button
        >
      </div>
      <nav>
        <button
          v-for="item in [
            { id: 'computers', icon: '▣', label: 'Computer' },
            { id: 'overview', icon: '▦', label: 'Videowand' },
            { id: 'events', icon: '◷', label: 'Ereignisse' },
            { id: 'plugins', icon: '◇', label: 'Plugins' },
            { id: 'settings', icon: '⚙', label: 'Einstellungen' },
          ]"
          :key="item.id"
          :class="{ active: section === item.id }"
          @click="page(item.id)"
          :title="item.label"
        >
          <span>{{ item.icon }}</span
          ><template v-if="prefs.sidebar"
            >{{ item.label
            }}<small v-if="item.id === 'computers'">{{
              devices.length
            }}</small></template
          >
        </button>
      </nav>
      <div v-if="prefs.sidebar" class="sidebar-devices">
        <div class="workspace-label">DEINE COMPUTER</div>
        <button
          v-for="d in ordered"
          :key="d.device_id"
          @click="select(d.device_id)"
          :class="{ chosen: prefs.selected === d.device_id }"
        >
          <span class="status-dot" :class="d.status" />{{ d.name
          }}<span v-if="d.lease">⌁</span></button
        ><Button v-if="can('devices:manage')" variant="ghost" @click="edit()"
          >＋ Computer hinzufügen</Button
        >
      </div>
      <div class="sidebar-bottom">
        <span class="status-dot ready" /><span v-if="prefs.sidebar"
          >Gateway verbunden</span
        >
      </div>
    </aside>
    <main>
      <header>
        <div>
          <h1>
            {{
              {
                computers: "Computer",
                overview: "Videowand",
                events: "Ereignisse",
                plugins: "Erweiterungen",
                settings: "Einstellungen",
              }[section] ?? "KVMHelm"
            }}
          </h1>
        </div>
        <div class="header-actions">
          <span class="pill">{{ ready }} / {{ devices.length }} verbunden</span
          ><Button
            variant="ghost"
            size="icon"
            aria-label="Farbschema wechseln"
            @click="
              prefs.theme = prefs.theme === 'dark' ? 'light' : 'dark';
              save();
            "
            >{{ prefs.theme === "dark" ? "☀" : "☾" }}</Button
          ><Button
            v-if="can('control:stop')"
            variant="destructive"
            @click="stopAll"
            >■ Not-Stopp</Button
          >
        </div>
      </header>
      <div v-if="error" role="alert" class="error banner">
        {{ error
        }}<button @click="error = ''" aria-label="Fehler schließen">×</button>
      </div>
      <template v-if="section === 'computers'"
        ><div class="section-heading">
          <Button v-if="can('devices:manage')" variant="outline" @click="edit()"
            >＋ Computer hinzufügen</Button
          >
        </div>
        <div v-if="devices.length" class="tabs-row">
          <input
            v-model="query"
            class="search"
            placeholder="Computer suchen …"
            aria-label="Computer suchen"
          />
          <div class="tabs">
            <button
              v-for="d in ordered"
              :key="d.device_id"
              :class="{ active: d.device_id === prefs.selected }"
              @click="select(d.device_id)"
            >
              <span class="status-dot" :class="d.status" />{{ d.name }}
            </button>
          </div>
        </div>
        <div v-if="selected" class="computer-layout">
          <Screen
            :key="selected.device_id"
            :device="selected"
            @refresh="run(refresh)"
          />
          <div class="detail-row">
            <div class="detail-card">
              <p class="eyebrow">VERBINDUNG</p>
              <strong>{{ selected.model }}</strong>
              <p>{{ selected.address ?? "Lokale synthetische Quelle" }}</p>
              <small
                >Firmware: {{ selected.firmware }} · Layout:
                {{ selected.keyboard_layout.toUpperCase() }}</small
              >
              <div class="inline-actions" v-if="can('devices:manage')">
                <Button variant="ghost" size="sm" @click="edit(selected)"
                  >Bearbeiten</Button
                ><Button
                  variant="ghost"
                  size="sm"
                  @click="
                    run(async () => {
                      await api(
                        `/devices/${selected.device_id}/test`,
                        'POST',
                        {},
                      );
                      await refresh();
                    })
                  "
                  >Verbindung testen</Button
                ><Button
                  variant="ghost"
                  size="sm"
                  @click="reorder(selected.device_id, -1)"
                  >←</Button
                ><Button
                  variant="ghost"
                  size="sm"
                  @click="reorder(selected.device_id, 1)"
                  >→</Button
                >
              </div>
            </div>
            <div v-if="can('devices:manage')" class="detail-card">
              <Button
                variant="ghost"
                size="sm"
                @click="
                  run(async () => {
                    diagnostics = await api(
                      `/devices/${selected.device_id}/benchmark`,
                      'POST',
                      {},
                    );
                    await refresh();
                  })
                "
                >Medienpfade vermessen ↗</Button
              >
              <pre v-if="diagnostics">{{
                JSON.stringify(diagnostics, null, 2)
              }}</pre>
            </div>
          </div>
          <template v-for="p in extensions" :key="p.id"
            ><div
              v-for="panel in p.panels.filter(
                (x: any) =>
                  x.device_id === selected.device_id &&
                  x.slot === 'kvm.sidepanel',
              )"
              :key="panel.id"
              class="detail-card"
            >
              <h3>{{ panel.title }}</h3>
              <p class="preserve">{{ panel.text }}</p>
            </div></template
          >
        </div>
        <div v-else class="empty-state">
          <div class="empty-icon">▣</div>
          <h2>Keine Computer vorhanden</h2>
          <Button v-if="can('devices:manage')" @click="edit()"
            >Computer hinzufügen</Button
          >
        </div></template
      >
      <template v-else-if="section === 'overview'"
        ><div class="section-heading">
          <p class="muted">{{ visible.length }} sichtbare Quellen</p>
          <label class="inline-label"
            >Kachelgröße
            <input
              v-model.number="prefs.tile_size"
              type="range"
              min="280"
              max="800"
              step="40"
              @change="save()"
          /></label>
        </div>
        <div class="visibility-bar">
          <label v-for="d in ordered" :key="d.device_id"
            ><input
              type="checkbox"
              :checked="prefs.visible.includes(d.device_id)"
              @change="toggle(d.device_id)"
            />{{ d.name }}</label
          >
        </div>
        <div class="wall" :style="{ '--tile': prefs.tile_size + 'px' }">
          <div
            v-for="(d, index) in visible"
            :key="d.device_id"
            :data-device-id="d.device_id"
          >
            <div
              class="order-controls"
              role="group"
              :aria-label="'Reihenfolge für ' + d.name"
            >
              <span>{{ index + 1 }}</span>
              <Button
                variant="ghost"
                size="sm"
                :disabled="index === 0"
                :aria-label="d.name + ' nach vorne'"
                @click="reorder(d.device_id, -1, true)"
                >←</Button
              >
              <Button
                variant="ghost"
                size="sm"
                :disabled="index === visible.length - 1"
                :aria-label="d.name + ' nach hinten'"
                @click="reorder(d.device_id, 1, true)"
                >→</Button
              >
            </div>
            <Screen :device="d" overview />
            <div class="tile-footer">
              <template v-for="p in extensions" :key="p.id"
                ><span
                  v-for="badge in p.panels.filter(
                    (x: any) =>
                      x.slot === 'overview.badge' &&
                      x.device_id === d.device_id,
                  )"
                  :key="badge.id"
                  class="pill accent"
                  :title="badge.text"
                  >{{ badge.title }}</span
                ></template
              >
              <span
                >{{
                  d.lease
                    ? "Steuerung: " + d.lease.client_id
                    : "Keine aktive Steuerung"
                }}
                · {{ d.consumers.length }} Verbraucher</span
              ><Button variant="ghost" size="sm" @click="select(d.device_id)"
                >Öffnen ↗</Button
              >
            </div>
          </div>
        </div>
        <div v-if="!visible.length" class="empty-state">
          <h2>Keine Computer ausgewählt</h2>
          <p>
            Wähle oben die Computer aus, die du gleichzeitig sehen möchtest.
          </p>
        </div></template
      >
      <template v-else-if="section === 'events'">
        <div class="event-list">
          <div v-for="e in events" :key="e.event_id" class="event">
            <span class="event-icon">◷</span>
            <div>
              <strong>{{ e.type }}</strong>
              <p>
                {{
                  devices.find((d) => d.device_id === e.device_id)?.name ??
                  "Gateway"
                }}
                <span v-if="e.data?.text">· {{ e.data.text }}</span>
              </p>
            </div>
            <span class="muted"
              >{{ e.historical ? "Verlauf · " : ""
              }}{{ new Date(e.at).toLocaleTimeString() }}</span
            >
          </div>
          <div v-if="!events.length" class="empty-state">
            <h2>Keine Ereignisse</h2>
          </div>
        </div></template
      >
      <template v-else-if="section === 'plugins'">
        <div v-if="can('plugins:manage')" class="detail-card">
          <h3>Lokales Plugin installieren</h3>
          <div class="input-tools">
            <input
              v-model="pluginPath"
              placeholder="Absoluter Plugin-Verzeichnispfad"
              aria-label="Pluginpfad"
            /><input
              v-model="pluginDevices"
              placeholder="Freigegebene Geräte-IDs, mit Komma getrennt"
              aria-label="Gerätefreigaben"
            /><Button
              @click="
                run(async () => {
                  await api('/plugins', 'POST', {
                    path: pluginPath,
                    devices: pluginDevices
                      .split(',')
                      .map((x) => x.trim())
                      .filter(Boolean),
                  });
                  plugins = await api('/plugins');
                })
              "
              >Installieren</Button
            >
          </div>
        </div>
        <template v-for="p in extensions" :key="p.id"
          ><div
            v-for="panel in p.panels.filter(
              (x: any) => x.slot === 'settings.plugins',
            )"
            :key="panel.id"
            class="detail-card"
          >
            <h3>{{ panel.title }}</h3>
            <p class="preserve">{{ panel.text }}</p>
          </div></template
        >
        <div v-for="p in plugins" :key="p.id" class="detail-card">
          <h3>
            {{ p.id }} <span class="pill">{{ p.status }}</span>
          </h3>
          <p>
            Version {{ p.version }} · {{ p.manifest.permissions.join(", ") }}
          </p>
          <div class="inline-actions">
            <Button
              variant="outline"
              @click="
                run(async () => {
                  await api(
                    `/plugins/${p.id}/${p.enabled ? 'disable' : 'enable'}`,
                    'POST',
                    {},
                  );
                  plugins = await api('/plugins');
                })
              "
              >{{ p.enabled ? "Deaktivieren" : "Aktivieren" }}</Button
            ><Button
              variant="ghost"
              @click="
                run(async () => {
                  await api(`/plugins/${p.id}/uninstall`, 'POST', {});
                  plugins = await api('/plugins');
                })
              "
              >Deinstallieren</Button
            >
          </div>
        </div></template
      >
      <template v-else-if="section === 'settings'"
        ><div class="settings-grid">
          <section class="detail-card">
            <h3>Geräteverwaltung</h3>
            <p>
              Die Reihenfolge gilt für die Bildschirme, Tabs und Seitenleiste.
            </p>
            <div v-for="d in ordered" :key="d.device_id" class="manage-row">
              <Button
                variant="ghost"
                size="sm"
                :disabled="allOrdered[0]?.device_id === d.device_id"
                :aria-label="d.name + ' nach vorne'"
                @click="reorder(d.device_id, -1)"
                >↑</Button
              >
              <Button
                variant="ghost"
                size="sm"
                :disabled="allOrdered.at(-1)?.device_id === d.device_id"
                :aria-label="d.name + ' nach hinten'"
                @click="reorder(d.device_id, 1)"
                >↓</Button
              >
              <span
                >{{ d.name }}<small>{{ d.device_id }}</small></span
              ><Button variant="ghost" size="sm" @click="edit(d)"
                >Bearbeiten</Button
              ><Button variant="ghost" size="sm" @click="remove(d)"
                >Entfernen</Button
              >
            </div>
          </section>
          <section v-if="can('tokens:manage')" class="detail-card">
            <h3>Personal Access Tokens</h3>
            <div v-for="t in tokens" :key="t.id" class="manage-row">
              <span
                >{{ t.name
                }}<small>{{
                  t.revoked ? "Widerrufen" : t.scopes.join(", ")
                }}</small></span
              ><Button
                variant="ghost"
                :disabled="t.revoked"
                @click="
                  run(async () => {
                    await api('/tokens/' + t.id, 'DELETE');
                    tokens = await api('/tokens');
                  })
                "
                >Widerrufen</Button
              >
            </div>
            <hr />
            <h4>Token erstellen</h4>
            <label>Name<input v-model="tokenName" /></label
            ><label
              >Geräte-IDs (Komma getrennt, * für alle)<input
                v-model="tokenDevices"
            /></label>
            <div class="scope-grid">
              <label v-for="s in allScopes" :key="s"
                ><input v-model="tokenScopes" type="checkbox" :value="s" />{{
                  s
                }}</label
              >
            </div>
            <Button
              @click="
                run(async () => {
                  const r = await api('/tokens', 'POST', {
                    name: tokenName,
                    scopes: tokenScopes,
                    devices: tokenDevices.split(',').map((x) => x.trim()),
                  });
                  newToken = r.token;
                  tokens = await api('/tokens');
                })
              "
              >Token erstellen</Button
            >
            <div v-if="newToken" class="notice">
              <strong>Nur einmal sichtbar – jetzt sicher ablegen.</strong
              ><textarea
                readonly
                :value="newToken"
                aria-label="Neuer Token"
              /><Button variant="ghost" @click="newToken = ''"
                >Ausblenden</Button
              >
            </div>
          </section>
          <section class="detail-card">
            <h3>Installation</h3>
            <p>KVMHelm 0.1 · POC</p>
            <Button
              variant="outline"
              @click="
                run(async () => {
                  diagnostics = await api('/diagnostics');
                })
              "
              >Diagnose laden</Button
            >
            <pre v-if="diagnostics">{{
              JSON.stringify(diagnostics, null, 2)
            }}</pre>
            <Button
              variant="ghost"
              @click="
                run(async () => {
                  try {
                    await api('/logout', 'POST', {});
                  } finally {
                    rememberToken('');
                    logged = false;
                    controller?.abort();
                  }
                })
              "
              >Abmelden</Button
            >
          </section>
        </div></template
      >
    </main>
  </div>
  <div v-if="modal" class="modal-backdrop" @click.self="modal = false">
    <form class="modal" @submit.prevent="saveDevice">
      <div class="section-heading">
        <h2>
          {{
            editing.device_id ? "Computer bearbeiten" : "Computer hinzufügen"
          }}
        </h2>
        <Button
          type="button"
          variant="ghost"
          @click="modal = false"
          aria-label="Dialog schließen"
          >×</Button
        >
      </div>
      <label
        >Name<input v-model="editing.name" required maxlength="100" autofocus
      /></label>
      <div class="form-row">
        <label
          >Treiber<select v-model="editing.driver_id" @change="changeDriver">
            <option value="pikvm-v4">PiKVM v4</option>
            <option value="jetkvm">JetKVM</option>
            <option value="glinet-comet">GL.iNet Comet</option>
            <option value="simulator">Simulator (synthetisch)</option>
          </select></label
        ><label
          >Modell<select v-model="editing.model">
            <option
              v-for="model in driverModels[editing.driver_id]"
              :key="model"
            >
              {{ model }}
            </option>
          </select></label
        >
      </div>
      <label
        >Minimum Action Latency (ms)<input
          v-model.number="editing.minimum_action_latency_ms"
          type="number"
          min="0"
          max="5000"
          step="1"
          required
      /></label>
      <p>
        Mindestens dieser Abstand zwischen letzter Eingabe und neuem Bild.
        Bereits verstrichene Zeit zählt mit; weitere Bildabfragen starten keine
        neue Pause.
      </p>
      <label v-if="editing.driver_id !== 'simulator'"
        >Geräteadresse<input
          v-model="editing.address"
          placeholder="https://192.168.1.50"
          required
      /></label>
      <div v-if="editing.driver_id !== 'simulator'" class="form-row">
        <label v-if="editing.driver_id !== 'jetkvm'"
          >Benutzername<input v-model="username" autocomplete="off" /></label
        ><label
          >Passwort {{ editing.secret_ref ? "(leer = beibehalten)" : ""
          }}<input v-model="secret" type="password" autocomplete="new-password"
        /></label>
      </div>
      <div class="form-row">
        <label
          >Tastaturlayout<select v-model="editing.keyboard_layout">
            <option value="de">Deutsch</option>
            <option value="us">English (US)</option>
          </select></label
        ><label
          >Medienprofil<select v-model="editing.media_profile">
            <option value="auto">Auto · gemessener Bildpfad</option>
            <option value="active">Aktiv</option>
            <option value="overview">Übersicht</option>
            <option value="warm">Warm</option>
          </select></label
        >
      </div>
      <label
        >Tags<input
          :value="editing.tags.join(', ')"
          @input="
            editing.tags = ($event.target as HTMLInputElement).value
              .split(',')
              .map((x) => x.trim())
              .filter(Boolean)
          " /></label
      ><label v-if="editing.driver_id !== 'simulator'"
        >Bestätigter SHA-256-Zertifikatsfingerprint (optional)<input
          v-model="editing.tls.fingerprint"
          placeholder="Normale Zertifikatsprüfung, wenn leer"
      /></label>
      <template v-if="editing.driver_id !== 'simulator'">
        <div class="form-row">
          <Button type="button" variant="outline" @click="rememberCertificate"
            >Aktuelles Zertifikat merken</Button
          >
          <Button
            type="button"
            variant="ghost"
            @click="
              editing.tls.fingerprint = '';
              editing.tls.insecure = false;
            "
            >Strikt prüfen</Button
          >
        </div>
        <label class="inline-label"
          ><input
            v-model="editing.tls.insecure"
            type="checkbox"
            @change="editing.tls.insecure && (editing.tls.fingerprint = '')"
          />Zertifikatsprüfung für dieses Gerät ignorieren</label
        >
        <p v-if="editing.tls.insecure" class="error">
          Die Verbindung bleibt verschlüsselt, die Identität des Geräts wird
          jedoch nicht geprüft.
        </p>
        <p v-else-if="editing.tls.fingerprint">
          Dieses Zertifikat wird nach dem Speichern fest gebunden. Ein
          Zertifikatswechsel wird abgewiesen.
        </p>
        <label
          >Eigene CA (PEM, leer = beibehalten)<textarea
            v-model="caPem"
            rows="3"
            placeholder="-----BEGIN CERTIFICATE-----"
          />
        </label>
        <label v-if="editing.secret_ref" class="inline-label"
          ><input v-model="clearCredentials" type="checkbox" />Gespeicherte
          Gerätezugangsdaten löschen</label
        >
        <label v-if="editing.tls.ca_ref" class="inline-label"
          ><input v-model="clearCa" type="checkbox" />Gespeicherte CA
          entfernen</label
        >
      </template>
      <label class="inline-label"
        ><input v-model="editing.enabled" type="checkbox" />Gerät
        aktiviert</label
      >
      <p v-if="error" class="error">{{ error }}</p>
      <div class="modal-actions">
        <Button type="button" variant="ghost" @click="modal = false"
          >Abbrechen</Button
        ><Button>Speichern</Button>
      </div>
    </form>
  </div>
</template>
