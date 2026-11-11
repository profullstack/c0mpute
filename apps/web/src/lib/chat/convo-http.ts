import { ConvoError } from "./convo-store";
import { json } from "./server";

/** A ConvoError as its HTTP answer (409 carries the newer server copy). */
export function convoErr(e: unknown) {
  if (e instanceof ConvoError) {
    return json({ error: { message: e.message, code: e.status }, ...(e.conversation ? { conversation: e.conversation } : {}) }, e.status);
  }
  console.error("chat conversations", e);
  return json({ error: { message: "conversation storage failed", code: 500 } }, 500);
}
