"use client";

import { useState } from "react";
import {
  DEFAULT_GPU_MODEL,
  GPU_GROUPS,
  GPU_RATE_CHECKED_AT,
  GPU_RATE_REFERENCES,
  GPU_RATE_SOURCES,
  UTILIZATION_SCENARIOS,
} from "@/lib/gpu-estimates";
import {
  CPU_RATE_CHECKED_AT,
  CPU_RATE_REFERENCES,
  CPU_RATE_SOURCE,
  DEFAULT_CPU_MODEL,
} from "@/lib/cpu-estimates";
import { MARKET_FEE_PCT, OPERATOR_SHARE_PCT } from "@/lib/market";
import { R4CK_SITE, type LiveRates } from "@/lib/r4ck-rates";
import { asOf, cpuRateHint as cpuHintFor, gpuRateHint as gpuHintFor, hourly } from "@/lib/rate-hints";
import type { ObservedGroup, ObservedWindow } from "@/lib/earnings/network-by-class";

const DAYS_PER_MONTH = 30;
const OTHER_GPU = "__other__";
const CUSTOM_CPU = "__custom__";

type Hardware = "gpu" | "cpu";

function gpuRateFor(model: string, live: LiveRates | null): string {
  const r = live?.gpu[model]?.hourlyUsd;
  if (r != null) return String(r);
  const match = GPU_RATE_REFERENCES.find((gpu) => gpu.model === model);
  return match?.hourlyUsd != null ? String(match.hourlyUsd) : "";
}

function cpuRateFor(model: string, live: LiveRates | null): string {
  const r = live?.cpu[model]?.hourlyUsd;
  if (r != null) return String(r);
  const match = CPU_RATE_REFERENCES.find((cpu) => cpu.model === model);
  return match ? String(match.hourlyUsd) : "";
}

/** The observed-network class that matches what the visitor picked. */
function classFor(hardware: Hardware, gpuModel: string): string {
  if (hardware === "cpu") return "cpu";
  const group = GPU_RATE_REFERENCES.find((g) => g.model === gpuModel)?.group ?? "";
  if (group.startsWith("AMD")) return "gpu:amd";
  if (group === "Apple silicon") return "gpu:apple";
  return "gpu:nvidia";
}

function usd(value: number): string {
  return new Intl.NumberFormat("en-US", {
    style: "currency",
    currency: "USD",
    maximumFractionDigits: value < 10 ? 2 : 0,
  }).format(value);
}

/** What node operators actually earned (from /status earnings), for the page header and payback panel. */
export interface ObservedEarnings {
  earned30d: number;
  earned7d: number;
  earnedAll: number;
  earningNodes30d: number;
  medianNode30d: number;
  bestNode30d: number;
  /** Paid (billed) inference jobs per day over the last 7 days, all models. */
  paidJobsPerDay: number;
  /** Average gross a paid job billed over the last 7 days, USD. */
  avgPaidJobUsd: number;
}

/** The live network price of the house GPU model, from Infernet's public /api/v1/pricing. */
export interface GpuModelPrice {
  model: string;
  outPerMUsd: number;
}

/** Payback multiples shown in the panel: 1x pays the rent; 5x pays it and rents four more. */
export const PAYBACK_MULTIPLES = [1, 5] as const;

