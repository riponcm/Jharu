import { useEffect, useMemo, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { formatBytes } from "../format";
import ConfirmClean from "../components/ConfirmClean";
import type { CleanOutcome, ModelEntry, ModelReport } from "../types";

type Filter = "all" | "unused";

function ageLabel(days: number | null): string {
  if (days == null) return "unknown";
  if (days < 1) return "today";
  if (days < 30) return `${days}d ago`;
  if (days < 365) return `${Math.round(days / 30)} mo ago`;
  return `${(days / 365).toFixed(1)} yr ago`;
}

export default function Models({ onRecovered }: { onRecovered: (bytes: number) => void }) {
  const [report, setReport] = useState<ModelReport | null>(null);
  const [scanning, setScanning] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [filter, setFilter] = useState<Filter>("all");
  const [selected, setSelected] = useState<Set<string>>(new Set());
  const [confirming, setConfirming] = useState(false);
  const [working, setWorking] = useState(false);

  const scan = async () => {
    setScanning(true);
    setError(null);
    setSelected(new Set());
    try {
      setReport(await invoke<ModelReport>("model_scan"));
    } catch (e) {
      setError(String(e));
    } finally {
      setScanning(false);
    }
  };

  useEffect(() => {
    scan();
  }, []);

  const visible = useMemo(() => {
    if (!report) return [];
    return filter === "unused"
      ? report.models.filter((m) => !m.used_since_download)
      : report.models;
  }, [report, filter]);

  const toggle = (name: string) => {
    setSelected((prev) => {
      const next = new Set(prev);
      if (next.has(name)) next.delete(name);
      else next.add(name);
      return next;
    });
  };

  const selectAllUnused = () => {
    if (!report) return;
    setSelected(new Set(report.models.filter((m) => !m.used_since_download).map((m) => m.name)));
  };

  const selectedModels: ModelEntry[] =
    report?.models.filter((m) => selected.has(m.name)) ?? [];
  const selectedBytes = selectedModels.reduce((a, m) => a + m.exclusive_bytes, 0);

  const removeSelected = async () => {
    setWorking(true);
    setConfirming(false);
    try {
      const paths = selectedModels.flatMap((m) => m.paths);
      const outcome = await invoke<CleanOutcome>("clean_paths", { paths });
      if (outcome.errors.length) setError(outcome.errors.join("; "));
      onRecovered(outcome.freed_bytes);
      await scan();
    } catch (e) {
      setError(String(e));
    } finally {
      setWorking(false);
    }
  };

  return (
    <>
      {confirming && (
        <ConfirmClean
          request={{
            kind: "model",
            title: `Remove ${selectedModels.length} model${selectedModels.length === 1 ? "" : "s"}?`,
            items: selectedModels.map((m) => m.name),
            bytes: selectedBytes,
            risky: selectedModels.some((m) => m.used_since_download),
          }}
          busy={working}
          onConfirm={removeSelected}
          onCancel={() => setConfirming(false)}
        />
      )}

      <div className="view-head">
        <div>
          <h2 className="view-title">ML models</h2>
          <p className="view-sub">
            Every downloaded model, one by one — keep what you use, remove what you never loaded
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

      {scanning && !report && (
        <div className="empty-state">
          <div className="spinner" />
          <p>Reading model caches and working out which ones you have actually loaded…</p>
        </div>
      )}

      {report && (
        <>
          <section className="metrics">
            <div className="metric">
              <span className="metric-label">All models</span>
              <span className="metric-value">{formatBytes(report.total_bytes)}</span>
            </div>
            <div className="metric accent">
              <span className="metric-label">Downloaded but never loaded</span>
              <span className="metric-value">
                {report.usage_tracking_reliable ? formatBytes(report.unused_bytes) : "—"}
              </span>
            </div>
            <div className="metric">
              <span className="metric-label">Models found</span>
              <span className="metric-value">{report.models.length}</span>
            </div>
          </section>

          <div className="banner-info">
            Removing a model does not break anything — it will be downloaded again automatically the
            next time your code asks for it. The only cost is the download.
            {!report.usage_tracking_reliable && (
              <>
                {" "}
                This disk does not record file access times, so Jharu cannot tell which models you
                have loaded. Only download dates are shown.
              </>
            )}
          </div>

          <div className="filter-row">
            <div className="filter-tabs">
              <button
                className={`filter-tab ${filter === "all" ? "is-active" : ""}`}
                onClick={() => setFilter("all")}
              >
                All ({report.models.length})
              </button>
              <button
                className={`filter-tab ${filter === "unused" ? "is-active" : ""}`}
                onClick={() => setFilter("unused")}
              >
                Never loaded ({report.models.filter((m) => !m.used_since_download).length})
              </button>
            </div>
            <div className="confirm">
              {report.usage_tracking_reliable && (
                <button className="btn-ghost" onClick={selectAllUnused}>
                  Select all never-loaded
                </button>
              )}
              {selected.size > 0 && (
                <button className="btn-clean" onClick={() => setConfirming(true)}>
                  Remove {selected.size} selected ({formatBytes(selectedBytes)})
                </button>
              )}
            </div>
          </div>

          <div className="findings">
            {visible.length === 0 && (
              <div className="empty-state"><p>No models in this view.</p></div>
            )}
            {visible.map((m) => (
              <label key={m.name} className="model-row">
                <input
                  type="checkbox"
                  checked={selected.has(m.name)}
                  onChange={() => toggle(m.name)}
                />
                <div className="model-main">
                  <div className="finding-title">
                    <span className="finding-name">{m.name}</span>
                    <span className="badge cat-ml_models">{m.provider}</span>
                    {report.usage_tracking_reliable &&
                      (m.used_since_download ? (
                        <span className="badge safety-safe">In use · read {ageLabel(m.last_used_days)}</span>
                      ) : (
                        <span className="badge safety-caution">Never loaded</span>
                      ))}
                  </div>
                  <span className="model-meta">
                    Downloaded {ageLabel(m.downloaded_days)}
                    {m.note && ` · ${m.note}`}
                  </span>
                </div>
                <div className="finding-size">
                  <span className="size">{formatBytes(m.size_bytes)}</span>
                  {m.exclusive_bytes !== m.size_bytes && (
                    <span className="files">{formatBytes(m.exclusive_bytes)} reclaimable</span>
                  )}
                </div>
                <button
                  className="btn-ghost sm"
                  onClick={(e) => {
                    e.preventDefault();
                    invoke("reveal_path", { path: m.paths[0] }).catch(() => {});
                  }}
                >
                  Show
                </button>
              </label>
            ))}
          </div>
        </>
      )}
    </>
  );
}
