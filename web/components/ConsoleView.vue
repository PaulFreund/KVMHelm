<script setup lang="ts">
import { computed, ref } from "vue";
import Screen from "./Screen.vue";
import ExtensionPanel from "./ExtensionPanel.vue";
import Button from "./ui/button/Button.vue";
import type { DeviceView, ExtensionView } from "../../shared/contracts";
const props = defineProps<{
  device: DeviceView;
  extensions: ExtensionView[];
  overview?: boolean;
  editable?: boolean;
}>();
const emit = defineEmits(["refresh"]);
const key = "kvmhelm.panels." + props.device.device_id;
const shown = ref(localStorage.getItem(key) === "true");
const panels = computed(() =>
  props.extensions.flatMap((p) =>
    p.panels
      .filter(
        (x) =>
          x.slot === "kvm.sidepanel" && x.device_id === props.device.device_id,
      )
      .map((panel) => ({ plugin: p.id, status: p.status, panel })),
  ),
);
function toggle() {
  shown.value = !shown.value;
  localStorage.setItem(key, String(shown.value));
}
</script>
<template>
  <div class="console-extensions">
    <div v-if="panels.length" class="extension-toggle">
      <Button size="sm" variant="outline" :aria-expanded="shown" @click="toggle"
        >{{ shown ? "Seitenpanel ausblenden" : "Seitenpanel anzeigen" }} ·
        {{ panels.length }}</Button
      >
    </div>
    <div
      class="console-with-panels"
      :class="{ 'panels-visible': shown && panels.length }"
    >
      <Screen
        :device="device"
        :overview="overview"
        @refresh="emit('refresh')"
      />
      <aside
        v-if="shown && panels.length"
        :aria-label="'Seitenpanel ' + device.name"
      >
        <div v-for="entry in panels" :key="entry.plugin + entry.panel.id">
          <p v-if="entry.status !== 'running'" class="notice">
            Erweiterung: {{ entry.status }} · Inhalte möglicherweise veraltet
          </p>
          <ExtensionPanel
            :panel="entry.panel"
            :plugin-id="entry.plugin"
            :editable="editable"
          />
        </div>
      </aside>
    </div>
  </div>
</template>
