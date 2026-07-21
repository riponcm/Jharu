import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { formatBytes } from "../format";
import type { DriveInfo } from "../types";

function Donut({ pct }: { pct: number }) {
  const r = 52;
  const c = 2 * Math.PI * r;
  const color = pct >= 90 ? "var(--danger)" : pct >= 80 ? "#ba7517" : "var(--c-dev)";
  return (
    <svg width="140" height="140" viewBox="0 0 140 140" role="img" aria-label={`${Math.round(pct)}% used`}>
      <circle cx="70" cy="70" r={r} fill="none" stroke="var(--surface-2)" strokeWidth="14" />
      <circle
        cx="70"
        cy="70"
        r={r}
        fill="none"
        stroke={color}
        strokeWidth="14"
        strokeLinecap="round"
        strokeDasharray={`${(pct / 100) * c} ${c}`}
        transform="rotate(-90 70 70)"
      />
      <text
        x="70"
        y="66"
        textAnchor="middle"
        style={{ fontSize: 24, fontWeight: 600, fill: "var(--text)" }}
      >
        {Math.round(pct)}%
      </text>
      <text x="70" y="86" textAnchor="middle" style={{ fontSize: 12, fill: "var(--text-2)" }}>
        used
      </text>
    </svg>
  );
}

export default function DriveHealth() {
  const [drives, setDrives] = useState<DriveInfo[] | null>(null);
  const [scanning, setScanning] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const scan = async () => {
    setScanning(true);
    setError(null);
    try {
      setDrives(await invoke<DriveInfo[]>("drive_health"));
    } catch (e) {
      setError(String(e));
    } finally {
      setScanning(false);
    }
  };

  useEffect(() => {
    scan();
  }, []);

  return (
    <>
      <div className="view-head">
        <div>
          <h2 className="view-title">Drive health</h2>
          <p className="view-sub">Capacity and hardware status for every mounted drive</p>
        </div>
        <button className="btn" onClick={scan} disabled={scanning}>
          {scanning ? "Checking…" : "Refresh"}
        </button>
      </div>

      {error && (
        <div className="banner-error">
          <span>{error}</span>
          <button className="btn-ghost" onClick={() => setError(null)}>Dismiss</button>
        </div>
      )}

      {scanning && !drives && (
        <div className="empty-state"><div className="spinner" /><p>Reading drive information…</p></div>
      )}

      <div className="drive-grid">
        {drives?.map((d) => {
          const used = d.total_bytes - d.available_bytes;
          const pct = d.total_bytes ? (used / d.total_bytes) * 100 : 0;
          const healthy = d.smart_status === "Verified";
          return (
            <div key={d.mount} className="drive-card">
              <Donut pct={pct} />
              <div className="drive-info">
                <span className="finding-name">{d.name || d.mount}</span>
                <code className="finding-path">{d.mount}</code>
                <div className="drive-badges">
                  <span className="badge stale">{d.kind}</span>
                  <span className="badge stale">{d.file_system}</span>
                  {d.removable && <span className="badge stale">Removable</span>}
                  {d.smart_status && (
                    <span className={`badge ${healthy ? "safety-safe" : "safety-risky"}`}>
                      SMART: {d.smart_status}
                    </span>
                  )}
                </div>
                <p className="drive-line">
                  <strong>{formatBytes(d.available_bytes)}</strong> free of {formatBytes(d.total_bytes)}
                </p>
                {pct >= 90 && (
                  <p className="drive-warn">
                    Nearly full — macOS and apps may slow down or fail to update. Free up space soon.
                  </p>
                )}
                {pct >= 80 && pct < 90 && (
                  <p className="drive-warn soft">Getting full — consider a cleanup pass.</p>
                )}
              </div>
            </div>
          );
        })}
      </div>
    </>
  );
}
