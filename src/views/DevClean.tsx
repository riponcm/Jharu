import { useEffect, useMemo, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { formatBytes } from "../format";
import ConfirmClean from "../components/ConfirmClean";
import type { CleanOutcome, Finding, ScanResult, Category, Safety } from "../types";

const CATEGORY_LABEL: Record<Category, string> = {
  ml_models: "ML models",
  dev_cache: "Dev cache",
  app_data: "App data",
};

const SAFETY_LABEL: Record<Safety, string> = {
  safe: "Safe to clear",
  caution: "Re-downloadable",
  risky: "Review first",
};

export default function DevClean({ onRecovered }: { onRecovered: (bytes: number) => void }) {
  const [result, setResult] = useState<ScanResult | null>(null);
  const [scanning, setScanning] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [expanded, setExpanded] = useState<string | null>(null);
  const [confirming, setConfirming] = useState<Finding | null>(null);
  const [cleaning, setCleaning] = useState<string | null>(null);

  const runScan = async () => {
    setScanning(true);
    setError(null);
    try {
      const scanned = await invoke<ScanResult>("scan_system");
      setResult(scanned);
      // Where the scan could only estimate what clearing frees (hard links
      // to elsewhere, as in uv's cache), fetch the exact figure afterwards.
      for (const f of scanned.findings.filter((x) => !x.reclaimable_exact)) {
        invoke<number>("reclaimable_size", { path: f.path })
          .then((exact) =>
            setResult((r) =>
              r && {
                ...r,
                findings: r.findings.map((x) =>
                  x.path === f.path
                    ? { ...x, reclaimable_bytes: Math.min(exact, x.size_bytes), reclaimable_exact: true }
                    : x
                ),
              }
            )
          )
          .catch(() => {});
      }
    } catch (e) {
      setError(String(e));
    } finally {
      setScanning(false);
    }
  };

  useEffect(() => {
    runScan();
  }, []);

  const stats = useMemo(() => {
    if (!result) return null;
    const sum = (f: (x: Finding) => boolean) =>
      result.findings.filter(f).reduce((a, x) => a + x.size_bytes, 0);
    const total = sum(() => true);
    // What clearing really frees, not the folders' size: a uv cache linked
    // into virtualenvs is large but frees little.
    const reclaimable = result.findings
      .filter((x) => x.safety !== "risky")
      .reduce((a, x) => a + x.reclaimable_bytes, 0);
    const ml = sum((x) => x.category === "ml_models");
    const dev = sum((x) => x.category === "dev_cache");
    const appData = sum((x) => x.category === "app_data");
    const used = result.disk_total - result.disk_free;
    const real = Math.max(0, used - total);
    return { total, reclaimable, ml, dev, appData, used, real };
  }, [result]);

  const clean = async (finding: Finding) => {
    setCleaning(finding.path);
    setConfirming(null);
    setExpanded(null);
    try {
      const outcome = await invoke<CleanOutcome>("clean_paths", {
        paths: [finding.path],
        // The Recycle Bin tab calls only what was rated safe here a safe cache.
        source: finding.safety === "safe" ? "Dev clean" : "Dev clean (not rated safe)",
      });
      if (outcome.errors.length) setError(outcome.errors.join("; "));
      onRecovered(outcome.freed_bytes);
      setResult((r) =>
        r ? { ...r, findings: r.findings.filter((f) => f.path !== finding.path) } : r
      );
    } catch (e) {
      setError(String(e));
    } finally {
      setCleaning(null);
    }
  };

  const reveal = (path: string) =>
    invoke("reveal_path", { path }).catch((e) => setError(String(e)));

  const segments =
    stats && result
      ? [
          { label: "Your data and apps", size: stats.real, cls: "seg-real" },
          { label: "ML models", size: stats.ml, cls: "seg-ml" },
          { label: "Dev caches", size: stats.dev, cls: "seg-dev" },
          { label: "App data found", size: stats.appData, cls: "seg-app" },
          { label: "Free", size: result.disk_free, cls: "seg-free" },
        ]
      : [];

  return (
    <>
      {confirming && (
        <ConfirmClean
          request={{
            kind: confirming.category === "ml_models" ? "model" : "cache",
            title: `Clean ${confirming.name}?`,
            items: [confirming.path],
            bytes: confirming.reclaimable_bytes,
            risky: confirming.safety === "risky",
          }}
          busy={cleaning === confirming.path}
          onConfirm={() => clean(confirming)}
          onCancel={() => setConfirming(null)}
        />
      )}

      <div className="view-head">
        <div>
          <h2 className="view-title">Dev clean</h2>
          <p className="view-sub">Known developer and ML cache locations</p>
        </div>
        <button className="btn" onClick={runScan} disabled={scanning}>
          {scanning ? "Scanning…" : "Rescan"}
        </button>
      </div>

      {error && (
        <div className="banner-error">
          <span>{error}</span>
          <button className="btn-ghost" onClick={() => setError(null)}>Dismiss</button>
        </div>
      )}

      {scanning && !result && (
        <div className="empty-state">
          <div className="spinner" />
          <p>Scanning known cache and model locations…</p>
        </div>
      )}

      {result && stats && (
        <>
          <section className="metrics">
            <div className="metric accent">
              <span className="metric-label">Reclaimable now</span>
              <span className="metric-value">{formatBytes(stats.reclaimable)}</span>
            </div>
            <div className="metric">
              <span className="metric-label">ML model data</span>
              <span className="metric-value">{formatBytes(stats.ml)}</span>
            </div>
            <div className="metric">
              <span className="metric-label">Dev caches</span>
              <span className="metric-value">{formatBytes(stats.dev)}</span>
            </div>
            <div className="metric">
              <span className="metric-label">Disk used</span>
              <span className="metric-value">
                {formatBytes(stats.used)}
                <span className="metric-sub"> / {formatBytes(result.disk_total)}</span>
              </span>
            </div>
          </section>

          <section>
            <h2 className="section-title">Storage breakdown</h2>
            <div className="breakdown">
              {segments.map((s) => (
                <div
                  key={s.label}
                  className={`seg ${s.cls}`}
                  style={{ width: `${(s.size / result.disk_total) * 100}%` }}
                  title={`${s.label} · ${formatBytes(s.size)}`}
                />
              ))}
            </div>
            <div className="legend">
              {segments.map((s) => (
                <span key={s.label} className="legend-item">
                  <i className={`dot ${s.cls}`} />
                  {s.label} · {formatBytes(s.size)}
                </span>
              ))}
            </div>
          </section>

          <section>
            <h2 className="section-title">
              Findings <span className="count">{result.findings.length}</span>
            </h2>
            <div className="findings">
              {result.findings.length === 0 && (
                <div className="empty-state"><p>Nothing found — your disk is already clean.</p></div>
              )}
              {result.findings.map((f) => (
                <div key={f.path} className="finding">
                  <div className="finding-row">
                    <button
                      className="expander"
                      onClick={() => setExpanded(expanded === f.path ? null : f.path)}
                      aria-label="Toggle details"
                    >
                      {expanded === f.path ? "▾" : "▸"}
                    </button>
                    <div className="finding-main">
                      <div className="finding-title">
                        <span className="finding-name">{f.name}</span>
                        <span className={`badge cat-${f.category}`}>{CATEGORY_LABEL[f.category]}</span>
                        <span className={`badge safety-${f.safety}`}>{SAFETY_LABEL[f.safety]}</span>
                        {f.stale_days != null && f.stale_days > 90 && (
                          <span className="badge stale">Unused {Math.round(f.stale_days / 30)} mo</span>
                        )}
                        {f.read_error && (
                          <span className="badge safety-risky" title={f.read_error}>
                            Partly unreadable
                          </span>
                        )}
                      </div>
                      <code className="finding-path">{f.path}</code>
                      <ul className="safety-reasons">
                        {f.reasons.map((r) => (
                          <li key={r.text} className={`reason-${r.level}`}>
                            <span className="reason-mark" aria-hidden>
                              {r.level === "ok" ? "✓" : r.level === "warn" ? "!" : "✕"}
                            </span>
                            {r.text}
                          </li>
                        ))}
                      </ul>
                      {f.size_bytes - f.reclaimable_bytes >= 1e8 && (
                        <p className="finding-desc">
                          {f.reclaimable_exact
                            ? `Clearing it frees ${formatBytes(f.reclaimable_bytes)}.`
                            : `Clearing it frees at most ${formatBytes(f.reclaimable_bytes)} — checking hard links for the exact figure…`}{" "}
                          {f.reclaimable_exact ? "" : "At least "}
                          {formatBytes(f.size_bytes - f.reclaimable_bytes)} of these files are also used
                          elsewhere through hard links (usually your virtual environments or projects),
                          so they stay on disk until those are removed too.
                        </p>
                      )}
                    </div>
                    <div className="finding-size">
                      <span className="size">{formatBytes(f.size_bytes)}</span>
                      <span className="files">{f.file_count.toLocaleString()} files</span>
                    </div>
                    <div className="finding-actions">
                      <button className="btn-ghost" onClick={() => reveal(f.path)}>Explore</button>
                      <button
                        className="btn-clean"
                        disabled={cleaning === f.path}
                        onClick={() => setConfirming(f)}
                      >
                        {cleaning === f.path ? "Cleaning…" : "Clean"}
                      </button>
                    </div>
                  </div>
                  {expanded === f.path && (
                    <div className="finding-detail">
                      <p className="finding-desc">{f.description}</p>
                      {f.children.length > 0 && (
                        <div className="children">
                          {f.children.map((c) => (
                            <div key={c.path} className="child">
                              <span className="child-name">{c.name}</span>
                              <div className="child-bar">
                                <div
                                  className="child-fill"
                                  style={{ width: `${(c.size_bytes / f.size_bytes) * 100}%` }}
                                />
                              </div>
                              <span className="child-size">{formatBytes(c.size_bytes)}</span>
                              <button className="btn-ghost sm" onClick={() => reveal(c.path)}>Show</button>
                            </div>
                          ))}
                        </div>
                      )}
                    </div>
                  )}
                </div>
              ))}
            </div>
          </section>
        </>
      )}
    </>
  );
}
