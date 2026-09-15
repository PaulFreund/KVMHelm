<script setup lang="ts">
import { ref, watch, nextTick, computed } from "vue";
import type { PluginPanel } from "../../shared/contracts";
import { api } from "../api";
import Button from "./ui/button/Button.vue";
const props = defineProps<{
  panel: PluginPanel;
  pluginId: string;
  editable?: boolean;
}>();
const values = ref<Record<string, string | string[]>>({}),
  dirty = ref(false),
  busy = ref(false),
  error = ref(""),
  follow = ref(true),
  log = ref<HTMLElement>();
const renderedText = computed(() => {
  const text = props.panel.text,
    ranges = [...(props.panel.highlights ?? [])]
      .filter(
        ({ start, end }) => start >= 0 && end > start && start < text.length,
      )
      .map(({ start, end }) => ({ start, end: Math.min(end, text.length) }))
      .sort((a, b) => a.start - b.start || a.end - b.end),
    merged: { start: number; end: number }[] = [];
  for (const range of ranges) {
    const previous = merged.at(-1);
    if (previous && range.start <= previous.end)
      previous.end = Math.max(previous.end, range.end);
    else merged.push({ ...range });
  }
  const parts: { text: string; highlighted: boolean }[] = [];
  let offset = 0;
  for (const range of merged) {
    if (range.start > offset)
      parts.push({ text: text.slice(offset, range.start), highlighted: false });
    parts.push({
      text: text.slice(range.start, range.end),
      highlighted: true,
    });
    offset = range.end;
  }
  if (offset < text.length)
    parts.push({ text: text.slice(offset), highlighted: false });
  return parts;
});
function reset() {
  if (dirty.value) return;
  values.value = Object.fromEntries(
    (props.panel.fields ?? []).map((f) => [
      f.id,
      Array.isArray(f.value)
        ? f.type === "multi-select"
          ? [...f.value]
          : f.value.join("\n")
        : String(f.value),
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
        f.type === "multi-select"
          ? Array.isArray(values.value[f.id])
            ? values.value[f.id]
            : []
          : f.type === "string-list"
            ? (values.value[f.id] ?? "")
                .toString()
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
    ><template v-for="(part, index) in renderedText" :key="index"
        ><mark v-if="part.highlighted">{{ part.text }}</mark
        ><template v-else>{{ part.text }}</template></template
      ></pre>
    <details v-if="panel.fields?.length && editable">
      <summary>Einstellungen</summary>
      <div
        v-for="field in panel.fields"
        :key="field.id"
        class="extension-field"
      >
        <fieldset v-if="field.type === 'multi-select'">
          <legend>{{ field.label }}</legend>
          <label v-for="option in field.options" :key="option.value">
            <input
              v-model="values[field.id]"
              type="checkbox"
              :value="option.value"
              @change="dirty = true"
            />{{ option.label }}</label
          >
        </fieldset>
        <label v-else
          >{{ field.label
          }}<textarea
            v-if="field.type === 'string-list'"
            v-model="values[field.id]"
            rows="3"
            @input="dirty = true" /><input
            v-else
            v-model="values[field.id]"
            type="number"
            @input="dirty = true"
        /></label>
      </div>
      <small v-if="dirty">Ungespeicherte Änderungen</small>
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
