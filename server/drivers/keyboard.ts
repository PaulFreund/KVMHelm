import { GatewayError } from "../../shared/contracts.js";
const aliases: Record<string, string> = {
  CTRL: "ControlLeft",
  CONTROL: "ControlLeft",
  SHIFT: "ShiftLeft",
  ALT: "AltLeft",
  ALTGR: "AltRight",
  META: "MetaLeft",
  SUPER: "MetaLeft",
  WIN: "MetaLeft",
  ENTER: "Enter",
  RETURN: "Enter",
  ESC: "Escape",
  ESCAPE: "Escape",
  TAB: "Tab",
  SPACE: "Space",
  BACKSPACE: "Backspace",
  DELETE: "Delete",
  UP: "ArrowUp",
  DOWN: "ArrowDown",
  LEFT: "ArrowLeft",
  RIGHT: "ArrowRight",
  HOME: "Home",
  END: "End",
  PAGEUP: "PageUp",
  PAGEDOWN: "PageDown",
  INSERT: "Insert",
  CAPSLOCK: "CapsLock",
  PRINTSCREEN: "PrintScreen",
};
export function keyName(k: string) {
  if (aliases[k.toUpperCase()]) return aliases[k.toUpperCase()];
  if (/^[a-z]$/i.test(k)) return "Key" + k.toUpperCase();
  if (/^\d$/.test(k)) return "Digit" + k;
  if (
    /^(Key[A-Z]|Digit\d|F([1-9]|1[0-9]|2[0-4])|Control(Left|Right)|Shift(Left|Right)|Alt(Left|Right)|Meta(Left|Right)|Arrow(Up|Down|Left|Right)|Enter|Escape|Tab|Space|Backspace|Delete|Home|End|PageUp|PageDown|Insert|CapsLock|PrintScreen|Minus|Equal|BracketLeft|BracketRight|Backslash|Semicolon|Quote|Backquote|Comma|Period|Slash|IntlBackslash)$/.test(
      k,
    )
  )
    return k;
  throw new GatewayError("UNSUPPORTED_ACTION", `Unknown key: ${k}`);
}
export function textKeys(text: string, layout: "us" | "de"): string[][] {
  const out: string[][] = [];
  const digits = "0123456789";
  const us: Record<string, string[]> = {};
  const base = "`-=[]\\;',./";
  const codes = [
    "Backquote",
    "Minus",
    "Equal",
    "BracketLeft",
    "BracketRight",
    "Backslash",
    "Semicolon",
    "Quote",
    "Comma",
    "Period",
    "Slash",
  ];
  [...base].forEach((c, i) => (us[c] = [codes[i]]));
  [...'~_+{}|:"<>?'].forEach((c, i) => (us[c] = ["ShiftLeft", codes[i]]));
  [...")!@#$%^&*("].forEach((c, i) => (us[c] = ["ShiftLeft", "Digit" + i]));
  const de: Record<string, string[]> = {
    ß: ["Minus"],
    "?": ["ShiftLeft", "Minus"],
    "´": ["Equal"],
    "+": ["BracketRight"],
    "*": ["ShiftLeft", "BracketRight"],
    ü: ["BracketLeft"],
    Ü: ["ShiftLeft", "BracketLeft"],
    ö: ["Semicolon"],
    Ö: ["ShiftLeft", "Semicolon"],
    ä: ["Quote"],
    Ä: ["ShiftLeft", "Quote"],
    "#": ["Backslash"],
    "'": ["ShiftLeft", "Backslash"],
    "<": ["IntlBackslash"],
    ">": ["ShiftLeft", "IntlBackslash"],
    "|": ["AltRight", "IntlBackslash"],
    ",": ["Comma"],
    ";": ["ShiftLeft", "Comma"],
    ".": ["Period"],
    ":": ["ShiftLeft", "Period"],
    "-": ["Slash"],
    _: ["ShiftLeft", "Slash"],
    "@": ["AltRight", "KeyQ"],
    "€": ["AltRight", "KeyE"],
    "{": ["AltRight", "Digit7"],
    "[": ["AltRight", "Digit8"],
    "]": ["AltRight", "Digit9"],
    "}": ["AltRight", "Digit0"],
    "\\": ["AltRight", "Minus"],
    "~": ["AltRight", "BracketRight"],
  };
  [...'=!"§$%&/()'].forEach((c, i) => (de[c] = ["ShiftLeft", "Digit" + i]));
  for (const c of text) {
    if (c === "\r") continue;
    if (layout === "de" && ["´", "`", "^"].includes(c)) {
      out.push(
        c === "´"
          ? ["Equal"]
          : c === "`"
            ? ["ShiftLeft", "Equal"]
            : ["Backquote"],
        ["Space"],
      );
      continue;
    }
    if (c === "\n") {
      out.push(["Enter"]);
      continue;
    }
    if (c === "\t") {
      out.push(["Tab"]);
      continue;
    }
    if (c === " ") {
      out.push(["Space"]);
      continue;
    }
    if (/[a-z]/i.test(c) && c.length === 1) {
      let key = c.toUpperCase();
      if (layout === "de") key = key === "Y" ? "Z" : key === "Z" ? "Y" : key;
      out.push(
        c === c.toUpperCase() ? ["ShiftLeft", "Key" + key] : ["Key" + key],
      );
      continue;
    }
    if (digits.includes(c)) {
      out.push(["Digit" + c]);
      continue;
    }
    const chord = (layout === "de" ? de : us)[c];
    if (!chord)
      throw new GatewayError(
        "UNSUPPORTED_TEXT",
        `Character U+${c.codePointAt(0)!.toString(16).toUpperCase()} is unavailable for ${layout}`,
      );
    out.push(chord);
  }
  return out;
}
