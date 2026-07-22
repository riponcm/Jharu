import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { formatBytes } from "../format";
import ConfirmClean from "../components/ConfirmClean";
import type { CleanOutcome, DesktopReport, MoveOutcome } from "../types";

type AgeFilter = "all" | "30" | "90";
type TypeFilter = "all" | "screenshots" | "other";

export default function Desktop({ onRecovered }: { onRecovered: (bytes: number) => void }) {
  const [report, setReport] = useState<DesktopReport | null>(null);
  const [scanning, setScanning] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [age, setAge] = useState<AgeFilter>("all");
  const [type, setType] = useState<TypeFilter>("screenshots");
  const [selected, setSelected] = useState<Set<string>>(new Set());
  const [confirmTrash, setConfirmTrash] = useState(false);
  const [working, setWorking] = useState(false);

  const scan = async () => {
    setScanning(true);
    setError(null);
    setSelected(new Set());
    try {
      setReport(await invoke<DesktopReport>("desktop_scan"));
    } catch (e) {
      setError(String(e));
    } finally {
      setScanning(false);
    }
  };

  useEffect(() => {
    scan();
  }, []);

  const shots = (report?.items ?? []).filter((i) => {
    if (type === "screenshots" && !i.is_screenshot) return false;
    if (type === "other" && i.is_screenshot) return false;
    if (age === "all") return true;
    return (i.age_days ?? 0) >= Number(age);
  });

  const otherCount = (report?.items ?? []).filter((i) => !i.is_screenshot).length;

  const toggle = (path: string) => {
    setSelected((prev) => {
      const next = new Set(prev);
      if (next.has(path)) next.delete(path);
      else next.add(path);
      return next;
    });
  };

  const selectedItems = (report?.items ?? []).filter((i) => selected.has(i.path));
  const selectedBytes = selectedItems.reduce((a, i) => a + i.size_bytes, 0);

  const archive = async () => {
    if (!report) return;
    setWorking(true);
    try {
      const outcome = await invoke<MoveOutcome>("move_to_folder", {
        paths: [...selected],
        destination: report.suggested_archive,
      });
      if (outcome.errors.length) setError(outcome.errors.join("; "));
      setNotice(
        `Moved ${outcome.moved} file(s) — ${formatBytes(outcome.moved_bytes)} — to ${report.suggested_archive}`
      );
      await scan();
    } catch (e) {
      setError(String(e));
    } finally {
      setWorking(false);
    }
  };

  const trash = async () => {
    setWorking(true);
    setConfirmTrash(false);
    try {
      const outcome = await invoke<CleanOutcome>("clean_paths", { paths: [...selected] });
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
      {confirmTrash && (
        <ConfirmClean
          request={{
            kind: "userfile",
            title: `Move ${selectedItems.length} file${selectedItems.length === 1 ? "" : "s"} to the Trash?`,
            items: selectedItems.map((i) => i.name),
            bytes: selectedBytes,
            risky: selectedItems.some((i) => !i.is_screenshot),
          }}
          busy={working}
          onConfirm={trash}
          onCancel={() => setConfirmTrash(false)}
        />
      )}

      <div className="view-head">
        <div>
          <h2 className="view-title">Desktop</h2>
          <p className="view-sub">Screenshots and clutter piling up on your desktop</p>
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
      {notice && (
        <div className="banner-info">
          <span>{notice}</span>{" "}
          <button className="btn-ghost" onClick={() => setNotice(null)}>Dismiss</button>
        </div>
      )}

      {scanning && !report && (
        <div className="empty-state"><div className="spinner" /><p>Reading your desktop…</p></div>
      )}

      {report && (
        <>
          <section className="metrics">
            <div className="metric accent">
              <span className="metric-label">Screenshots</span>
              <span className="metric-value">{formatBytes(report.screenshot_bytes)}</span>
            </div>
            <div className="metric">
              <span className="metric-label">Screenshot files</span>
              <span className="metric-value">{report.screenshot_count}</span>
            </div>
            <div className="metric">
              <span className="metric-label">Everything on desktop</span>
              <span className="metric-value">{formatBytes(report.total_bytes)}</span>
            </div>
            <div className="metric">
              <span className="metric-label">Items</span>
              <span className="metric-value">{report.file_count}</span>
            </div>
          </section>

          <div className="banner-info">
            Screenshots are your own files, so archiving is the default — they move to
            <code className="inline-path"> {report.suggested_archive}</code> where you can still find
            them. Trashing is available when you are sure.
          </div>

          <div className="filter-row">
            <div className="filter-tabs">
              <button
                className={`filter-tab ${type === "screenshots" ? "is-active" : ""}`}
                onClick={() => setType("screenshots")}
              >
                Screenshots ({report.screenshot_count})
              </button>
              <button
                className={`filter-tab ${type === "other" ? "is-active" : ""}`}
                onClick={() => setType("other")}
              >
                Other files ({otherCount})
              </button>
              <button
                className={`filter-tab ${type === "all" ? "is-active" : ""}`}
                onClick={() => setType("all")}
              >
                All ({report.file_count})
              </button>
            </div>
            <div className="filter-tabs">
              {(["all", "30", "90"] as AgeFilter[]).map((a) => (
                <button
                  key={a}
                  className={`filter-tab ${age === a ? "is-active" : ""}`}
                  onClick={() => setAge(a)}
                >
                  {a === "all" ? "Any age" : `Older than ${a}d`}
                </button>
              ))}
            </div>
            <div className="confirm">
              <button
                className="btn-ghost"
                onClick={() => setSelected(new Set(shots.map((i) => i.path)))}
              >
                Select all shown
              </button>
              {selected.size > 0 && (
                <>
                  <button className="btn" onClick={archive} disabled={working}>
                    {working ? "Working…" : `Archive ${selected.size} (${formatBytes(selectedBytes)})`}
                  </button>
                  <button className="btn-clean" onClick={() => setConfirmTrash(true)}>
                    Trash
                  </button>
                </>
              )}
            </div>
          </div>

          <div className="findings">
            {shots.length === 0 && (
              <div className="empty-state"><p>No screenshots in this view.</p></div>
            )}
            {shots.map((i) => (
              <label key={i.path} className="model-row">
                <input
                  type="checkbox"
                  checked={selected.has(i.path)}
                  onChange={() => toggle(i.path)}
                />
                <div className="model-main">
                  <span className="finding-name">{i.name}</span>
                  <span className="model-meta">
                    {i.age_days != null
                      ? i.age_days < 1
                        ? "Today"
                        : `${i.age_days} days old`
                      : "Unknown date"}
                  </span>
                </div>
                <div className="finding-size">
                  <span className="size">{formatBytes(i.size_bytes)}</span>
                </div>
                <button
                  className="btn-ghost sm"
                  onClick={(e) => {
                    e.preventDefault();
                    invoke("reveal_path", { path: i.path }).catch(() => {});
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
