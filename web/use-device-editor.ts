import { ref } from "vue";
import { api } from "./api";
import type { Device, DeviceView } from "../shared/contracts";
export function useDeviceEditor(
  run: (fn: () => Promise<unknown>) => Promise<unknown>,
  refresh: () => Promise<void>,
) {
  type EditableDevice = Omit<Device, "device_id"> & { device_id?: string };
  const defaults = (): EditableDevice => ({
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
  });
  const editing = ref<EditableDevice>(defaults());
  const modal = ref(false),
    secret = ref(""),
    username = ref("admin");
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
  function edit(d?: DeviceView) {
    editing.value = d
      ? (Object.fromEntries(
          Object.keys(defaults())
            .concat(["device_id", "secret_ref"])
            .map((key) => [key, d[key as keyof DeviceView]]),
        ) as EditableDevice)
      : defaults();
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
      const cert = await api<{ fingerprint: string }>(
        "/certificates/inspect",
        "POST",
        {
          address: editing.value.address,
        },
      );
      editing.value.tls.fingerprint = cert.fingerprint;
      editing.value.tls.insecure = false;
    });
  }
  async function remove(d: DeviceView) {
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
  return {
    editing,
    driverModels,
    changeDriver,
    caPem,
    clearCredentials,
    clearCa,
    modal,
    secret,
    username,
    edit,
    saveDevice,
    rememberCertificate,
    remove,
  };
}