export function EarningsEstimator({
  jobsCompleted24h,
  cpuWorkersOnline,
  observed = null,
  gpuPrice = null,
  liveRates = null,
  observedByClass = null,
}: {
  jobsCompleted24h: number | null;
  cpuWorkersOnline: number | null;
  observed?: ObservedEarnings | null;
  gpuPrice?: GpuModelPrice | null;
  liveRates?: LiveRates | null;
  observedByClass?: { history_from: number | null; windows: ObservedWindow[] } | null;
}) {
  const [hardware, setHardware] = useState<Hardware>("gpu");
  const [gpuModel, setGpuModel] = useState(DEFAULT_GPU_MODEL);
  const [customGpuModel, setCustomGpuModel] = useState("");
  const [gpuHourlyRate, setGpuHourlyRate] = useState(gpuRateFor(DEFAULT_GPU_MODEL, liveRates));
  const [cpuReference, setCpuReference] = useState(DEFAULT_CPU_MODEL);
  const [cpuHourlyRate, setCpuHourlyRate] = useState(cpuRateFor(DEFAULT_CPU_MODEL, liveRates));
  const [observedDays, setObservedDays] = useState(7);
  const [cpuRateEdited, setCpuRateEdited] = useState(false);
  const [hoursAvailable, setHoursAvailable] = useState("24");

  const selectedGpu = GPU_RATE_REFERENCES.find((gpu) => gpu.model === gpuModel);
  const selectedCpu = CPU_RATE_REFERENCES.find((cpu) => cpu.model === cpuReference);
  const rate = Math.max(0, Number(hardware === "gpu" ? gpuHourlyRate : cpuHourlyRate) || 0);
  const hours = Math.max(0, Math.min(24, Number(hoursAvailable) || 0));

  function updateGpuModel(next: string) {
    setGpuModel(next);
    setGpuHourlyRate(gpuRateFor(next, liveRates));
  }

  function updateCpuModel(next: string) {
    setCpuReference(next);
    setCpuHourlyRate(cpuRateFor(next, liveRates));
    setCpuRateEdited(false);
  }

  const gpuRateHint = gpuHintFor(selectedGpu, liveRates);
  const cpuRateHint = cpuHintFor(selectedCpu, liveRates);
  const observedWindow = observedByClass?.windows.find((w) => w.days === observedDays) ?? null;
  const myClass = classFor(hardware, gpuModel);

  return (
    <section className="space-y-5 border border-[var(--color-rule)] bg-[var(--color-card)] rounded-md p-4 sm:p-6">
      <div className="flex gap-2" role="group" aria-label="Choose hardware type">
        {(["gpu", "cpu"] as const).map((type) => (
          <button
            key={type}
            type="button"
            aria-pressed={hardware === type}
            onClick={() => setHardware(type)}
            className={`rounded border px-4 py-2 text-sm font-semibold capitalize ${hardware === type ? "border-[var(--color-accent)] accent" : "border-[var(--color-rule)] text-[var(--color-dim)]"}`}
          >
            {type}
          </button>
        ))}
      </div>

      <div className="grid gap-4 sm:grid-cols-2">
        {hardware === "gpu" ? (
          <>
            <label className="block space-y-2 text-sm">
              <span className="text-[var(--color-fg)]">GPU model</span>
              <select
                aria-label="GPU model"
                value={gpuModel}
                onChange={(event) => updateGpuModel(event.target.value)}
                className="w-full rounded border border-[var(--color-rule)] bg-[var(--color-bg)] px-3 py-2 text-[var(--color-fg)] outline-none focus:border-[var(--color-accent)]"
              >
                {GPU_GROUPS.map((group) => (
                  <optgroup key={group} label={group}>
                    {GPU_RATE_REFERENCES.filter((gpu) => gpu.group === group).map((gpu) => (
                      <option key={gpu.model} value={gpu.model}>{`${gpu.model} · ${gpu.vram}${liveRates?.gpu[gpu.model] ? ` · ${hourly(liveRates.gpu[gpu.model].hourlyUsd)}` : ""}`}</option>
                    ))}
                  </optgroup>
                ))}
                <option value={OTHER_GPU}>Other GPU (enter your own rate)</option>
              </select>
              {gpuModel === OTHER_GPU && (
                <input
                  aria-label="Your GPU model"
                  value={customGpuModel}
                  onChange={(event) => setCustomGpuModel(event.target.value)}
                  placeholder="Your GPU, e.g. RTX 2080 Ti"
                  className="w-full rounded border border-[var(--color-rule)] bg-[var(--color-bg)] px-3 py-2 text-[var(--color-fg)] outline-none focus:border-[var(--color-accent)]"
                />
              )}
            </label>

            <label className="block space-y-2 text-sm">
              <span className="text-[var(--color-fg)]">Comparable GPU price per hour (USD)</span>
              <input
                aria-label="Comparable GPU price per hour"
                type="number"
                min="0"
                step="0.01"
                inputMode="decimal"
                value={gpuHourlyRate}
                onChange={(event) => setGpuHourlyRate(event.target.value)}
                placeholder="Enter your own comparison rate"
                className="w-full rounded border border-[var(--color-rule)] bg-[var(--color-bg)] px-3 py-2 text-[var(--color-fg)] outline-none focus:border-[var(--color-accent)]"
              />
              <span className="block text-xs text-[var(--color-dim)]">
                {gpuRateHint} This is an external rental-price benchmark.
              </span>
            </label>
          </>
        ) : (
          <>
            <label className="block space-y-2 text-sm">
              <span className="text-[var(--color-fg)]">CPU cloud VM reference</span>
              <select
                aria-label="CPU cloud VM reference"
                value={cpuReference}
                onChange={(event) => updateCpuModel(event.target.value)}
                className="w-full rounded border border-[var(--color-rule)] bg-[var(--color-bg)] px-3 py-2 text-[var(--color-fg)] outline-none focus:border-[var(--color-accent)]"
              >
                {CPU_RATE_REFERENCES.map((cpu) => (
                  <option key={cpu.model} value={cpu.model}>{`${cpu.model} · ${hourly(Number(cpuRateFor(cpu.model, liveRates)))}`}</option>
                ))}
                <option value={CUSTOM_CPU}>Other CPU VM (enter your own rate)</option>
              </select>
            </label>

            <label className="block space-y-2 text-sm">
              <span className="text-[var(--color-fg)]">Comparable CPU VM price per hour (USD)</span>
              <input
                aria-label="Comparable CPU VM price per hour"
                type="number"
                min="0"
                step="0.00001"
                inputMode="decimal"
                value={cpuHourlyRate}
                onChange={(event) => {
                  setCpuHourlyRate(event.target.value);
                  setCpuRateEdited(true);
                }}
                placeholder="Enter your own comparison rate"
                className="w-full rounded border border-[var(--color-rule)] bg-[var(--color-bg)] px-3 py-2 text-[var(--color-fg)] outline-none focus:border-[var(--color-accent)]"
              />
              <span className="block text-xs text-[var(--color-dim)]">
                {cpuRateHint}{cpuRateEdited ? " Using your edited rate." : ""} VM prices include memory, storage, and transfer.
              </span>
            </label>
          </>
        )}

        <label className="block space-y-2 text-sm sm:col-span-2">
          <span className="text-[var(--color-fg)]">Hours per day your {hardware === "gpu" ? "GPU" : "CPU"} is available</span>
          <div className="flex items-center gap-3">
            <input
              aria-label="Hours per day available"
              type="range"
              min="1"
              max="24"
              value={hours || 1}
              onChange={(event) => setHoursAvailable(event.target.value)}
              className="w-full accent-[var(--color-accent)]"
            />
            <span className="w-16 text-right tabular-nums">{hours} h/day</span>
          </div>
        </label>
      </div>

      <div className="space-y-3">
        <h2 className="text-sm font-semibold text-[var(--color-fg)]">Illustrative monthly scenarios</h2>
        <p className="text-xs text-[var(--color-dim)] leading-6">
          Paid utilization means the share of available time occupied by paid work. These scenarios apply the comparison rate to that time for 30 days; they do not assume c0mpute has this demand.
        </p>
        <div className="overflow-x-auto">
          <table className="w-full border-collapse text-left text-xs">
            <thead className="text-[var(--color-dim)]">
              <tr className="border-b border-[var(--color-rule)]">
                <th className="py-2 pr-4 font-medium">paid utilization</th>
                <th className="py-2 pr-4 font-medium">paid hours / month</th>
                <th className="py-2 pr-4 font-medium">gross reference</th>
                <th className="py-2 font-medium">after assumed {MARKET_FEE_PCT}% fee</th>
              </tr>
            </thead>
            <tbody className="divide-y divide-[var(--color-rule)]">
              {UTILIZATION_SCENARIOS.map((utilization) => {
                const paidHours = hours * DAYS_PER_MONTH * utilization / 100;
                const gross = paidHours * rate;
                const operatorNet = gross * OPERATOR_SHARE_PCT / 100;
                return (
                  <tr key={utilization}>
                    <td className="py-3 pr-4">{utilization}%</td>
                    <td className="py-3 pr-4 tabular-nums">{paidHours.toFixed(0)} h</td>
                    <td className="py-3 pr-4 tabular-nums">{usd(gross)}</td>
                    <td className="py-3 accent font-bold tabular-nums">{usd(operatorNet)}</td>
                  </tr>
                );
              })}
            </tbody>
          </table>
        </div>
        <p className="text-xs text-[var(--color-dim)] leading-6">
          Direct peer-to-peer jobs have no c0mpute market fee, and the market fee is not collected until market settlement launches. The after-fee column assumes every dollar comes through the c0mpute market and excludes electricity, internet, hardware, tax, and other costs.
        </p>
      </div>

      {hardware === "gpu" && rate > 0 && (
        <div className="space-y-3">
          <h2 className="text-sm font-semibold text-[var(--color-fg)]">What a rented GPU has to sell to pay back</h2>
          <p className="text-xs text-[var(--color-dim)] leading-6">
            Renting this GPU at {usd(rate)}/h costs {usd(rate * 24)} a day. At <strong>1x</strong> the work it serves
            pays its own rent. At <strong>5x</strong> it pays its rent and the rent of four more GPUs, which is how one
            profitable GPU grows into five. Counted after the {MARKET_FEE_PCT}% house fee, before electricity and tax
            (a rented GPU has none of the first).
          </p>
          <div className="overflow-x-auto">
            <table className="w-full border-collapse text-left text-xs">
              <thead className="text-[var(--color-dim)]">
                <tr className="border-b border-[var(--color-rule)]">
                  <th className="py-2 pr-4 font-medium">payback</th>
                  <th className="py-2 pr-4 font-medium">operator earnings / day</th>
                  <th className="py-2 pr-4 font-medium">
                    {gpuPrice ? `output tokens / day at ${usd(gpuPrice.outPerMUsd)}/1M` : "output tokens / day"}
                  </th>
                  <th className="py-2 font-medium">paid jobs / day</th>
                </tr>
              </thead>
              <tbody className="divide-y divide-[var(--color-rule)]">
                {PAYBACK_MULTIPLES.map((x) => {
                  const net = x * rate * 24;
                  const buyerSpend = net / (OPERATOR_SHARE_PCT / 100);
                  const tokens = gpuPrice && gpuPrice.outPerMUsd > 0 ? (buyerSpend / gpuPrice.outPerMUsd) * 1e6 : null;
                  const jobs = observed && observed.avgPaidJobUsd > 0 ? buyerSpend / observed.avgPaidJobUsd : null;
                  return (
                    <tr key={x}>
                      <td className="py-3 pr-4 font-bold">{x}x</td>
                      <td className="py-3 pr-4 tabular-nums accent font-bold">{usd(net)}</td>
                      <td className="py-3 pr-4 tabular-nums">
                        {tokens === null ? "—" : `${(tokens / 1e6).toFixed(tokens < 1e6 ? 2 : 1)}M`}
                      </td>
                      <td className="py-3 tabular-nums">{jobs === null ? "—" : Math.ceil(jobs).toLocaleString()}</td>
                    </tr>
                  );
                })}
              </tbody>
            </table>
          </div>
          <p className="text-xs text-[var(--color-dim)] leading-6">
            {gpuPrice
              ? `Tokens are priced at the live network output price of ${gpuPrice.model} (input is extra, so this is the most work it can take). `
              : "The live network price is unavailable right now. "}
            {observed
              ? `Paid jobs use what a paid inference job actually billed over the last 7 days (${usd(observed.avgPaidJobUsd)} on average); the whole network sold ${Math.round(observed.paidJobsPerDay).toLocaleString()} paid jobs a day. A GPU needs that much demand of its own before renting it pays.`
              : "Observed demand is unavailable right now."}
          </p>
        </div>
      )}

      <div className="border-t border-[var(--color-rule)] pt-4 space-y-3">
        <div className="flex flex-wrap items-center justify-between gap-2">
          <h2 className="text-sm font-semibold text-[var(--color-fg)]">Observed on c0mpute: job prices by hardware and workload</h2>
          <div className="flex gap-2" role="group" aria-label="Observed window">
            {(observedByClass?.windows ?? []).map((w) => (
              <button
                key={w.days}
                type="button"
                aria-pressed={observedDays === w.days}
                onClick={() => setObservedDays(w.days)}
                className={`rounded border px-3 py-1 text-xs font-semibold ${observedDays === w.days ? "border-[var(--color-accent)] accent" : "border-[var(--color-rule)] text-[var(--color-dim)]"}`}
              >
                {w.days} days
              </button>
            ))}
          </div>
        </div>
        {!observedWindow ? (
          <p className="text-xs text-[var(--color-dim)] leading-6">
            The network job history is unavailable right now, so there is no observed figure to show.
          </p>
        ) : (
          <ObservedPanel window={observedWindow} myClass={myClass} rate={rate} historyFrom={observedByClass?.history_from ?? null} />
        )}
      </div>

      <div className="border-t border-[var(--color-rule)] pt-4 space-y-2 text-xs leading-6">
        <p className="text-[var(--color-fg)]">
          <strong className="accent">What operators were actually paid (settled):</strong>{" "}
          {observed
            ? `${usd(observed.earned30d)} in the last 30 days (${usd(observed.earned7d)} in 7 days, ${usd(observed.earnedAll)} all time) across ${observed.earningNodes30d.toLocaleString()} earning nodes; the median earning node made ${usd(observed.medianNode30d)} and the best ${usd(observed.bestNode30d)} in 30 days.`
            : "The earnings ledgers are not answering right now."}{" "}
          {jobsCompleted24h === null ? "" : `${jobsCompleted24h.toLocaleString()} jobs completed in the last 24 hours. `}
          <a href="/status#earnings">See the full breakdown</a>.
        </p>
        {hardware === "cpu" && (
          <p className="text-[var(--color-dim)]">
            {cpuWorkersOnline === null
              ? "The live status does not report CPU-capable workers right now."
              : `${cpuWorkersOnline.toLocaleString()} online workers advertise CPU capability; this is not a count of CPU-only nodes.`} Observed job prices for CPU nodes are in the hardware-class table above.
          </p>
        )}
        <p className="text-[var(--color-dim)]">
          The monthly scenarios are comparisons, not a c0mpute payout forecast. Providers currently bid per job, so realized earnings depend on workload demand, your bids, and how often jobs choose your node.{" "}
          {liveRates ? (
            <>
              Comparison rates come from{" "}
              <a href={`${R4CK_SITE}/servers?kind=gpu&interval=hour&sort=price`} target="_blank" rel="noopener noreferrer">r4ck</a>, our search engine over server and GPU offers: the median price of the live offers for each spec ({liveRates.gpuOffersRead.toLocaleString()} hourly GPU offers read), refreshed daily, as of {asOf(liveRates)}. GPU rates are per GPU; CPU rates are VPS monthly prices over 730 hours. Where r4ck lists no offer, the page falls back to hand-checked rates from{" "}
            </>
          ) : (
            <>Live r4ck rates are unavailable right now, so these are hand-checked rates (GPU {GPU_RATE_CHECKED_AT}, CPU {CPU_RATE_CHECKED_AT}) from{" "}</>
          )}
          <a href={GPU_RATE_SOURCES["runpod-secure"].url} target="_blank" rel="noopener noreferrer">RunPod</a>,
          {" "}<a href={GPU_RATE_SOURCES["vast-median"].url} target="_blank" rel="noopener noreferrer">Vast.ai</a>,
          {" "}<a href={GPU_RATE_SOURCES.lambda.url} target="_blank" rel="noopener noreferrer">Lambda</a> and
          {" "}<a href={CPU_RATE_SOURCE.url} target="_blank" rel="noopener noreferrer">DigitalOcean</a>. Prices change.
        </p>
      </div>
    </section>
  );
}

