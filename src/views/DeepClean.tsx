import { useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { formatBytes } from "../format";
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
  const [confirming, setConfirming] = useState<string | null>(null);
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

  const maxSize = entries.length ? Math.max(...entries.map((e) => e.size_bytes)) : 1;
  const totalSize = entries.reduce((a, e) => a + e.size_bytes, 0);

  return (
    <>
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
              {!e.protected &&
                (confirming === e.path ? (
                  <span className="confirm">
                    <button className="btn-danger" onClick={() => clean(e)}>Trash</button>
                    <button className="btn-ghost sm" onClick={() => setConfirming(null)}>Cancel</button>
                  </span>
                ) : (
                  <button className="btn-clean sm" onClick={() => setConfirming(e.path)}>Clean</button>
                ))}
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

      <p className="footnote">
        System locations are protected and can never be cleaned from Jharu. If some folders
        show 0 B, grant Full Disk Access in System Settings → Privacy &amp; Security.
      </p>
    </>
  );
}
