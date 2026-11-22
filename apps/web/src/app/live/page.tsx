import Link from "next/link";
import { liveStore } from "@/lib/chat/server";
import { FREE_MAX_HEIGHT, FREE_MAX_VIEWERS, LIVE_RTMP_URL, LIVE_USD_PER_GB } from "@/lib/live/store";

export const metadata = {
  title: "c0mpute Live — peer-to-peer live streaming",
  description: "Push RTMP from OBS or PairUX and c0mpute nodes relay your stream to any HLS player. Relays are paid per GB; no platform cut of your audience.",
};

export const dynamic = "force-dynamic";

export default async function LiveIndex() {
  const onAir = await liveStore()
    .onAir(30)
    .catch(() => []);
  return (
    <div className="max-w-3xl mx-auto px-4 sm:px-6 py-12 space-y-8">
      <header className="space-y-2">
        <h1 className="text-2xl font-bold accent">c0mpute Live</h1>
        <p className="comment">// live streaming carried by the network's nodes, not a CDN</p>
      </header>

      <section className="space-y-3 text-sm">
        <p>
          Create a stream, point OBS or PairUX at <code>{LIVE_RTMP_URL}</code> with your stream key, and share the watch link. Nodes relay the video to each other and to viewers; it plays in a browser
          and in VLC, mpv or ffplay.
        </p>
        <ul className="list-disc pl-5 space-y-1">
          <li>
            Relays are paid <b>${LIVE_USD_PER_GB} per GB</b> they carry, out of your c0mpute balance. Operators keep 95%.
          </li>
          <li>
            No balance: the stream still runs, up to {FREE_MAX_HEIGHT}p and {FREE_MAX_VIEWERS} viewers at once.
          </li>
          <li>Every node relays live streams unless its operator turns it off, so there is nothing to opt in to.</li>
        </ul>
        <p>
          <Link href="/dashboard/live" className="!border border-[var(--color-accent)] accent px-3 py-1 inline-block">
            create a stream
          </Link>{" "}
          <Link href="/getting-started" className="ml-3">
            run a node and get paid to relay
          </Link>
        </p>
      </section>

      <section className="space-y-2 text-sm">
        <h2 className="font-bold">on air now</h2>
        {onAir.length === 0 && <p className="comment">// nothing public is live right now</p>}
        <ul className="space-y-1">
          {onAir.map((s) => (
            <li key={s.id}>
              <Link href={`/live/${s.id}`}>{s.title}</Link>
            </li>
          ))}
        </ul>
      </section>

      <section className="space-y-2 text-sm">
        <h2 className="font-bold">API</h2>
        <p>
          <code>POST /api/v1/live/streams</code> with an API key, or an OAuth token with the <code>live:publish</code> scope, returns the stream key and watch link. Apps connect through the same
          OAuth server as the MCP connector.
        </p>
      </section>
    </div>
  );
}
