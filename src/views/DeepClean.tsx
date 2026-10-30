import { useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { formatBytes, isWindows } from "../format";
import ConfirmClean from "../components/ConfirmClean";
import Treemap from "../components/Treemap";
import type { CleanOutcome, DeepEntry, Verdict } from "../types";

const KIND_LABEL: Record<DeepEntry["kind"], string> = {
  system: "System",
  applications: "Apps",
  user_data: "Your files",
  app_data: "App data",
  dev: "Dev / config",
  other: "Other",
};

const ROOT = "__root__";

function verdictLabel(v: Verdict): string {
  if (v.basis === "unknown") return "Unknown — look inside";
  if (v.safety === "safe") return "Safe to remove";
  if (v.safety === "caution") return "Remove with care";
  return "Review first";
}

const BASIS_HINT: Record<Verdict["basis"], string> = {
  known: "Jharu knows this location.",
  detected: "Judged from what the folder contains.",
  unknown: "Jharu does not recognise this folder.",
};

export default function DeepClean({ onRecovered }: { onRecovered: (bytes: number) => void }) {
  const [crumbs, setCrumbs] = useState<string[]>([]);
  const [entries, setEntries] = useState<DeepEntry[]>([]);
  const [scanning, setScanning] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [confirming, setConfirming] = useState<DeepEntry | null>(null);
  const [mode, setMode] = useState<"map" | "list">("map");
  const cache = useRef(new Map<string, DeepEntry[]>());
  const activeToken = useRef("");

  const currentPath = crumbs.length ? crumbs[crumbs.length - 1] : null;

  const scan = async (path: string | null, force = false) => {
    const key = path ?? ROOT;
    if (!force && cache.current.has(key)) {
      // Leaving a level that is still being measured: stop listening to it,
      // or its rows would land in this list and "Measuring…" would stay up.
      activeToken.current = "";
      setScanning(false);
      setEntries(cache.current.get(key)!);
      return;
    }
    const token = `${key}:${Math.random()}`;
    activeToken.current = token;
    setEntries([]);
    setScanning(true);
    setError(null);
    try {
      const result = await invoke<DeepEntry[]>("deep_scan", { path, token });
      if (activeToken.current === token) {
        cache.current.set(key, result);
        setEntries(result);
      }
    } catch (e) {
      if (activeToken.current === token) setError(String(e));
    } finally {
      if (activeToken.current === token) setScanning(false);
    }
  };

  useEffect(() => {
    let unlisten: UnlistenFn | undefined;
    listen<{ token: string; entry: DeepEntry }>("deep-entry", (event) => {
      if (event.payload.token !== activeToken.current) return;
      setEntries((prev) =>
        [...prev.filter((p) => p.path !== event.payload.entry.path), event.payload.entry].sort(
          (a, b) => b.size_bytes - a.size_bytes
        )
      );
    }).then((u) => (unlisten = u));
    scan(null);
    return () => unlisten?.();
  }, []);

  const navigate = (entry: DeepEntry) => {
    if (!entry.is_dir) return;
    const next = [...crumbs, entry.path];
    setCrumbs(next);
    scan(entry.path);
  };

  const jumpTo = (index: number) => {
    const next = crumbs.slice(0, index + 1);
    setCrumbs(next);
    scan(next.length ? next[next.length - 1] : null);
  };

  const clean = async (entry: DeepEntry) => {
    setConfirming(null);
    try {
      const outcome = await invoke<CleanOutcome>("clean_paths", { paths: [entry.path], source: "Deep clean" });
      if (outcome.errors.length) setError(outcome.errors.join("; "));
      onRecovered(outcome.freed_bytes);
      const key = currentPath ?? ROOT;
      const updated = entries.filter((e) => e.path !== entry.path);
      cache.current.set(key, updated);
      setEntries(updated);
    } catch (e) {
      setError(String(e));
    }
  };

  const reveal = (path: string) =>
    invoke("reveal_path", { path }).catch((e) => setError(String(e)));

  // A folder Jharu could not read measures less than it holds, maybe 0 B, so
  // the map draws it small or not at all: name it rather than let it vanish.
  const unreadable = entries.filter((e) => e.unreadable > 0);
  const maxSize = entries.length ? Math.max(...entries.map((e) => e.size_bytes)) : 1;
  const totalSize = entries.reduce((a, e) => a + e.size_bytes, 0);

  return (
    <>
      {confirming && (
        <ConfirmClean
          request={{
            kind: confirming.kind === "user_data" ? "userfile" : "cache",
            title: `Move ${confirming.name} to the Trash?`,
            items: [confirming.path],
            bytes: confirming.size_bytes,
            risky: confirming.verdict ? confirming.verdict.safety !== "safe" : true,
          }}
          onConfirm={() => clean(confirming)}
          onCancel={() => setConfirming(null)}
        />
      )}

      <div className="view-head">
        <div>
          <h2 className="view-title">Deep clean</h2>
          <p className="view-sub">
            Full-disk analysis — see what every folder actually holds
          </p>
        </div>
        <button className="btn" onClick={() => scan(currentPath, true)} disabled={scanning}>
          {scanning ? "Scanning…" : "Rescan"}
        </button>
      </div>

      {error && (
        <div className="banner-error">
          <span>{error}</span>
          <button className="btn-ghost" onClick={() => setError(null)}>Dismiss</button>
        </div>
      )}

      <div className="crumbs">
        <button className="crumb" onClick={() => jumpTo(-1)}>This computer</button>
        {crumbs.map((c, i) => (
          <span key={c}>
            <span className="crumb-sep">/</span>
            <button className="crumb" onClick={() => jumpTo(i)}>
              {c.split("/").filter(Boolean).pop() ?? c}
            </button>
          </span>
        ))}
        <span className="crumb-total">
          {scanning
            ? "Measuring…"
            : `${formatBytes(totalSize)} in ${entries.filter((e) => !e.is_virtual).length} items`}
        </span>
      </div>

      <div className="filter-row">
        <div className="filter-tabs">
          <button
            className={`filter-tab ${mode === "map" ? "is-active" : ""}`}
            aria-pressed={mode === "map"}
            onClick={() => setMode("map")}
          >
            Map
          </button>
          <button
            className={`filter-tab ${mode === "list" ? "is-active" : ""}`}
            aria-pressed={mode === "list"}
            onClick={() => setMode("list")}
          >
            List
          </button>
        </div>
        {mode === "list" && (
          <div className="legend" style={{ margin: 0 }}>
            {[
              ["kindfill-user_data", "Your files"],
              ["kindfill-applications", "Apps"],
              ["kindfill-app_data", "App data"],
              ["kindfill-dev", "Dev / config"],
              ["kindfill-system", "System"],
            ].map(([cls, label]) => (
              <span key={label} className="legend-item">
                <i className={`dot ${cls}`} />
                {label}
              </span>
            ))}
          </div>
        )}
      </div>

      {mode === "map" && entries.length > 0 && (
        <>
          {unreadable.length > 0 && !scanning && (
            <div className="banner-info">
              Jharu could not read everything in {unreadable.length} folder
              {unreadable.length === 1 ? "" : "s"}, so {unreadable.length === 1 ? "it is" : "they are"}{" "}
              smaller on the map than on disk, or missing from it:{" "}
              <strong>{unreadable.map((e) => e.name).join(", ")}</strong>.{" "}
              {isWindows
                ? "Run Jharu as an administrator to measure them."
                : "Grant Full Disk Access in System Settings → Privacy and Security to measure them."}
            </div>
          )}
          {scanning && (
            <div className="map-scanning">
              <div className="spinner sm" />
              <span className="view-sub">Measuring — blocks appear as each folder finishes…</span>
            </div>
          )}
          <Treemap entries={entries} onOpen={navigate} />
          <p className="footnote">
            Each block is a folder, sized by what it holds and coloured by how safe it is to remove,
            the same rating the List shows. Click a block (or Tab to it and press Enter) to go inside.
            Hatched grey stands for items too small to draw — switch to List to see everything and to
            clean.
          </p>
        </>
      )}

      {mode === "list" && (
      <div className="deep-list">
        {entries.map((e) => (
          <div key={e.path} className="deep-item">
          <div className={`deep-row ${e.protected ? "is-protected" : ""}`}>
            <button
              className="deep-name"
              onClick={() => navigate(e)}
              disabled={!e.is_dir}
              title={e.hint}
            >
              <span className="deep-icon">{e.is_dir ? "▸" : "·"}</span>
              {e.name}
            </button>
            {e.verdict ? (
              <span
                className={`badge safety-${e.verdict.basis === "unknown" ? "caution" : e.verdict.safety}`}
                title={`${BASIS_HINT[e.verdict.basis]} ${e.verdict.reasons.map((r) => r.text).join(" ")}`}
              >
                {verdictLabel(e.verdict)}
              </span>
            ) : (
              <span className={`badge kind-${e.kind}`}>{KIND_LABEL[e.kind]}</span>
            )}
            {e.protected && !e.is_virtual && <span className="badge safety-risky">Protected</span>}
            {e.is_virtual && (
              <span className="badge stale" title={e.hint}>
                Needs admin to see
              </span>
            )}
            {e.unreadable > 0 && (
              <span
                className="badge stale"
                title={`${e.unreadable.toLocaleString()} ${e.unreadable === 1 ? "item" : "items"} inside could not be read, so the size shown is less than the folder holds.`}
              >
                Partly unreadable
              </span>
            )}
            {e.online_only_bytes >= 1e8 && (
              <span
                className="badge cat-dev_cache"
                title="Cloud files (OneDrive, iCloud) that are listed here but stored only online. They use no space on this disk and are not in the size shown."
              >
                +{formatBytes(e.online_only_bytes)} online-only
              </span>
            )}
            {e.shared_bytes >= 1e8 && (
              <span
                className="badge stale"
                title="Files here that are hard links to files already counted in another folder, so they take no extra space and are not in the size shown. Removing only this folder would not free them."
              >
                +{formatBytes(e.shared_bytes)} shared
              </span>
            )}
            <div className="deep-bar">
              <div
                className={`deep-fill kindfill-${e.kind}`}
                style={{ width: `${Math.max(1, (e.size_bytes / maxSize) * 100)}%` }}
              />
            </div>
            <span className="deep-size">{formatBytes(e.size_bytes)}</span>
            <span className="deep-files">
              {e.is_virtual ? "" : `${e.file_count.toLocaleString()} files`}
            </span>
            <div className="finding-actions">
              {!e.is_virtual && (
                <button className="btn-ghost sm" onClick={() => reveal(e.path)}>Show</button>
              )}
              {!e.protected && (e.verdict?.clean_here ?? true) && (
                <button className="btn-clean sm" onClick={() => setConfirming(e)}>Clean</button>
              )}
            </div>
          </div>
          {e.verdict && (
            <ul className="safety-reasons deep-why">
              {e.verdict.what && <li className="deep-what">{e.verdict.what}</li>}
              {e.verdict.reasons.map((r) => (
                <li key={r.text} className={`reason-${r.level}`}>
                  <span className="reason-mark" aria-hidden>
                    {r.level === "ok" ? "✓" : r.level === "warn" ? "!" : "✕"}
                  </span>
                  {r.text}
                </li>
              ))}
            </ul>
          )}
          </div>
        ))}
        {scanning && (
          <div className="deep-row scanning-row">
            <div className="spinner sm" />
            <span className="view-sub">Measuring folders — large ones appear as they finish…</span>
          </div>
        )}
      </div>
      )}

      <p className="footnote">
        System locations are protected and can never be cleaned from Jharu.{" "}
        {isWindows
          ? "If some folders show 0 B, they need administrator rights to read — run Jharu as an administrator to measure them."
          : "If some folders show 0 B, grant Full Disk Access in System Settings → Privacy & Security."}
      </p>
    </>
  );
}