function perHour(g: ObservedGroup): string {
  return g.net_per_busy_hour == null ? "-" : `${usd(g.net_per_busy_hour)}/h`;
}

function ObservedPanel({
  window: w,
  myClass,
  rate,
  historyFrom,
}: {
  window: ObservedWindow;
  myClass: string;
  rate: number;
  historyFrom: number | null;
}) {
  const mine = w.by_class.find((g) => g.key === myClass) ?? null;
  const small = w.totals.jobs < 1000 || w.totals.nodes < 10;
  const since = historyFrom ? new Date(historyFrom).toISOString().slice(0, 10) : null;
  return (
    <div className="space-y-3 text-xs leading-6">
      <p className="text-[var(--color-dim)]">
        Every job the network completed in the last {w.days} days, as the status aggregators heard it, priced at what the buyer agreed to pay in the job auction, after the assumed {MARKET_FEE_PCT}% fee. Per paid hour divides that by the time nodes spent running the jobs, so it compares directly with the rental rate above. Source: the c0mpute job ledger (node_jobs). Chat inference bridged from Infernet is paid through the Infernet ledger, not at its auction price, so it shows jobs and hours here but no money; the settled totals in the line below count it once, from that ledger. See <a href="/status#earnings">/status</a>.
      </p>
      {w.totals.jobs === 0 ? (
        <p className="text-[var(--color-fg)]">No completed jobs are on file for this window.</p>
      ) : (
        <>
          <p className="text-[var(--color-fg)]">
            <strong className="accent">Sample:</strong> {w.totals.jobs.toLocaleString()} jobs ({w.totals.settled_elsewhere_jobs.toLocaleString()} of them Infernet chat) on {w.totals.nodes} node{w.totals.nodes === 1 ? "" : "s"}, {w.totals.busy_hours.toFixed(1)} paid hours, {usd(w.totals.net_usd)} after the fee for c0mpute-priced work ({perHour(w.totals)} per priced hour).
            {w.covered_days < w.days && since ? ` History on file starts ${since}, so this covers ${w.covered_days} of ${w.days} days.` : ""}
            {small ? " That is a small sample from an early network: read it as what has happened so far, not a typical rate." : ""}
          </p>
          {mine ? (
            <p className="text-[var(--color-fg)]">
              <strong className="accent">Your hardware class ({mine.label}):</strong> {mine.jobs.toLocaleString()} jobs on {mine.nodes} node{mine.nodes === 1 ? "" : "s"}, {perHour(mine)} per paid hour observed
              {rate > 0 ? `, against the ${usd(rate)}/h comparison rate above` : ""}.
              {mine.nodes < 3 ? ` Only ${mine.nodes} node${mine.nodes === 1 ? "" : "s"} in this class, so this is one operator's experience, not a market.` : ""}
            </p>
          ) : (
            <p className="text-[var(--color-fg)]">No completed jobs are on file for this hardware class in the window.</p>
          )}
          <ObservedTable title="by hardware class" groups={w.by_class} />
          <ObservedTable title="by workload" groups={w.by_type} />
        </>
      )}
    </div>
  );
}

