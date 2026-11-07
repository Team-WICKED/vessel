// Typed calls over the generated wire types (ADR 0017): a method's
// parameters and result are looked up from the `Call` and `Reply` unions,
// so `call(node, "session.create", params)` returns a `SessionInfo`.
import type { Call, Reply } from "./proto";

/** Every method's wire name. */
export type MethodName = Call["method"];

/** What `method` takes. */
export type ParamsOf<M extends MethodName> = Extract<
  Call,
  { method: M }
>["params"];

/** What `method` answers. */
export type ResultOf<M extends MethodName> = Extract<
  Reply,
  { method: M }
>["result"];
