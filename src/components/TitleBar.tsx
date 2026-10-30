import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { isWindows } from "../format";
import "../styles/titlebar.css";

interface GadgetSettings {
  enabled: boolean;
  mode: "bar" | "card";
}

const PATHS = {
  gadget: "M3.5 12.5a6 6 0 1 1 9 0M8 8.5l2.5-2.5",
  settings:
    "M8 10.2a2.2 2.2 0 1 0 0-4.4 2.2 2.2 0 0 0 0 4.4zM13 8c0-.4 0-.7-.1-1l1.4-1.1-1.3-2.3-1.7.6c-.5-.4-1-.7-1.6-.9L9.4 1.6H6.6l-.3 1.7c-.6.2-1.1.5-1.6.9L3 3.6 1.7 5.9 3.1 7c0 .3-.1.6-.1 1s0 .7.1 1l-1.4 1.1L3 12.4l1.7-.6c.5.4 1 .7 1.6.9l.3 1.7h2.8l.3-1.7c.6-.2 1.1-.5 1.6-.9l1.7.6 1.3-2.3L12.9 9c.1-.3.1-.6.1-1z",
  minimize: "M3 8h10",
  maximize: "M3.5 3.5h9v9h-9z",
  restore: "M5.5 5.5h7v7h-7zM3.5 10.5v-7h7",
  close: "M3.5 3.5l9 9M12.5 3.5l-9 9",
};

function Icon({ d }: { d: string }) {
  return (
    <svg width="16" height="16" viewBox="0 0 16 16" aria-hidden>
      <path d={d} />
    </svg>
  );
}

/// The window's own title bar on Windows (the system one is turned off in
/// lib.rs). It names the page and holds two shortcuts; navigation stays in
/// the sidebar. Empty space drags the window, and a double click maximizes.
export default function TitleBar({ title, onSettings }: { title: string; onSettings: () => void }) {
  const [maximized, setMaximized] = useState(false);
  const [gadget, setGadget] = useState<GadgetSettings | null>(null);

  useEffect(() => {
    const win = getCurrentWindow();
    const read = () => win.isMaximized().then(setMaximized).catch(() => {});
    read();
    const off = win.onResized(read);
    return () => {
      off.then((f) => f()).catch(() => {});
    };
  }, []);

  // The gadget can turn itself off from its own close button.
  useEffect(() => {
    const read = () => invoke<GadgetSettings>("gadget_get").then(setGadget).catch(() => {});
    read();
    window.addEventListener("focus", read);
    return () => window.removeEventListener("focus", read);
  }, []);

  const toggleGadget = () => {
    if (!gadget) return;
    invoke<GadgetSettings>("gadget_set", { enabled: !gadget.enabled, mode: gadget.mode })
      .then(setGadget)
      .catch(() => {});
  };

  const win = getCurrentWindow();
  return (
    <header className="titlebar" data-tauri-drag-region>
      <span className="tb-title" data-tauri-drag-region>
        {title}
      </span>
      <span className="tb-tools">
        <button
          className={`tb-icon ${gadget?.enabled ? "is-on" : ""}`}
          onClick={toggleGadget}
          title={gadget?.enabled ? "Hide the floating gadget" : "Show the floating gadget"}
          aria-pressed={gadget?.enabled ?? false}
        >
          <Icon d={PATHS.gadget} />
        </button>
        <button className="tb-icon" onClick={onSettings} title="Settings and updates">
          <Icon d={PATHS.settings} />
        </button>
      </span>
      {isWindows && (
        <span className="tb-controls">
          <button className="tb-ctl" onClick={() => win.minimize()} aria-label="Minimize" title="Minimize">
            <Icon d={PATHS.minimize} />
          </button>
          <button
            className="tb-ctl"
            onClick={() => win.toggleMaximize()}
            aria-label={maximized ? "Restore" : "Maximize"}
            title={maximized ? "Restore" : "Maximize"}
          >
            <Icon d={maximized ? PATHS.restore : PATHS.maximize} />
          </button>
          <button className="tb-ctl is-close" onClick={() => win.close()} aria-label="Close" title="Close">
            <Icon d={PATHS.close} />
          </button>
        </span>
      )}
    </header>
  );
}
