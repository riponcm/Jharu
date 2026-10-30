import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import "../styles/history.css";

/// The space history's verdict on one drive: when it fills, or why there is
/// no answer yet. Shown on the Drive health tab under its own forecast.
export interface SpaceForecast {
  mount: string;
  trend: "not_enough_data" | "steady" | "filling" | "freeing";
  bytes_per_day: number;
  days_until_full: number | null;
  readings: number;
  span_days: number;
  confidence: "none" | "low" | "fair" | "good";
  summary: string;
}

const norm = (mount: string) => mount.replace(/[\\/]+$/, "").toLowerCase();

// Every drive card asks for the same list, so ask once per refresh.
let cache: { at: number; list: Promise<SpaceForecast[]> } | null = null;

export function loadForecasts(): Promise<SpaceForecast[]> {
  if (!cache || Date.now() - cache.at > 5000) {
    cache = { at: Date.now(), list: invoke<SpaceForecast[]>("history_forecasts") };
  }
  return cache.list;
}

export default function ForecastNote({ mount }: { mount: string }) {
  const [forecast, setForecast] = useState<SpaceForecast | null>(null);

  useEffect(() => {
    let live = true;
    loadForecasts()
      .then((list) => live && setForecast(list.find((f) => norm(f.mount) === norm(mount)) ?? null))
      .catch(() => {});
    return () => {
      live = false;
    };
  }, [mount]);

  if (!forecast) return null;
  return (
    <p className="forecast-note hs-drive-note">
      <strong>From Jharu's space history:</strong> {forecast.summary}
    </p>
  );
}
