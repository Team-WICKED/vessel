// Mutual proof over role-separated transcripts. Fresh nonces bind every
// welcome to this socket, so replaying a previous welcome cannot open it.
import type { ClientFrame, ServerFrame } from "../wire/bridge";
import type { Credential } from "./token";

const HEX = /^[0-9a-f]{64}$/;
const text = new TextEncoder();
function hex(bytes: Uint8Array): string {
  return Array.from(bytes, (byte) => byte.toString(16).padStart(2, "0")).join(
    "",
  );
}
async function mac(key: string, transcript: string): Promise<string> {
  const imported = await crypto.subtle.importKey(
    "raw",
    text.encode(key),
    { name: "HMAC", hash: "SHA-256" },
    false,
    ["sign"],
  );
  return hex(
    new Uint8Array(
      await crypto.subtle.sign("HMAC", imported, text.encode(transcript)),
    ),
  );
}

type Challenge = Extract<ServerFrame, { type: "challenge" }>;
type Welcome = Extract<ServerFrame, { type: "welcome" }>;
export class Handshake {
  readonly #credential: Credential;
  readonly #client = hex(crypto.getRandomValues(new Uint8Array(32)));
  #challenge: Challenge | null = null;
  constructor(credential: Credential) {
    this.#credential = credential;
  }

  async hello(challenge: Challenge): Promise<ClientFrame> {
    if (
      this.#challenge !== null ||
      typeof challenge.launch !== "string" ||
      typeof challenge.nonce !== "string" ||
      !HEX.test(challenge.launch) ||
      !HEX.test(challenge.nonce)
    )
      throw new Error("Invalid challenge");
    if (
      this.#credential.kind === "session" &&
      this.#credential.launch !== challenge.launch
    )
      throw new Error("The UI was restarted");
    this.#challenge = challenge;
    return {
      type: "hello",
      mode: this.#credential.kind === "bootstrap" ? "bootstrap" : "resume",
      nonce: this.#client,
      proof: await mac(
        this.#credential.key,
        `vessel-ui-v1\nclient\n${challenge.launch}\n${challenge.nonce}\n${this.#client}\n`,
      ),
    };
  }

  async welcome(frame: Welcome): Promise<Credential> {
    const challenge = this.#challenge;
    if (challenge === null) throw new Error("Missing challenge");
    if (
      frame.launch !== challenge.launch ||
      typeof frame.proof !== "string" ||
      !HEX.test(frame.proof) ||
      typeof frame.version !== "string" ||
      frame.version.includes("\n")
    )
      throw new Error("Invalid welcome");
    const bootstrap = this.#credential.kind === "bootstrap";
    if (
      bootstrap
        ? typeof frame.session !== "string" || !HEX.test(frame.session)
        : frame.session !== null
    )
      throw new Error("Invalid session");
    const expected = await mac(
      this.#credential.key,
      `vessel-ui-v1\nserver\n${challenge.launch}\n${challenge.nonce}\n${this.#client}\n${frame.version}\n${frame.session ?? ""}`,
    );
    if (expected !== frame.proof) throw new Error("Server proof failed");
    this.#challenge = null;
    return {
      kind: "session",
      launch: frame.launch,
      key: frame.session ?? this.#credential.key,
    };
  }
}
