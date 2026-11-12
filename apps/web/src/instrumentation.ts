// Runs once when this Next server starts. Starts the chat run manager so
// answers that a previous server (killed by a deploy) was still generating are
// taken over and finished here, without waiting for anyone to reconnect.
export async function register() {
  if (process.env.NEXT_RUNTIME !== "nodejs") return;
  try {
    const { runManager } = await import("./lib/chat/run-manager");
    runManager();
  } catch (e) {
    console.error("chat runs: could not start the run manager", e);
  }
}
