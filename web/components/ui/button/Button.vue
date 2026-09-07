<script setup lang="ts">
import { Primitive, type PrimitiveProps } from "reka-ui";
import { cva } from "class-variance-authority";
import { clsx } from "clsx";
import { twMerge } from "tailwind-merge";
interface Props extends PrimitiveProps {
  variant?: "default" | "outline" | "ghost" | "destructive";
  size?: "default" | "sm" | "icon";
  class?: string;
}
const props = withDefaults(defineProps<Props>(), {
  as: "button",
  variant: "default",
  size: "default",
});
const variants = cva("button", {
  variants: {
    variant: {
      default: "button-primary",
      outline: "button-outline",
      ghost: "button-ghost",
      destructive: "button-danger",
    },
    size: { default: "", sm: "button-sm", icon: "button-icon" },
  },
});
</script>
<template>
  <Primitive
    :as="as"
    :as-child="asChild"
    :class="twMerge(clsx(variants({ variant, size }), props.class))"
    ><slot
  /></Primitive>
</template>
