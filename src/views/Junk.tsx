import { useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { formatBytes, isWindows } from "../format";
import ConfirmClean from "../components/ConfirmClean";
import type { CleanOutcome, JunkGroup, JunkItem, LocationOutcome } from "../types";

const BIN = isWindows ? "Recycle Bin" : "Trash";

/// Plain-language summary of a contents clean, or null when there is nothing
/// worth saying beyond the error banner.
function describe(o: LocationOutcome, adminLabels: string[]): string | null {
  const parts: string[] = [];
  if (o.moved > 0) {
    parts.push(
      `Moved ${o.moved.toLocaleString()} item${o.moved === 1 ? "" : "s"} (${formatBytes(o.freed_bytes)}) to the ${BIN}.`
    );
  }
  if (o.left > 0) {
    const names = o.left_names.length ? ` (${o.left_names.join(", ")}${o.left > o.left_names.length ? ", …" : ""})` : "";
    parts.push(
      `${o.left.toLocaleString()} ${o.left === 1 ? "item is" : "items are"} open in a running program and stayed${names}. They usually clear after a restart.`
    );
  }
  if (o.recent > 0) {
    parts.push(
      `${o.recent.toLocaleString()} ${o.recent === 1 ? "item" : "items"} changed in the last day ${o.recent === 1 ? "was" : "were"} left alone, in case a program still needs ${o.recent === 1 ? "it" : "them"}.`
    );
  }
  if (o.admin_declined) {
    parts.push(
      `Administrator permission was declined, so ${adminLabels.join(", ") || "the system folders"} ${adminLabels.length === 1 ? "was" : "were"} left as ${adminLabels.length === 1 ? "it was" : "they were"}.`
    );
  }
  return parts.length ? parts.join(" ") : null;
}

export default function Junk({ onRecovered }: { onRecovered: (bytes: number) => void }) {
  const [groups, setGroups] = useState<JunkGroup[] | null>(null);
  const [scanning, setScanning] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [confirming, setConfirming] = useState<string | null>(null);
  const [cleaning, setCleaning] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  // The card the last clean started from: progress and the summary show
  // there, next to the button that was pressed, not at the top of the page.
  const [activeGroup, setActiveGroup] = useState<string | null>(null);
  const [progress, setProgress] = useState<{ label: string; done: number; total: number } | null>(null);
  const activeToken = useRef<string | null>(null);

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
    let unlisten: UnlistenFn | undefined;
    listen<{ token: string; label: string; done: number; total: number }>("clean-progress", (event) => {
      if (event.payload.token !== activeToken.current) return;
      setProgress(event.payload);
    }).then((u) => (unlisten = u));
    return () => unlisten?.();
  }, []);

  const clean = async (key: string, items: JunkItem[]) => {
    setCleaning(key);
    setActiveGroup(groups?.find((g) => g.id === key || g.items.some((i) => i.path === key))?.id ?? null);
    setConfirming(null);
    setNotice(null);
    // Folders the system keeps using are emptied, never moved whole; plain
    // files and folders move as one piece.
    const locations = items.filter((i) => i.location);
    const paths = items.filter((i) => !i.location).map((i) => i.path);
    try {
      const errors: string[] = [];
      let freed = 0;
      if (paths.length) {
        const outcome = await invoke<CleanOutcome>("clean_paths", { paths, source: "Junk files" });
        errors.push(...outcome.errors);
        freed += outcome.freed_bytes;
      }
      if (locations.length) {
        const token = `clean:${Math.random()}`;
        activeToken.current = token;
        setProgress({ label: locations[0].location!, done: 0, total: 0 });
        const outcome = await invoke<LocationOutcome>("clean_locations", {
          ids: locations.map((i) => i.location),
          token,
        });
        errors.push(...outcome.errors);
        freed += outcome.freed_bytes;
        setNotice(describe(outcome, locations.filter((i) => i.needs_admin).map((i) => i.label)));
      }
      if (errors.length) setError(errors.join("; "));
      onRecovered(freed);
      await scan();
    } catch (e) {
      setError(String(e));
    } finally {
      activeToken.current = null;
      setProgress(null);
      setCleaning(null);
    }
  };

  const progressLabel = (label: string) =>
    label === "admin"
      ? "Waiting for the administrator helper — approve the Windows prompt if it is showing"
      : groups?.flatMap((g) => g.items).find((i) => i.location === label)?.label ?? label;

  const total = groups?.filter((g) => g.cleanable).reduce((a, g) => a + g.total_bytes, 0) ?? 0;

  // A pending confirm is either a whole group's safe items, or one item.
  const pendingGroup = groups?.find((g) => g.id === confirming);
  const pendingItem: JunkItem | undefined = groups
    ?.flatMap((g) => g.items)
    .find((i) => i.path === confirming);
  const pendingSafe = pendingGroup?.items.filter((i) => i.safe) ?? [];
  const pendingItems = pendingGroup ? pendingSafe : pendingItem ? [pendingItem] : [];
  const pendingKind = pendingItems.length && pendingItems.every((i) => i.location) ? "system" : "userfile";
  const pendingAdmin = pendingItems.filter((i) => i.needs_admin).map((i) => i.label);

  const status = (
    <>
      {progress && (
        <div className="banner-info wt-progress">
          <div className="meter-head">
            <span>{progressLabel(progress.label)}</span>
            {progress.total > 0 && (
              <span className="meter-value">
                {progress.done.toLocaleString()} of {progress.total.toLocaleString()} items
              </span>
            )}
          </div>
          {progress.total > 0 && (
            <div className="meter-track">
              <div
                className="meter-fill"
                style={{
                  width: `${Math.max(3, (progress.done / progress.total) * 100)}%`,
                  background: "var(--accent)",
                }}
              />
            </div>
          )}
          <p className="model-meta">
            Folders with many small files take a while to move to the {BIN}.
          </p>
        </div>
      )}

      {notice && (
        <div className="banner-info">
          {notice}{" "}
          <button className="btn-ghost sm" onClick={() => setNotice(null)}>Dismiss</button>
        </div>
      )}
    </>
  );

  return (
    <>
      {(pendingGroup || pendingItem) && (
        <ConfirmClean
          request={
            pendingGroup
              ? {
                  kind: pendingKind,
                  title: `Clean ${pendingGroup.title}?`,
                  items: pendingSafe.map((i) => i.label),
                  bytes: pendingSafe.reduce((a, i) => a + i.size_bytes, 0),
                  risky: false,
                  admin: pendingAdmin,
                }
              : {
                  kind: pendingKind,
                  title: pendingItem!.location
                    ? `Clean out ${pendingItem!.label}?`
                    : `Move ${pendingItem!.label} to the ${BIN}?`,
                  items: [pendingItem!.path],
                  bytes: pendingItem!.size_bytes,
                  risky: !pendingItem!.safe,
                  admin: pendingAdmin,
                }
          }
          busy={cleaning === confirming}
          onConfirm={() =>
            pendingGroup
              ? clean(pendingGroup.id, pendingSafe)
              : clean(pendingItem!.path, [pendingItem!])
          }
          onCancel={() => setConfirming(null)}
        />
      )}

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
                <button
                  className="btn-clean"
                  disabled={cleaning != null}
                  onClick={() => setConfirming(g.id)}
                >
                  {cleaning === g.id ? "Cleaning…" : `Clean safe items (${formatBytes(safeTotal)})`}
                </button>
              )}
            </div>
            {g.id === activeGroup && <div className="junk-status">{status}</div>}
            <div className="browser-items">
              {g.items.map((i) => (
                <div key={i.path} className="junk-item">
                  <span className="child-name" title={i.path}>{i.label}</span>
                  <span className="junk-badges">
                    <span className={`badge ${i.safe ? "safety-safe" : "safety-caution"}`}>
                      {i.safe ? "Safe" : "Review"}
                    </span>
                    {i.stale_days != null && i.stale_days > 30 && (
                      <span className="badge stale">{Math.round(i.stale_days / 30)} mo old</span>
                    )}
                    {i.needs_admin && (
                      <span className="badge stale" title="Windows asks for administrator permission when you clean this">
                        Admin
                      </span>
                    )}
                    {i.read_error && (
                      <span className="badge safety-risky" title={i.read_error}>
                        Partly unreadable
                      </span>
                    )}
                  </span>
                  <span className="child-size">{formatBytes(i.size_bytes)}</span>
                  <button
                    className="btn-ghost sm"
                    onClick={() => invoke("reveal_path", { path: i.path }).catch((e) => setError(String(e)))}
                  >
                    Show
                  </button>
                  {g.cleanable && (
                    <button
                      className="btn-clean sm"
                      disabled={cleaning != null}
                      onClick={() => setConfirming(i.path)}
                    >
                      {cleaning === i.path ? "Cleaning…" : "Clean"}
                    </button>
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
