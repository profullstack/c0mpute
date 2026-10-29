"use client";

import { useMemo, useState } from "react";
import {
  GPU_RATE_CHECKED_AT,
  GPU_RATE_REFERENCES,
  GPU_RATE_SOURCE,
  UTILIZATION_SCENARIOS,
} from "@/lib/gpu-estimates";
import { MARKET_FEE_PCT, OPERATOR_SHARE_PCT } from "@/lib/market";

const DAYS_PER_MONTH = 30;

function usd(value: number): string {
  return new Intl.NumberFormat("en-US", {
    style: "currency",
    currency: "USD",
    maximumFractionDigits: value < 10 ? 2 : 0,
  }).format(value);
}

export function EarningsEstimator({ jobsCompleted24h }: { jobsCompleted24h: number | null }) {
  const [model, setModel] = useState(GPU_RATE_REFERENCES[1].model);
  const [hourlyRate, setHourlyRate] = useState(String(GPU_RATE_REFERENCES[1].hourlyUsd));
  const [hoursAvailable, setHoursAvailable] = useState("24");

  const selected = useMemo(
    () => GPU_RATE_REFERENCES.find((gpu) => gpu.model === model),
    [model],
  );
  const rate = Math.max(0, Number(hourlyRate) || 0);
  const hours = Math.max(0, Math.min(24, Number(hoursAvailable) || 0));

  function updateModel(next: string) {
    setModel(next);
    const match = GPU_RATE_REFERENCES.find((gpu) => gpu.model === next);
    setHourlyRate(match ? String(match.hourlyUsd) : "");
  }

  return (
    <section className="space-y-5 border border-[var(--color-rule)] bg-[var(--color-card)] rounded-md p-4 sm:p-6">
      <div className="grid gap-4 sm:grid-cols-2">
        <label className="block space-y-2 text-sm">
          <span className="text-[var(--color-fg)]">GPU model</span>
          <input
            aria-label="GPU model"
            list="gpu-rate-models"
            value={model}
            onChange={(event) => updateModel(event.target.value)}
            placeholder="Type a model, e.g. RTX 4090"
            className="w-full rounded border border-[var(--color-rule)] bg-[var(--color-bg)] px-3 py-2 text-[var(--color-fg)] outline-none focus:border-[var(--color-accent)]"
          />
          <datalist id="gpu-rate-models">
            {GPU_RATE_REFERENCES.map((gpu) => (
              <option key={gpu.model} value={gpu.model}>{`${gpu.model} · ${gpu.vram}`}</option>
            ))}
          </datalist>
        </label>

        <label className="block space-y-2 text-sm">
          <span className="text-[var(--color-fg)]">Comparable GPU price per hour (USD)</span>
          <input
            aria-label="Comparable GPU price per hour"
            type="number"
            min="0"
            step="0.01"
            inputMode="decimal"
            value={hourlyRate}
            onChange={(event) => setHourlyRate(event.target.value)}
            placeholder="Enter your own comparison rate"
            className="w-full rounded border border-[var(--color-rule)] bg-[var(--color-bg)] px-3 py-2 text-[var(--color-fg)] outline-none focus:border-[var(--color-accent)]"
          />
          <span className="block text-xs text-[var(--color-dim)]">
            {selected ? `${selected.vram} reference rate.` : "Custom model: enter a comparable rate."} This is an external rental-price benchmark.
          </span>
        </label>

        <label className="block space-y-2 text-sm sm:col-span-2">
          <span className="text-[var(--color-fg)]">Hours per day your GPU is available</span>
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
          Paid utilization means the share of available time that is occupied by paid work. These scenarios apply the comparison rate to that time for 30 days.
        </p>
        <div className="overflow-x-auto">
          <table className="w-full border-collapse text-left text-xs">
            <thead className="text-[var(--color-dim)]">
              <tr className="border-b border-[var(--color-rule)]">
                <th className="py-2 pr-4 font-medium">paid utilization</th>
                <th className="py-2 pr-4 font-medium">paid hours / month</th>
                <th className="py-2 pr-4 font-medium">gross reference</th>
                <th className="py-2 font-medium">after {MARKET_FEE_PCT}% market fee</th>
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
          Direct peer-to-peer jobs have no c0mpute market fee. The after-fee column assumes every dollar comes through the c0mpute market and excludes electricity, internet, hardware, tax, and other costs.
        </p>
      </div>

      <div className="border-t border-[var(--color-rule)] pt-4 space-y-2 text-xs leading-6">
        <p className="text-[var(--color-fg)]">
          <strong className="accent">Observed c0mpute demand:</strong>{" "}
          {jobsCompleted24h === null
            ? "Live network stats are unavailable. Public payout totals by GPU are not available yet, so there is no actual average to show."
            : `${jobsCompleted24h.toLocaleString()} completed jobs in the last 24 hours. Public payout totals by GPU are not available yet, so there is no actual average to show.`}
        </p>
        <p className="text-[var(--color-dim)]">
          This is a comparison scenario, not a c0mpute payout forecast. c0mpute providers currently bid per job, so realized earnings depend on workload demand, your bids, and how often jobs choose your node. The reference rates are public RunPod GPU prices checked {GPU_RATE_CHECKED_AT}; prices change.
          {" "}<a href={GPU_RATE_SOURCE} target="_blank" rel="noopener noreferrer">View current RunPod rates</a>.
        </p>
      </div>
    </section>
  );
}
