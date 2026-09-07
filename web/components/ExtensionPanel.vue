<script setup lang="ts">
import { ref, watch, nextTick } from "vue";
import type { PluginPanel } from "../../shared/contracts";
import { api } from "../api";
import Button from "./ui/button/Button.vue";
const props = defineProps<{
  panel: PluginPanel;
  pluginId: string;
  editable?: boolean;
}>();
const values = ref<Record<string, string>>({}),
  dirty = ref(false),
  busy = ref(false),
  error = ref(""),
  follow = ref(true),
  log = ref<HTMLElement>();
function reset() {
  if (dirty.value) return;
  values.value = Object.fromEntries(
    (props.panel.fields ?? []).map((f) => [
      f.id,
      Array.isArray(f.value) ? f.value.join("\n") : String(f.value),
    ]),
  );
}
watch(() => props.panel.fields, reset, { immediate: true, deep: true });
watch(
  () => props.panel.text,
  async () => {
    if (follow.value) {
      await nextTick();
      if (log.value) log.value.scrollTop = log.value.scrollHeight;
    }
  },
  { immediate: true },
);
async function action(id: string) {
  busy.value = true;
  error.value = "";
  try {
    const fields = Object.fromEntries(
      (props.panel.fields ?? []).map((f) => [
        f.id,
        f.type === "string-list"
          ? (values.value[f.id] ?? "")
              .split("\n")
              .map((s) => s.trim())
              .filter(Boolean)
          : Number(values.value[f.id]),
      ]),
    );
    await api(
      `/plugins/${encodeURIComponent(props.pluginId)}/panels/${encodeURIComponent(props.panel.id)}/actions`,
      "POST",
      { action: id, values: fields },
    );
    dirty.value = false;
  } catch (e) {
    error.value = (e as Error).message;
  } finally {
    busy.value = false;
  }
}
</script>
<template>
  <article class="extension-panel">
    <header>
      <strong>{{ panel.title }}</strong
      ><label><input v-model="follow" type="checkbox" />Mitlaufen</label>
    </header>
    <pre
      ref="log"
      class="extension-log"
      tabindex="0"
      :aria-label="panel.title"
      >{{ panel.text }}</pre>
    <details v-if="panel.fields?.length && editable">
      <summary>Einstellungen</summary>
      <label
        v-for="field in panel.fields"
        :key="field.id"
        class="extension-field"
        >{{ field.label
        }}<textarea
          v-if="field.type === 'string-list'"
          v-model="values[field.id]"
          rows="3"
          @input="dirty = true" /><input
          v-else
          v-model="values[field.id]"
          type="number"
          @input="dirty = true" /></label
      ><small v-if="dirty">Ungespeicherte Änderungen</small>
    </details>
    <div v-if="editable" class="extension-actions">
      <Button
        v-for="a in panel.actions"
        :key="a.id"
        size="sm"
        variant="outline"
        :disabled="busy || a.disabled"
        @click="action(a.id)"
        >{{ a.label }}</Button
      >
    </div>
    <p v-if="error" class="error" role="alert">{{ error }}</p>
  </article>
</template>
