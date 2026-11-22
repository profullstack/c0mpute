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

const DAYS_PER_MONTH = 30;
const OTHER_GPU = "__other__";
const CUSTOM_CPU = "__custom__";

type Hardware = "gpu" | "cpu";

function rateFor(model: string): string {
  const match = GPU_RATE_REFERENCES.find((gpu) => gpu.model === model);
  return match?.hourlyUsd != null ? String(match.hourlyUsd) : "";
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
}: {
  jobsCompleted24h: number | null;
  cpuWorkersOnline: number | null;
  observed?: ObservedEarnings | null;
  gpuPrice?: GpuModelPrice | null;
}) {
  const [hardware, setHardware] = useState<Hardware>("gpu");
  const [gpuModel, setGpuModel] = useState(DEFAULT_GPU_MODEL);
  const [customGpuModel, setCustomGpuModel] = useState("");
  const [gpuHourlyRate, setGpuHourlyRate] = useState(rateFor(DEFAULT_GPU_MODEL));
  const [cpuReference, setCpuReference] = useState(DEFAULT_CPU_MODEL);
  const [cpuHourlyRate, setCpuHourlyRate] = useState(String(CPU_RATE_REFERENCES[0].hourlyUsd));
  const [cpuRateEdited, setCpuRateEdited] = useState(false);
  const [hoursAvailable, setHoursAvailable] = useState("24");

  const selectedGpu = GPU_RATE_REFERENCES.find((gpu) => gpu.model === gpuModel);
  const selectedCpu = CPU_RATE_REFERENCES.find((cpu) => cpu.model === cpuReference);
  const rate = Math.max(0, Number(hardware === "gpu" ? gpuHourlyRate : cpuHourlyRate) || 0);
  const hours = Math.max(0, Math.min(24, Number(hoursAvailable) || 0));

  function updateGpuModel(next: string) {
    setGpuModel(next);
    setGpuHourlyRate(rateFor(next));
  }

  function updateCpuModel(next: string) {
    setCpuReference(next);
    const match = CPU_RATE_REFERENCES.find((cpu) => cpu.model === next);
    setCpuHourlyRate(match ? String(match.hourlyUsd) : "");
    setCpuRateEdited(false);
  }

  const selectedGpuSource = selectedGpu?.source ? GPU_RATE_SOURCES[selectedGpu.source] : null;
  const gpuRateHint = !selectedGpu
    ? "Custom model: enter a comparable rate."
    : selectedGpuSource
      ? `${selectedGpu.vram} · rate from ${selectedGpuSource.label}.`
      : `${selectedGpu.vram} · no public hourly rental rate for this GPU, so enter your own.`;

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
                      <option key={gpu.model} value={gpu.model}>{`${gpu.model} · ${gpu.vram}`}</option>
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
                  <option key={cpu.model} value={cpu.model}>{`${cpu.model} · $${cpu.hourlyUsd}/h`}</option>
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
                {selectedCpu ? `${selectedCpu.specs} · starting price from ${CPU_RATE_SOURCE.label}${cpuRateEdited ? "; using your edited rate" : ""}.` : "Custom CPU VM: enter a comparable hourly price."} VM prices include memory, storage, and transfer.
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

      <div className="border-t border-[var(--color-rule)] pt-4 space-y-2 text-xs leading-6">
        <p className="text-[var(--color-fg)]">
          <strong className="accent">What operators actually earned:</strong>{" "}
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
              : `${cpuWorkersOnline.toLocaleString()} online workers advertise CPU capability; this is not a count of CPU-only nodes.`} The public status does not attribute jobs or payouts to CPU hardware, so actual CPU earnings, including Infernet work, cannot be calculated from it.
          </p>
        )}
        <p className="text-[var(--color-dim)]">
          This is a comparison scenario, not a c0mpute payout forecast. Providers currently bid per job, so realized earnings depend on workload demand, your bids, and how often jobs choose your node. {hardware === "gpu" ? (
            <>GPU rental references were checked {GPU_RATE_CHECKED_AT}; prices change. Sources:{" "}
              <a href={GPU_RATE_SOURCES["runpod-secure"].url} target="_blank" rel="noopener noreferrer">RunPod</a>,
              {" "}<a href={GPU_RATE_SOURCES["vast-median"].url} target="_blank" rel="noopener noreferrer">Vast.ai</a>,
              {" "}<a href={GPU_RATE_SOURCES["vast-low"].url} target="_blank" rel="noopener noreferrer">r4ck.dev</a>,
              {" "}<a href={GPU_RATE_SOURCES.lambda.url} target="_blank" rel="noopener noreferrer">Lambda</a>.
            </>
          ) : (
            <>CPU VM comparison rates are from {CPU_RATE_SOURCE.label}, checked {CPU_RATE_CHECKED_AT}; they include RAM, storage, and transfer. Source:{" "}
              <a href={CPU_RATE_SOURCE.url} target="_blank" rel="noopener noreferrer">DigitalOcean pricing</a>.
            </>
          )}
        </p>
      </div>
    </section>
  );
}
