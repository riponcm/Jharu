import { useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { formatBytes, isWindows } from "../format";
import type { Reason, Safety } from "../types";
import "../styles/system-space.css";

interface Fact {
  label: string;
  text: string;
}

interface Inspect {
  label: string;
  command: string;
  admin: boolean;
  slow: boolean;
  done: boolean;
}

interface SpaceAction {
  id: string;
  label: string;
  commands: string[];
  cost: string;
  safety: Safety;
  reasons: Reason[];
  admin: boolean;
  permanent: boolean;
  undo: string | null;
  slow: boolean;
  cancellable: boolean;
  est_bytes: number | null;
  est_note: string | null;
  enabled: boolean;
  disabled_reason: string | null;
}

interface SpaceItem {
  id: string;
  title: string;
  summary: string;
  size_bytes: number | null;
  size_note: string | null;
  facts: Fact[];
  notice: string | null;
  how_to: string | null;
  info_only: boolean;
  inspect: Inspect | null;
  actions: SpaceAction[];
}

interface SpaceReport {
  platform: "windows" | "macos" | "other";
  is_admin: boolean;
  items: SpaceItem[];
}

interface InspectOutcome {
  admin_declined: boolean;
  error: string | null;
  output: string;
}

interface RunOutcome {
  action_id: string;
  ok: boolean;
  skipped: string | null;
  freed_bytes: number | null;
  message: string;
  output: string;
  admin_declined: boolean;
  cancelled: boolean;
  errors: string[];
}

interface Busy {
  token: string;
  /// What is running, for the progress line.
  label: string;
  cancellable: boolean;
  stage: string;
}

const SAFETY_LABEL: Record<Safety, string> = { safe: "Safe", caution: "Caution", risky: "Risky" };
const ADMIN_WORD = isWindows ? "Windows administrator" : "macOS administrator";

/// Explanations quote commands in `backticks`; show those as code.
function withCode(text: string) {
  return text.split("`").map((part, n) => (n % 2 ? <code key={n}>{part}</code> : part));
}

function Reasons({ reasons }: { reasons: Reason[] }) {
  return (
    <ul className="safety-reasons">
      {reasons.map((r) => (
        <li key={r.text} className={`reason-${r.level}`}>
          <span className="reason-mark" aria-hidden>
            {r.level === "ok" ? "✓" : r.level === "warn" ? "!" : "✕"}
          </span>
          {r.text}
        </li>
      ))}
    </ul>
  );
}

export default function SystemSpace({ onRecovered }: { onRecovered: (bytes: number) => void }) {
  const [report, setReport] = useState<SpaceReport | null>(null);
  const [scanning, setScanning] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState<Busy | null>(null);
  const [confirming, setConfirming] = useState<{ item: SpaceItem; action: SpaceAction } | null>(null);
  const [understood, setUnderstood] = useState(false);
  const [notice, setNotice] = useState<{ tone: "info" | "error"; lines: string[]; output?: string } | null>(null);
  const activeToken = useRef<string | null>(null);

  const scan = async () => {
    setScanning(true);
    try {
      setReport(await invoke<SpaceReport>("system_space_scan"));
    } catch (e) {
      setError(String(e));
    } finally {
      setScanning(false);
    }
  };

  useEffect(() => {
    scan();
    let unlisten: UnlistenFn | undefined;
    listen<{ token: string; stage: string }>("system-space-progress", (event) => {
      if (event.payload.token !== activeToken.current) return;
      setBusy((b) => (b ? { ...b, stage: event.payload.stage } : b));
    }).then((u) => (unlisten = u));
    return () => unlisten?.();
  }, []);

  const begin = (label: string, cancellable: boolean) => {
    const token = `ss:${Math.random()}`;
    activeToken.current = token;
    setBusy({ token, label, cancellable, stage: "Starting…" });
    setNotice(null);
    setError(null);
    return token;
  };

  const finish = () => {
    activeToken.current = null;
    setBusy(null);
  };

  const inspect = async (item: SpaceItem) => {
    if (!item.inspect) return;
    const token = begin(item.inspect.label, false);
    try {
      const r = await invoke<InspectOutcome>("system_space_inspect", { itemId: item.id, token });
      if (r.admin_declined) {
        setNotice({
          tone: "info",
          lines: ["You declined the permission prompt, so Jharu did not look. Nothing was changed."],
        });
      } else if (r.error) {
        setNotice({ tone: "error", lines: [r.error], output: r.output });
      }
      await scan();
    } catch (e) {
      setError(String(e));
    } finally {
      finish();
    }
  };

  const run = async (item: SpaceItem, action: SpaceAction) => {
    setConfirming(null);
    setUnderstood(false);
    const token = begin(action.label, action.cancellable);
    try {
      const r = await invoke<RunOutcome>("system_space_run", { actionId: action.id, token });
      const lines: string[] = [];
      if (r.admin_declined) lines.push(r.message);
      else if (r.skipped) lines.push(`Skipped: ${r.skipped}`);
      else if (r.cancelled) lines.push(r.message);
      else if (r.ok) lines.push(r.message || "Done.");
      lines.push(...r.errors);
      setNotice({ tone: r.ok || r.admin_declined || r.skipped || r.cancelled ? "info" : "error", lines, output: r.output });
      // Docker frees space inside its own disk image, not on the drive.
      if (r.ok && r.freed_bytes && item.id !== "docker") onRecovered(r.freed_bytes);
      await scan();
    } catch (e) {
      setError(String(e));
    } finally {
      finish();
    }
  };

  const cancel = () => {
    if (busy) invoke("system_space_cancel", { token: busy.token }).catch(() => {});
  };

  const working = busy != null;

  return (
    <>
      {confirming && (
        <div className="modal-backdrop" role="dialog" aria-modal="true" onClick={() => setConfirming(null)}>
          <div className="modal ss-modal" onClick={(e) => e.stopPropagation()}>
            <h3 className="modal-title">{confirming.action.label}?</h3>
            <p className="modal-body">{confirming.action.cost}</p>
            {confirming.action.permanent ? (
              <p className="modal-warn">
                This cannot be undone. It does not go to the {isWindows ? "Recycle Bin" : "Trash"}, and
                there is no command that brings it back.
              </p>
            ) : (
              confirming.action.undo && (
                <p className="modal-body">
                  {withCode(confirming.action.undo)}
                </p>
              )
            )}
            {confirming.action.admin && (
              <p className="modal-body">
                {isWindows ? "Windows" : "macOS"} will ask for {ADMIN_WORD} permission. Jharu itself keeps
                running without it, and only this one command is run with it.
              </p>
            )}
            {confirming.action.slow && (
              <p className="modal-body">
                This takes a while. Keep the computer on until it finishes; you can use Jharu meanwhile.
              </p>
            )}
            <div className="modal-items">
              {confirming.action.commands.map((c) => (
                <code key={c} className="modal-item">{c}</code>
              ))}
            </div>
            {confirming.action.est_bytes != null && (
              <div className="modal-total">
                <span>{confirming.action.est_note ? "Expected (estimate)" : "Space freed"}</span>
                <strong>{formatBytes(confirming.action.est_bytes)}</strong>
              </div>
            )}
            {confirming.action.safety === "risky" && (
              <label className="ss-understand">
                <input type="checkbox" checked={understood} onChange={(e) => setUnderstood(e.target.checked)} />
                I understand what I lose and that Jharu rates this as risky.
              </label>
            )}
            <div className="modal-actions">
              <button className="btn" onClick={() => setConfirming(null)}>
                No, keep it
              </button>
              <button
                className="btn-danger"
                disabled={confirming.action.safety === "risky" && !understood}
                onClick={() => run(confirming.item, confirming.action)}
              >
                Yes, run it
              </button>
            </div>
          </div>
        </div>
      )}

      <div className="view-head">
        <div>
          <h2 className="view-title">System space</h2>
          <p className="view-sub">
            Large things the system keeps that are not ordinary files:{" "}
            {isWindows ? "hibernation, restore points, snapshots, Docker" : "local snapshots, the sleep image, Docker"}
          </p>
        </div>
        <button className="btn" onClick={scan} disabled={scanning || working}>
          {scanning ? "Scanning…" : "Rescan"}
        </button>
      </div>

      {error && (
        <div className="banner-error">
          <span>{error}</span>
          <button className="btn-ghost" onClick={() => setError(null)}>Dismiss</button>
        </div>
      )}

      {busy && (
        <div className="banner-info">
          <div className="meter-head">
            <span>
              {busy.label} · {busy.stage}
            </span>
            {busy.cancellable && (
              <button className="btn-ghost sm" onClick={cancel}>
                Cancel
              </button>
            )}
          </div>
          <div className="meter-track">
            <div className="meter-fill ss-indeterminate" />
          </div>
          <p className="model-meta">
            {busy.cancellable
              ? "Cancel stops waiting for the command; Docker may finish what it had started."
              : "System commands are not interrupted half way. You can keep using Jharu meanwhile."}
          </p>
        </div>
      )}

      {notice && !busy && (
        <div className={notice.tone === "error" ? "banner-error ss-notice" : "banner-info ss-notice"}>
          <div>
            {notice.lines.map((l) => (
              <p key={l}>{l}</p>
            ))}
            {notice.output && (
              <details>
                <summary>Command output</summary>
                <pre className="ss-output">{notice.output}</pre>
              </details>
            )}
          </div>
          <button className="btn-ghost" onClick={() => setNotice(null)}>Dismiss</button>
        </div>
      )}

      {scanning && !report && (
        <div className="empty-state">
          <div className="spinner" />
          <p>Reading file sizes and asking the system what it keeps…</p>
        </div>
      )}

      {report && (
        <>
          <div className="banner-info">
            None of this can go to the {isWindows ? "Recycle Bin" : "Trash"}, so every action says
            what it costs, whether it can be undone, and shows the exact command Jharu will run. Looking
            changes nothing; Jharu checks again right before it acts and skips anything that changed.
            {report.is_admin && " Jharu is running as administrator."}
          </div>

          {report.items.length === 0 && (
            <div className="empty-state">
              <p>Nothing to show on this system.</p>
            </div>
          )}

          {report.items.map((item) => (
            <section key={item.id} className="ss-card">
              <div className="ss-head">
                <div className="ss-head-main">
                  <h3 className="ss-title">
                    {item.title}
                    {item.info_only && <span className="badge stale">Information only</span>}
                  </h3>
                  <p className="ss-summary">{item.summary}</p>
                </div>
                <div className="ss-size">
                  <span className="size">{item.size_bytes != null ? formatBytes(item.size_bytes) : "—"}</span>
                  {item.size_note && <span className="model-meta">{item.size_note}</span>}
                </div>
              </div>

              {item.notice && <div className="banner-info ss-inline">{item.notice}</div>}

              {item.facts.length > 0 && (
                <dl className="ss-facts">
                  {item.facts.map((f) => (
                    <div key={f.label + f.text} className="ss-fact">
                      <dt>{f.label}</dt>
                      <dd>{f.text}</dd>
                    </div>
                  ))}
                </dl>
              )}

              {item.how_to && <p className="ss-howto">{item.how_to}</p>}

              {item.inspect && (
                <div className="ss-inspect">
                  <div>
                    <code className="ss-command">{item.inspect.command}</code>
                    <p className="model-meta">
                      Read-only.{item.inspect.admin && ` Asks for ${ADMIN_WORD} permission.`}
                      {item.inspect.slow && " Takes a minute or two."}
                    </p>
                  </div>
                  <button className="btn" disabled={working} onClick={() => inspect(item)}>
                    {item.inspect.done ? "Check again" : item.inspect.label}
                  </button>
                </div>
              )}

              {item.actions.map((a) => (
                <div key={a.id} className={`ss-action ${a.enabled ? "" : "is-off"}`}>
                  <div className="ss-action-head">
                    <div className="finding-title">
                      <span className="finding-name">{a.label}</span>
                      <span className={`badge safety-${a.safety}`}>{SAFETY_LABEL[a.safety]}</span>
                      {a.admin && <span className="badge stale">Needs administrator</span>}
                      <span className="badge stale">{a.permanent ? "Cannot be undone" : "Reversible"}</span>
                    </div>
                    <div className="ss-action-run">
                      {a.est_bytes != null && (
                        <span className="ss-est">
                          {a.est_note ? "≈ " : ""}
                          {formatBytes(a.est_bytes)}
                        </span>
                      )}
                      <button
                        className="btn-clean"
                        disabled={working || !a.enabled}
                        title={a.disabled_reason ?? undefined}
                        onClick={() => {
                          setUnderstood(false);
                          setConfirming({ item, action: a });
                        }}
                      >
                        Run…
                      </button>
                    </div>
                  </div>
                  <p className="ss-cost">
                    <strong>Costs:</strong> {a.cost}
                  </p>
                  {!a.enabled && a.disabled_reason && <p className="model-meta">{a.disabled_reason}</p>}
                  <Reasons reasons={a.reasons} />
                  <div className="ss-commands">
                    {a.commands.map((c) => (
                      <code key={c} className="ss-command">{c}</code>
                    ))}
                  </div>
                  {a.undo && <p className="model-meta">Undo: {withCode(a.undo)}</p>}
                  {a.est_note && <p className="model-meta">{a.est_note}</p>}
                </div>
              ))}
            </section>
          ))}

          <p className="footnote">
            {isWindows
              ? "Page file and sleep image are listed for information only: Jharu never changes them. Windows reports its tools' results in the system language; where Jharu cannot read a figure it says so and shows the tool's own output."
              : "The sleep image is listed for information only: Jharu never changes it. Where Jharu cannot read a figure from a system tool it says so and shows the tool's own output."}
          </p>
        </>
      )}
    </>
  );
}
