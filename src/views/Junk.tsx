import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { formatBytes } from "../format";
import type { CleanOutcome, JunkGroup } from "../types";

export default function Junk({ onRecovered }: { onRecovered: (bytes: number) => void }) {
  const [groups, setGroups] = useState<JunkGroup[] | null>(null);
  const [scanning, setScanning] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [confirming, setConfirming] = useState<string | null>(null);
  const [cleaning, setCleaning] = useState<string | null>(null);

  const scan = async () => {
    setScanning(true);
    setError(null);
    try {
      setGroups(await invoke<JunkGroup[]>("junk_scan"));
    } catch (e) {
      setError(String(e));
    } finally {
      setScanning(false);
    }
  };

  useEffect(() => {
    scan();
  }, []);

  const clean = async (key: string, paths: string[]) => {
    setCleaning(key);
    setConfirming(null);
    try {
      const outcome = await invoke<CleanOutcome>("clean_paths", { paths });
      if (outcome.errors.length) setError(outcome.errors.join("; "));
      onRecovered(outcome.freed_bytes);
      await scan();
    } catch (e) {
      setError(String(e));
    } finally {
      setCleaning(null);
    }
  };

  const total = groups?.filter((g) => g.cleanable).reduce((a, g) => a + g.total_bytes, 0) ?? 0;

  return (
    <>
      <div className="view-head">
        <div>
          <h2 className="view-title">Junk files</h2>
          <p className="view-sub">
            Not caches — forgotten files that quietly take up space
          </p>
        </div>
        <button className="btn" onClick={scan} disabled={scanning}>
          {scanning ? "Scanning…" : "Rescan"}
        </button>
      </div>

      {error && (
        <div className="banner-error">
          <span>{error}</span>
          <button className="btn-ghost" onClick={() => setError(null)}>Dismiss</button>
        </div>
      )}

      {scanning && !groups && (
        <div className="empty-state">
          <div className="spinner" />
          <p>Looking for installers, backups, logs, and large unused files…</p>
        </div>
      )}

      {groups && (
        <section className="metrics" style={{ gridTemplateColumns: "repeat(auto-fit, minmax(220px, 1fr))" }}>
          <div className="metric accent">
            <span className="metric-label">Junk found</span>
            <span className="metric-value">{formatBytes(total)}</span>
          </div>
        </section>
      )}

      {groups?.length === 0 && (
        <div className="empty-state"><p>No junk found — nice and tidy.</p></div>
      )}

      {groups?.map((g) => {
        const safeItems = g.items.filter((i) => i.safe);
        const safeTotal = safeItems.reduce((a, i) => a + i.size_bytes, 0);
        return (
          <div key={g.id} className="browser-card">
            <div className="browser-head">
              <div>
                <span className="finding-name">{g.title}</span>
                <span className="view-sub" style={{ marginLeft: 10 }}>{formatBytes(g.total_bytes)}</span>
                <p className="view-sub" style={{ marginTop: 2 }}>{g.note}</p>
              </div>
              {g.cleanable && safeItems.length > 0 && (
                confirming === g.id ? (
                  <span className="confirm">
                    <button className="btn-danger" onClick={() => clean(g.id, safeItems.map((i) => i.path))}>
                      Move {formatBytes(safeTotal)} to Trash
                    </button>
                    <button className="btn-ghost" onClick={() => setConfirming(null)}>Cancel</button>
                  </span>
                ) : (
                  <button
                    className="btn-clean"
                    disabled={cleaning === g.id}
                    onClick={() => setConfirming(g.id)}
                  >
                    {cleaning === g.id ? "Cleaning…" : `Clean safe items (${formatBytes(safeTotal)})`}
                  </button>
                )
              )}
            </div>
            <div className="browser-items">
              {g.items.map((i) => (
                <div key={i.path} className="junk-item">
                  <span className="child-name" title={i.path}>{i.label}</span>
                  <span className={`badge ${i.safe ? "safety-safe" : "safety-caution"}`}>
                    {i.safe ? "Safe" : "Review"}
                  </span>
                  {i.stale_days != null && i.stale_days > 30 && (
                    <span className="badge stale">{Math.round(i.stale_days / 30)} mo old</span>
                  )}
                  <span className="child-size">{formatBytes(i.size_bytes)}</span>
                  <button
                    className="btn-ghost sm"
                    onClick={() => invoke("reveal_path", { path: i.path }).catch((e) => setError(String(e)))}
                  >
                    Show
                  </button>
                  {g.cleanable && (
                    confirming === i.path ? (
                      <span className="confirm">
                        <button className="btn-danger sm" onClick={() => clean(i.path, [i.path])}>Trash</button>
                        <button className="btn-ghost sm" onClick={() => setConfirming(null)}>Cancel</button>
                      </span>
                    ) : (
                      <button
                        className="btn-clean sm"
                        disabled={cleaning === i.path}
                        onClick={() => setConfirming(i.path)}
                      >
                        Clean
                      </button>
                    )
                  )}
                </div>
              ))}
            </div>
          </div>
        );
      })}
    </>
  );
}
