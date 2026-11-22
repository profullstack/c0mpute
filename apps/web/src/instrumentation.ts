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
  // Node earnings history for /dashboard/nodes: copy credited jobs out of the
  // aggregators before their in-memory 24h window forgets them.
  try {
    const { startRecorder } = await import("./lib/earnings/recorder");
    startRecorder();
  } catch (e) {
    console.error("node earnings: could not start the recorder", e);
  }
  // Disk (hosting) dues for /dashboard/disk: copy each finished hour out of
  // the gateways before their receipts age out.
  try {
    const { startDiskRecorder } = await import("./lib/disk/recorder");
    startDiskRecorder();
  } catch (e) {
    console.error("disk dues: could not start the recorder", e);
  }
  // Gallery "Get listed": badge hit counts, the 30-day free queue and the
  // weekly badge re-checks.
  try {
    const { startGalleryTicker } = await import("./lib/gallery/server");
    startGalleryTicker();
  } catch (e) {
    console.error("gallery: could not start the ticker", e);
  }
}
