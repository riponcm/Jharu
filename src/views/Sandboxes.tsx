import { useEffect, useMemo, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { formatBytes, isWindows } from "../format";
import ConfirmClean from "../components/ConfirmClean";
import type { Reason, Safety } from "../types";
import "../styles/sandboxes.css";

const BIN = isWindows ? "Recycle Bin" : "Trash";
/// Sandboxes smaller than this are folded away; there are over a hundred.
const TINY = 1024 * 1024;

interface SbEntry {
  path: string;
  rel: string;
  label: string;
  what: string;
  size_bytes: number;
  reclaimable_bytes: number;
  file_count: number;
  safety: Safety;
  basis: "layout" | "known" | "detected" | "unknown";
  reasons: Reason[];
  removable: boolean;
  mode: "folder" | "contents";
  preselect: boolean;
  appears_as: string | null;
  children: SbEntry[];
}

interface SbPackage {
  id: string;
  name: string;
  root: string;
  kind: "msix" | "container" | "group_container";
  state: "installed" | "orphaned" | "unknown";
  running: string[];
  install_path: string | null;
  version: string | null;
  system: boolean;
  size_bytes: number;
  safe_bytes: number;
  safety: Safety | null;
  reasons: Reason[];
  entries: SbEntry[];
}

interface SbReport {
  supported: boolean;
  platform: string;
  locations: string[];
  inventory_known: boolean;
  total_bytes: number;
  safe_bytes: number;
  orphan_bytes: number;
  cancelled: boolean;
  packages: SbPackage[];
}

interface SbOutcome {
  freed_bytes: number;
  removed: string[];
  partial: string[];
  already_gone: string[];
  skipped: string[];
  errors: string[];
  cancelled: boolean;
  not_attempted: number;
}

interface Progress {
  token: string;
  done: number;
  total: number;
  current: string;
}

type Filter = "all" | "left" | "clean";

const BASIS: Record<SbEntry["basis"], string> = {
  layout: "Sandbox folder",
  known: "Known location",
  detected: "Judged from what is inside",
  unknown: "Not recognised",
};

const STATE_LABEL: Record<SbPackage["state"], string> = {
  installed: "Installed",
  orphaned: "App removed",
  unknown: "Install state unknown",
};

function flatten(entries: SbEntry[]): SbEntry[] {
  return entries.flatMap((e) => [e, ...flatten(e.children)]);
}

function baseName(path: string): string {
  return path.split(/[\\/]/).filter(Boolean).pop() ?? path;
}

export default function Sandboxes({ onRecovered }: { onRecovered: (bytes: number) => void }) {
  const [report, setReport] = useState<SbReport | null>(null);
  const [scanning, setScanning] = useState(false);
  const [scanProgress, setScanProgress] = useState<Progress | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [filter, setFilter] = useState<Filter>("all");
  const [showTiny, setShowTiny] = useState(false);
  const [selected, setSelected] = useState<Set<string>>(new Set());
  const [open, setOpen] = useState<Set<string>>(new Set());
  const [collapsed, setCollapsed] = useState<Set<string>>(new Set());
  const [confirming, setConfirming] = useState(false);
  const [progress, setProgress] = useState<Progress | null>(null);
  const [stopping, setStopping] = useState(false);
  const [outcome, setOutcome] = useState<SbOutcome | null>(null);
  const scanToken = useRef<string | null>(null);
  const removeToken = useRef<string | null>(null);

  const packages = report?.packages ?? [];
  const index = useMemo(() => {
    const m = new Map<string, { pkg: SbPackage; entry: SbEntry }>();
    for (const pkg of packages) for (const entry of flatten(pkg.entries)) m.set(entry.path, { pkg, entry });
    return m;
  }, [packages]);

  const scan = async () => {
    const token = `sb-scan:${Math.random()}`;
    scanToken.current = token;
    setScanning(true);
    setStopping(false);
    setScanProgress(null);
    setError(null);
    try {
      const r = await invoke<SbReport>("sandbox_scan", { token });
      setReport(r);
      setSelected(new Set(r.packages.flatMap((p) => flatten(p.entries)).filter((e) => e.preselect).map((e) => e.path)));
      // The biggest sandboxes start open; the rest are one click away.
      setOpen(new Set(r.packages.filter((p) => p.size_bytes >= TINY).slice(0, 3).map((p) => p.root)));
      setCollapsed(new Set());
    } catch (e) {
      setError(String(e));
    } finally {
      scanToken.current = null;
      setScanning(false);
      setScanProgress(null);
      setStopping(false);
    }
  };

  useEffect(() => {
    scan();
    const unlisten: Promise<UnlistenFn>[] = [
      listen<Progress>("sandbox-scan-progress", (event) => {
        if (event.payload.token === scanToken.current) setScanProgress(event.payload);
      }),
      listen<Progress>("sandbox-progress", (event) => {
        if (event.payload.token === removeToken.current) setProgress(event.payload);
      }),
    ];
    return () => unlisten.forEach((u) => u.then((f) => f()));
  }, []);

  const stop = (token: string | null) => {
    if (!token) return;
    setStopping(true);
    invoke("cancel_sandbox_task", { token }).catch(() => {});
  };

  const toggle = (entry: SbEntry) => {
    if (!entry.removable) return;
    setSelected((prev) => {
      const next = new Set(prev);
      if (next.has(entry.path)) next.delete(entry.path);
      else next.add(entry.path);
      return next;
    });
  };

  const toggleIn = (setter: typeof setOpen, key: string) =>
    setter((prev) => {
      const next = new Set(prev);
      if (next.has(key)) next.delete(key);
      else next.add(key);
      return next;
    });

  const chosen = [...selected].map((p) => index.get(p)).filter((x): x is { pkg: SbPackage; entry: SbEntry } => !!x && x.entry.removable);
  const chosenBytes = chosen.reduce((a, c) => a + c.entry.reclaimable_bytes, 0);
  const chosenCareful = chosen.some((c) => c.entry.safety !== "safe");

  const removeSelected = async () => {
    setConfirming(false);
    setOutcome(null);
    setStopping(false);
    const token = `sb-rm:${Math.random()}`;
    removeToken.current = token;
    setProgress({ token, done: 0, total: chosen.length, current: "" });
    try {
      const result = await invoke<SbOutcome>("remove_sandbox_items", {
        items: chosen.map((c) => ({ package_root: c.pkg.root, path: c.entry.path, expected: c.entry.safety })),
        token,
      });
      setOutcome(result);
      onRecovered(result.freed_bytes);
      await scan();
      // After the rescan, which clears the error line.
      if (result.errors.length) setError(result.errors.join("; "));
    } catch (e) {
      setError(String(e));
    } finally {
      removeToken.current = null;
      setProgress(null);
      setStopping(false);
    }
  };

  const busy = progress != null || scanning;
  const cleanable = (p: SbPackage) => flatten(p.entries).some((e) => e.removable && e.reclaimable_bytes > 0);
  const leftCount = packages.filter((p) => p.state === "orphaned").length;
  const cleanCount = packages.filter(cleanable).length;
  const tinyCount = packages.filter((p) => p.size_bytes < TINY).length;
  const visible = packages.filter((p) => {
    if (!showTiny && p.size_bytes < TINY) return false;
    if (filter === "left") return p.state === "orphaned";
    if (filter === "clean") return cleanable(p);
    return true;
  });

  const label = (entry: SbEntry, pkg: SbPackage) =>
    `${pkg.name} · ${entry.rel}${entry.mode === "contents" ? " (what is inside)" : ""}`;

  const renderEntry = (entry: SbEntry, pkg: SbPackage, depth: number) => {
    const hasKids = entry.children.length > 0;
    const isCollapsed = collapsed.has(entry.path);
    return (
      <div key={entry.path}>
        <label className={`model-row wt-row sb-row ${entry.removable ? "" : "is-locked"}`} style={{ paddingLeft: 16 + depth * 22 }}>
          <input
            type="checkbox"
            checked={selected.has(entry.path)}
            disabled={!entry.removable || busy}
            onChange={() => toggle(entry)}
            title={entry.removable ? undefined : "Jharu does not offer to remove this one"}
          />
          <div className="model-main">
            <div className="finding-title">
              <span className="finding-name">{entry.label}</span>
              <span className={`badge safety-${entry.safety}`}>
                {entry.safety === "safe" ? "Safe" : entry.safety === "caution" ? "Caution" : "Risky"}
              </span>
              <span className="model-meta">
                {entry.what} · {BASIS[entry.basis]}
              </span>
            </div>
            <code className="finding-path">{entry.rel}</code>
            <ul className="safety-reasons">
              {entry.reasons.map((r) => (
                <li key={r.text} className={`reason-${r.level}`}>
                  <span className="reason-mark" aria-hidden>
                    {r.level === "ok" ? "✓" : r.level === "warn" ? "!" : "✕"}
                  </span>
                  {r.text}
                </li>
              ))}
            </ul>
            {entry.appears_as && (
              <p className="model-meta sb-sees">
                The app itself sees this folder as <code>{entry.appears_as}</code>
              </p>
            )}
            {entry.removable && entry.mode === "contents" && (
              <p className="model-meta sb-sees">
                Jharu moves what is inside to the {BIN} and keeps the folder, which the app expects to find.
              </p>
            )}
            {hasKids && (
              <button
                className="btn-ghost sm sb-toggle"
                onClick={(e) => {
                  e.preventDefault();
                  toggleIn(setCollapsed, entry.path);
                }}
              >
                {isCollapsed ? `Show the ${entry.children.length} parts` : "Hide the parts"}
              </button>
            )}
          </div>
          <div className="finding-size">
            <span className="size">{formatBytes(entry.size_bytes)}</span>
          </div>
          <button
            className="btn-ghost sm"
            onClick={(e) => {
              e.preventDefault();
              invoke("reveal_path", { path: entry.path }).catch((err) => setError(String(err)));
            }}
          >
            Show
          </button>
        </label>
        {hasKids && !isCollapsed && entry.children.map((c) => renderEntry(c, pkg, depth + 1))}
      </div>
    );
  };

  const renderBar = (pkg: SbPackage) => {
    const top = [...pkg.entries].filter((e) => e.size_bytes > 0).slice(0, 12);
    if (pkg.size_bytes === 0 || top.length === 0) return null;
    return (
      <div className="sb-bar" aria-hidden>
        {top.map((e) => (
          <div
            key={e.path}
            className={`sb-seg sb-seg-${e.safety}`}
            style={{ width: `${Math.max(0.6, (e.size_bytes / pkg.size_bytes) * 100)}%` }}
            title={`${e.label}: ${formatBytes(e.size_bytes)}`}
          />
        ))}
      </div>
    );
  };

  const title = `Move ${chosen.length} folder${chosen.length === 1 ? "" : "s"} to the ${BIN}?`;

  return (
    <>
      {confirming && (
        <ConfirmClean
          request={{
            kind: "sandbox",
            title,
            items: chosen.map((c) => label(c.entry, c.pkg)),
            bytes: chosenBytes,
            risky: chosenCareful,
          }}
          busy={progress != null}
          onConfirm={removeSelected}
          onCancel={() => setConfirming(false)}
        />
      )}

      <div className="view-head">
        <div>
          <h2 className="view-title">App sandboxes</h2>
          <p className="view-sub">
            {isWindows
              ? "The private copies of AppData that Store and packaged apps keep. Explorer and most cleaners never show them."
              : "The private containers that sandboxed Mac apps keep in ~/Library/Containers and Group Containers."}
          </p>
        </div>
        <button className="btn" onClick={scan} disabled={busy}>
          {scanning ? "Scanning…" : "Rescan"}
        </button>
      </div>

      {error && (
        <div className="banner-error">
          <span>{error}</span>
          <button className="btn-ghost" onClick={() => setError(null)}>Dismiss</button>
        </div>
      )}

      {scanning && (
        <div className="banner-info wt-progress">
          <div className="meter-head">
            <span>
              Measuring app sandboxes
              {scanProgress?.current && ` · ${scanProgress.current}`}
            </span>
            <span className="meter-value">
              {scanProgress ? `${Math.min(scanProgress.done, scanProgress.total)} of ${scanProgress.total}` : ""}
            </span>
          </div>
          <div className="meter-track">
            <div
              className="meter-fill"
              style={{
                width: `${Math.max(3, scanProgress ? (scanProgress.done / (scanProgress.total || 1)) * 100 : 0)}%`,
                background: "var(--accent)",
              }}
            />
          </div>
          <div className="sb-progress-foot">
            <p className="model-meta">A sandbox can hold tens of gigabytes, so this can take a minute.</p>
            <button className="btn-ghost sm" disabled={stopping} onClick={() => stop(scanToken.current)}>
              {stopping ? "Stopping…" : "Stop"}
            </button>
          </div>
        </div>
      )}

      {progress && (
        <div className="banner-info wt-progress">
          <div className="meter-head">
            <span>
              Moving sandbox folders to the {BIN}
              {progress.current && ` · ${progress.current}`}
            </span>
            <span className="meter-value">
              {Math.min(progress.done, progress.total)} of {progress.total}
            </span>
          </div>
          <div className="meter-track">
            <div
              className="meter-fill"
              style={{ width: `${Math.max(3, (progress.done / (progress.total || 1)) * 100)}%`, background: "var(--accent)" }}
            />
          </div>
          <div className="sb-progress-foot">
            <p className="model-meta">
              Jharu finishes the folder it is on, then stops. What was already moved stays in the {BIN}.
            </p>
            <button className="btn-ghost sm" disabled={stopping} onClick={() => stop(removeToken.current)}>
              {stopping ? "Stopping…" : "Cancel"}
            </button>
          </div>
        </div>
      )}

      {outcome && !progress && (
        <div className="banner-info">
          {outcome.removed.length > 0 && (
            <>
              Moved {outcome.removed.length} folder{outcome.removed.length === 1 ? "" : "s"} to the {BIN} (
              {formatBytes(outcome.freed_bytes)} freed).{" "}
            </>
          )}
          {outcome.cancelled && (
            <>Stopped at your request; {outcome.not_attempted} not touched. </>
          )}
          {outcome.already_gone.length > 0 && <>{outcome.already_gone.length} had already disappeared. </>}
          {outcome.partial.length > 0 && (
            <>
              Emptied only in part:
              <ul className="wt-reasons">
                {outcome.partial.map((s) => (
                  <li key={s}>{s}</li>
                ))}
              </ul>
            </>
          )}
          {outcome.skipped.length > 0 && (
            <>
              Skipped {outcome.skipped.length}:
              <ul className="wt-reasons">
                {outcome.skipped.map((s) => (
                  <li key={s}>{s}</li>
                ))}
              </ul>
            </>
          )}
          {outcome.removed.length === 0 &&
            outcome.partial.length === 0 &&
            outcome.skipped.length === 0 &&
            outcome.already_gone.length === 0 &&
            !outcome.cancelled &&
            "Nothing needed moving."}
        </div>
      )}

      {scanning && !report && (
        <div className="empty-state">
          <div className="spinner" />
          <p>Listing installed apps and measuring each one's private folders…</p>
        </div>
      )}

      {report && !report.supported && (
        <div className="banner-info">App sandboxes of this kind exist on Windows (Store apps) and macOS only.</div>
      )}

      {report && report.supported && (
        <>
          {report.cancelled && (
            <div className="banner-info">The scan was stopped, so this list holds only what was measured.</div>
          )}
          {!report.inventory_known && (
            <div className="banner-info">
              Jharu could not read the list of installed apps, so it does not say which sandboxes belong to apps
              you removed.
            </div>
          )}

          <section className="metrics">
            <div className="metric">
              <span className="metric-label">In app sandboxes</span>
              <span className="metric-value">{formatBytes(report.total_bytes)}</span>
            </div>
            <div className="metric accent">
              <span className="metric-label">Safe to clear</span>
              <span className="metric-value">{formatBytes(report.safe_bytes)}</span>
            </div>
            <div className="metric">
              <span className="metric-label">Left by removed apps</span>
              <span className="metric-value">{formatBytes(report.orphan_bytes)}</span>
            </div>
          </section>

          <div className="banner-info">
            {isWindows ? (
              <>
                Windows runs Store and MSIX apps in a sandbox: what an app believes is its{" "}
                <code>AppData\Local</code> and <code>AppData\Roaming</code> is really a private copy in{" "}
                <code>LocalCache</code> inside its package folder. An app with this virtualization sees these
                files as its normal AppData, so a cache here is rated like the real one. Green below means
                rebuilt by the app, amber needs a look, red is the app's own data.
              </>
            ) : (
              <>
                A sandboxed Mac app keeps its files under <code>~/Library/Containers/&lt;bundle id&gt;/Data</code>,
                and a developer's apps share <code>~/Library/Group Containers</code>. Caches there are rebuilt by
                the app; Application Support and Documents are its own data.
              </>
            )}
          </div>

          <div className="filter-row">
            <div className="filter-tabs">
              <button className={`filter-tab ${filter === "all" ? "is-active" : ""}`} onClick={() => setFilter("all")}>
                All ({packages.length})
              </button>
              <button className={`filter-tab ${filter === "left" ? "is-active" : ""}`} onClick={() => setFilter("left")}>
                App removed ({leftCount})
              </button>
              <button className={`filter-tab ${filter === "clean" ? "is-active" : ""}`} onClick={() => setFilter("clean")}>
                Can free space ({cleanCount})
              </button>
              {tinyCount > 0 && (
                <label className="sb-tiny">
                  <input type="checkbox" checked={showTiny} onChange={(e) => setShowTiny(e.target.checked)} /> Show {tinyCount}{" "}
                  under 1 MB
                </label>
              )}
            </div>
            <div className="confirm">
              {chosen.length > 0 && (
                <button className="btn-clean" disabled={busy} onClick={() => setConfirming(true)}>
                  Move {chosen.length} selected ({formatBytes(chosenBytes)})
                </button>
              )}
            </div>
          </div>

          {visible.length === 0 && (
            <div className="empty-state">
              <p>No app sandboxes match.</p>
            </div>
          )}

          {visible.map((pkg) => {
            const isOpen = open.has(pkg.root);
            return (
              <section key={pkg.root} className="sb-card">
                <div className="sb-head">
                  <button
                    className="expander"
                    aria-label={isOpen ? "Collapse" : "Expand"}
                    aria-expanded={isOpen}
                    onClick={() => toggleIn(setOpen, pkg.root)}
                  >
                    {isOpen ? "▾" : "▸"}
                  </button>
                  <div className="sb-title">
                    <div className="finding-title">
                      <span className="finding-name">{pkg.name}</span>
                      <span className={`badge ${pkg.state === "orphaned" ? "safety-caution" : "stale"}`}>
                        {STATE_LABEL[pkg.state]}
                      </span>
                      {pkg.running.length > 0 && <span className="badge safety-caution">Running</span>}
                      {pkg.system && <span className="badge stale">{isWindows ? "Windows component" : "System app"}</span>}
                    </div>
                    <span className="model-meta">
                      {baseName(pkg.root)}
                      {pkg.version && ` · version ${pkg.version}`}
                    </span>
                  </div>
                  <div className="finding-size">
                    <span className="size">{formatBytes(pkg.size_bytes)}</span>
                    {pkg.safe_bytes > 0 && <span className="model-meta">{formatBytes(pkg.safe_bytes)} safe to clear</span>}
                  </div>
                  <button className="btn-ghost sm" onClick={() => invoke("reveal_path", { path: pkg.root }).catch((e) => setError(String(e)))}>
                    Show
                  </button>
                </div>
                {renderBar(pkg)}
                {isOpen && (
                  <>
                    {pkg.reasons.length > 0 && (
                      <ul className="safety-reasons sb-pkg-reasons">
                        {pkg.reasons.map((r) => (
                          <li key={r.text} className={`reason-${r.level}`}>
                            <span className="reason-mark" aria-hidden>
                              {r.level === "ok" ? "✓" : r.level === "warn" ? "!" : "✕"}
                            </span>
                            {r.text}
                          </li>
                        ))}
                      </ul>
                    )}
                    <div className="findings">{pkg.entries.map((e) => renderEntry(e, pkg, 0))}</div>
                  </>
                )}
              </section>
            );
          })}

          <p className="footnote">
            Jharu moves only the folders it rates, never a whole sandbox, and checks each one again just before
            moving it. It skips anything whose app is running and tells you why. Folders an app expects to find
            (temporary files, web cache) keep their place; only what is inside moves. Risky folders hold the
            app's own data and are shown for information only.
          </p>
        </>
      )}
    </>
  );
}
