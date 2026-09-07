import { GatewayError } from "../../shared/contracts.js";
import { keyName } from "./keyboard.js";
const special: Record<string, number> = {
  Enter: 40,
  Escape: 41,
  Backspace: 42,
  Tab: 43,
  Space: 44,
  Minus: 45,
  Equal: 46,
  BracketLeft: 47,
  BracketRight: 48,
  Backslash: 49,
  Semicolon: 51,
  Quote: 52,
  Backquote: 53,
  Comma: 54,
  Period: 55,
  Slash: 56,
  CapsLock: 57,
  PrintScreen: 70,
  Insert: 73,
  Home: 74,
  PageUp: 75,
  Delete: 76,
  End: 77,
  PageDown: 78,
  ArrowRight: 79,
  ArrowLeft: 80,
  ArrowDown: 81,
  ArrowUp: 82,
  IntlBackslash: 100,
};
const modifiers = [
  "ControlLeft",
  "ShiftLeft",
  "AltLeft",
  "MetaLeft",
  "ControlRight",
  "ShiftRight",
  "AltRight",
  "MetaRight",
];
export function usbKey(name: string): number {
  const key = keyName(name),
    mod = modifiers.indexOf(key);
  if (mod >= 0) return 224 + mod;
  if (/^Key[A-Z]$/.test(key)) return key.charCodeAt(3) - 65 + 4;
  if (/^Digit\d$/.test(key))
    return key === "Digit0" ? 39 : 29 + Number(key.slice(5));
  if (/^F\d+$/.test(key)) {
    const n = Number(key.slice(1));
    return n <= 12 ? 57 + n : 91 + n;
  }
  if (special[key] !== undefined) return special[key];
  throw new GatewayError("UNSUPPORTED_ACTION", "Unsupported USB key: " + key);
}
