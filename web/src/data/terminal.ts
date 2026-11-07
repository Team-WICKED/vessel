// Terminal bytes on the wire are base64 text (`TerminalBytes`); xterm.js
// takes bytes, and input goes back as base64 of UTF-8.

/** The bytes of base64 `text`. */
export function decodeBytes(text: string): Uint8Array {
  const binary = window.atob(text);
  const bytes = new Uint8Array(binary.length);
  for (let index = 0; index < binary.length; index += 1) {
    bytes[index] = binary.charCodeAt(index);
  }
  return bytes;
}

/** `text` as UTF-8, in base64. */
export function encodeText(text: string): string {
  const bytes = new TextEncoder().encode(text);
  let binary = "";
  for (const byte of bytes) {
    binary += String.fromCharCode(byte);
  }
  return window.btoa(binary);
}
