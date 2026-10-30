import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { formatBytes } from "../format";
import ConfirmClean from "../components/ConfirmClean";
import type { BrowserReport, CleanOutcome } from "../types";

export default function Browsers({ onRecovered }: { onRecovered: (bytes: number) => void }) {
  const [reports, setReports] = useState<BrowserReport[] | null>(null);
  const [scanning, setScanning] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [confirming, setConfirming] = useState<string | null>(null);
  const [cleaning, setCleaning] = useState<string | null>(null);

  const scan = async () => {
    setScanning(true);
    setError(null);
    try {
      setReports(await invoke<BrowserReport[]>("browser_scan"));
    } catch (e) {
      setError(String(e));
    } finally {
      setScanning(false);
    }
  };

  useEffect(() => {
    scan();
  }, []);

  const cleanPaths = async (key: string, paths: string[]) => {
    setCleaning(key);
    setConfirming(null);
    try {
      const outcome = await invoke<CleanOutcome>("clean_paths", { paths, source: "Browsers" });
      if (outcome.errors.length) setError(outcome.errors.join("; "));
      onRecovered(outcome.freed_bytes);
      await scan();
    } catch (e) {
      setError(String(e));
    } finally {
      setCleaning(null);
    }
  };

  const pending = reports?.find((b) => b.name === confirming);
  const pendingSafe = pending?.items.filter((i) => i.safe) ?? [];

  return (
    <>
      {pending && (
        <ConfirmClean
          request={{
            kind: "cache",
            title: `Clear ${pending.name} cache?`,
            items: pendingSafe.map((i) => i.label),
            bytes: pendingSafe.reduce((a, i) => a + i.size_bytes, 0),
          }}
          busy={cleaning === pending.name}
          onConfirm={() => cleanPaths(pending.name, pendingSafe.map((i) => i.path))}
          onCancel={() => setConfirming(null)}
        />
      )}

      <div className="view-head">
        <div>
          <h2 className="view-title">Browsers</h2>
          <p className="view-sub">Cache data per installed browser</p>
        </div>
        <button className="btn" onClick={scan} disabled={scanning}>
          {scanning ? "Scanning…" : "Rescan"}
        </button>
      </div>

      <div className="banner-info">
        Quit a browser before cleaning its caches — open browsers may recreate or lock files.
      </div>

      {error && (
        <div className="banner-error">
          <span>{error}</span>
          <button className="btn-ghost" onClick={() => setError(null)}>Dismiss</button>
        </div>
      )}

      {scanning && !reports && (
        <div className="empty-state"><div className="spinner" /><p>Detecting browsers…</p></div>
      )}

      {reports && reports.length === 0 && (
        <div className="empty-state"><p>No browser cache data found.</p></div>
      )}

      {reports?.map((b) => {
        const safeItems = b.items.filter((i) => i.safe);
        const safeTotal = safeItems.reduce((a, i) => a + i.size_bytes, 0);
        return (
          <div key={b.name} className="browser-card">
            <div className="browser-head">
              <div>
                <span className="finding-name">{b.name}</span>
                <span className="view-sub" style={{ marginLeft: 10 }}>
                  {formatBytes(b.total_bytes)} cache
                </span>
              </div>
              {safeItems.length > 0 && (
                <button
                  className="btn-clean"
                  disabled={cleaning === b.name}
                  onClick={() => setConfirming(b.name)}
                >
                  {cleaning === b.name ? "Cleaning…" : `Clean all safe (${formatBytes(safeTotal)})`}
                </button>
              )}
            </div>
            <div className="browser-items">
              {b.items.map((i) => (
                <div key={i.path} className="browser-item">
                  <span className="child-name" title={i.path}>{i.label}</span>
                  <span className={`badge ${i.safe ? "safety-safe" : "safety-caution"}`}>
                    {i.safe ? "Safe" : "Review"}
                  </span>
                  <div className="child-bar">
                    <div
                      className="child-fill"
                      style={{ width: `${Math.max(1, (i.size_bytes / b.total_bytes) * 100)}%` }}
                    />
                  </div>
                  <span className="child-size">{formatBytes(i.size_bytes)}</span>
                </div>
              ))}
            </div>
          </div>
        );
      })}
    </>
  );
}