function ObservedTable({ title, groups }: { title: string; groups: ObservedGroup[] }) {
  return (
    <div className="overflow-x-auto">
      <table className="w-full border-collapse text-left text-xs">
        <thead className="text-[var(--color-dim)]">
          <tr className="border-b border-[var(--color-rule)]">
            <th className="py-2 pr-4 font-medium">{title}</th>
            <th className="py-2 pr-4 font-medium">nodes</th>
            <th className="py-2 pr-4 font-medium">jobs</th>
            <th className="py-2 pr-4 font-medium">paid hours</th>
            <th className="py-2 pr-4 font-medium">after fee</th>
            <th className="py-2 font-medium">per paid hour</th>
          </tr>
        </thead>
        <tbody className="divide-y divide-[var(--color-rule)]">
          {groups.map((g) => (
            <tr key={g.key}>
              <td className="py-2 pr-4">{g.label}</td>
              <td className="py-2 pr-4 tabular-nums">{g.nodes}</td>
              <td className="py-2 pr-4 tabular-nums">{g.jobs.toLocaleString()}</td>
              <td className="py-2 pr-4 tabular-nums">{g.busy_hours.toFixed(1)}</td>
              {g.settled_elsewhere_jobs === g.jobs ? (
                <td className="py-2 text-[var(--color-dim)]" colSpan={2}>paid via the Infernet ledger</td>
              ) : (
                <>
                  <td className="py-2 pr-4 tabular-nums">{usd(g.net_usd)}</td>
                  <td className="py-2 accent font-bold tabular-nums">{perHour(g)}</td>
                </>
              )}
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}
