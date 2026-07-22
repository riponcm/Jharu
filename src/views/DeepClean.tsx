import { useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { formatBytes, isWindows } from "../format";
import ConfirmClean from "../components/ConfirmClean";
import Treemap from "../components/Treemap";
import type { CleanOutcome, DeepEntry } from "../types";

const KIND_LABEL: Record<DeepEntry["kind"], string> = {
  system: "System",
  applications: "Apps",
  user_data: "Your files",
  app_data: "App data",
  dev: "Dev / config",
  other: "Other",
};

const ROOT = "__root__";

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
      const outcome = await invoke<CleanOutcome>("clean_paths", { paths: [entry.path] });
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

  // A folder we could not fully read reports a size that is only a floor. Where
  // nothing at all could be read it measures 0 B, and a 0 B block cannot be
  // drawn — those have to be named explicitly or they simply vanish.
  const unreadable = entries.filter((e) => e.read_error);
  const undrawable = unreadable.filter((e) => e.size_bytes === 0);
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
            risky: confirming.kind === "user_data" || confirming.kind === "app_data",
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
          {scanning ? "Measuring…" : `${formatBytes(totalSize)} in ${entries.length} items`}
        </span>
      </div>

      <div className="filter-row">
        <div className="filter-tabs">
          <button
            className={`filter-tab ${mode === "map" ? "is-active" : ""}`}
            onClick={() => setMode("map")}
          >
            Map
          </button>
          <button
            className={`filter-tab ${mode === "list" ? "is-active" : ""}`}
            onClick={() => setMode("list")}
          >
            List
          </button>
        </div>
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
      </div>

      {mode === "map" && entries.length > 0 && (
        <>
          {unreadable.length > 0 && (
            <div className="banner-info">
              {unreadable.length} folder{unreadable.length === 1 ? "" : "s"} could not be read in
              full, so the size shown is a floor rather than the real total:{" "}
              <strong>{unreadable.map((e) => e.name).join(", ")}</strong>.
              {undrawable.length > 0 && (
                <>
                  {" "}
                  {undrawable.length} of {unreadable.length === 1 ? "them" : "those"} measured 0 B
                  and {undrawable.length === 1 ? "is" : "are"} not drawn on the map at all.
                </>
              )}{" "}
              {isWindows
                ? "Run Jharu as an administrator to measure them properly."
                : "Grant Full Disk Access in System Settings → Privacy and Security to measure them properly."}
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
            Each block is a folder, sized by what it holds. Click a block to go inside it.
            Blocks smaller than a fraction of a percent are left off the map — switch to List to see
            everything.
          </p>
        </>
      )}

      {mode === "list" && (
      <div className="deep-list">
        {entries.map((e) => (
          <div key={e.path} className={`deep-row ${e.protected ? "is-protected" : ""}`}>
            <button
              className="deep-name"
              onClick={() => navigate(e)}
              disabled={!e.is_dir}
              title={e.hint}
            >
              <span className="deep-icon">{e.is_dir ? "▸" : "·"}</span>
              {e.name}
            </button>
            <span className={`badge kind-${e.kind}`}>{KIND_LABEL[e.kind]}</span>
            {e.protected && <span className="badge safety-risky">Protected</span>}
            {e.read_error && (
              <span className="badge safety-risky" title={e.read_error}>
                Partly unreadable
              </span>
            )}
            <div className="deep-bar">
              <div
                className={`deep-fill kindfill-${e.kind}`}
                style={{ width: `${Math.max(1, (e.size_bytes / maxSize) * 100)}%` }}
              />
            </div>
            <span className="deep-size">{formatBytes(e.size_bytes)}</span>
            <span className="deep-files">{e.file_count.toLocaleString()} files</span>
            <div className="finding-actions">
              <button className="btn-ghost sm" onClick={() => reveal(e.path)}>Show</button>
              {!e.protected && (
                <button className="btn-clean sm" onClick={() => setConfirming(e)}>Clean</button>
              )}
            </div>
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
