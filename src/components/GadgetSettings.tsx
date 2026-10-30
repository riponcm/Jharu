import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import "../styles/gadget-settings.css";

type Mode = "bar" | "card";

interface Settings {
  enabled: boolean;
  mode: Mode;
}

/// The option that turns the floating gadget on or off. Off by default.
export default function GadgetSettings() {
  const [s, setS] = useState<Settings | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    invoke<Settings>("gadget_get").then(setS).catch((e) => setError(String(e)));
  }, []);

  // The gadget can turn itself off from its own ✕, so re-read on focus.
  useEffect(() => {
    const again = () => invoke<Settings>("gadget_get").then(setS).catch(() => {});
    window.addEventListener("focus", again);
    return () => window.removeEventListener("focus", again);
  }, []);

  const apply = (next: Settings) => {
    setS(next);
    setError(null);
    invoke<Settings>("gadget_set", { enabled: next.enabled, mode: next.mode })
      .then(setS)
      .catch((e) => setError(String(e)));
  };

  if (!s) return error ? <p className="gs-error">{error}</p> : null;

  return (
    <div className="gs-card">
      <label className="gs-main">
        <input type="checkbox" checked={s.enabled} onChange={(e) => apply({ ...s, enabled: e.target.checked })} />
        <span>
          <strong>Floating gadget</strong>
          <span className="gs-sub">
            A small window that stays on top of others: CPU, memory, free space on the system drive and
            when it runs out, the busiest programs, and one-click cleaning of safe caches. It reads a
            few numbers every two seconds and never scans the disk in the background.
          </span>
        </span>
      </label>
      <div className="gs-modes" role="radiogroup" aria-label="Gadget size">
        {(["bar", "card"] as Mode[]).map((m) => (
          <button
            key={m}
            role="radio"
            aria-checked={s.mode === m}
            className={`filter-tab ${s.mode === m ? "is-active" : ""}`}
            disabled={!s.enabled}
            onClick={() => apply({ ...s, mode: m })}
          >
            {m === "bar" ? "Slim bar" : "Card"}
          </button>
        ))}
        <span className="gs-sub">Drag it anywhere; it opens where you left it. ✕ on the gadget turns it off.</span>
      </div>
      {error && <p className="gs-error">{error}</p>}
    </div>
  );
}
