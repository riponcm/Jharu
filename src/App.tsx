import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import DevClean from "./views/DevClean";
import DeepClean from "./views/DeepClean";
import Browsers from "./views/Browsers";
import Uninstaller from "./views/Uninstaller";
import DriveHealth from "./views/DriveHealth";
import Junk from "./views/Junk";
import Models from "./views/Models";
import Python from "./views/Python";
import Desktop from "./views/Desktop";
import About from "./views/About";
import { formatBytes } from "./format";
import type { AccessStatus } from "./types";
import logo from "./assets/jharu-mark.svg";
import "./App.css";

type View =
  | "dev"
  | "models"
  | "python"
  | "deep"
  | "junk"
  | "desktop"
  | "browsers"
  | "uninstall"
  | "health"
  | "about";

const NAV: { id: View; label: string; icon: string; sub: string }[] = [
  { id: "dev", label: "Dev clean", icon: "⌘", sub: "ML and dev caches" },
  { id: "models", label: "ML models", icon: "◈", sub: "Per-model, used or not" },
  { id: "python", label: "Python", icon: "◐", sub: "Envs and duplicates" },
  { id: "deep", label: "Deep clean", icon: "◉", sub: "Full-disk analysis" },
  { id: "junk", label: "Junk files", icon: "▤", sub: "Installers, backups, logs" },
  { id: "desktop", label: "Desktop", icon: "▢", sub: "Screenshots and clutter" },
  { id: "browsers", label: "Browsers", icon: "◍", sub: "Web caches" },
  { id: "uninstall", label: "Uninstaller", icon: "✕", sub: "Apps and leftovers" },
  { id: "health", label: "Drive health", icon: "♥", sub: "Capacity and SMART" },
  { id: "about", label: "About", icon: "◈", sub: "Version and updates" },
];

function App() {
  const [view, setView] = useState<View>("dev");
  const [recovered, setRecovered] = useState(0);
  const [access, setAccess] = useState<AccessStatus | null>(null);
  const [accessDismissed, setAccessDismissed] = useState(false);

  const addRecovered = (bytes: number) => setRecovered((r) => r + bytes);

  const checkAccess = () => {
    invoke<AccessStatus>("check_access").then(setAccess).catch(() => {});
  };

  useEffect(() => {
    checkAccess();
  }, []);

  return (
    <div className="shell">
      <aside className="sidebar">
        <div className="brand">
          <img src={logo} alt="" className="brand-logo" />
          <div>
            <h1>Jharu</h1>
            <p className="subtitle">Storage insight</p>
          </div>
        </div>
        <nav className="nav">
          {NAV.map((n) => (
            <button
              key={n.id}
              className={`nav-item ${view === n.id ? "is-active" : ""}`}
              onClick={() => setView(n.id)}
            >
              <span className="nav-icon">{n.icon}</span>
              <span className="nav-text">
                <span className="nav-label">{n.label}</span>
                <span className="nav-sub">{n.sub}</span>
              </span>
            </button>
          ))}
        </nav>
        <div className="sidebar-foot">
          {recovered > 0 && (
            <div className="recovered-tile">
              <span className="metric-label">Recovered this session</span>
              <span className="recovered-value">{formatBytes(recovered)}</span>
            </div>
          )}
          <p className="footnote left">Everything goes to the Trash — nothing is permanently deleted.</p>
          <p className="footnote left brandline">A Matily.org product</p>
        </div>
      </aside>

      <main className="content">
        {access?.needs_prompt && !accessDismissed && (
          <div className="banner-access">
            <div>
              <strong>Give Jharu full access for a complete scan</strong>
              <p className="view-sub">
                Without Full Disk Access, some folders show 0 B or are skipped. Enable Jharu in
                the list, then come back and rescan.
              </p>
            </div>
            <div className="confirm">
              <button
                className="btn"
                onClick={() => invoke("open_access_settings").catch(() => {})}
              >
                Open settings
              </button>
              <button className="btn-ghost" onClick={checkAccess}>Recheck</button>
              <button className="btn-ghost" onClick={() => setAccessDismissed(true)}>Later</button>
            </div>
          </div>
        )}

        {view === "dev" && <DevClean onRecovered={addRecovered} />}
        {view === "models" && <Models onRecovered={addRecovered} />}
        {view === "python" && <Python onRecovered={addRecovered} />}
        {view === "deep" && <DeepClean onRecovered={addRecovered} />}
        {view === "junk" && <Junk onRecovered={addRecovered} />}
        {view === "desktop" && <Desktop onRecovered={addRecovered} />}
        {view === "browsers" && <Browsers onRecovered={addRecovered} />}
        {view === "uninstall" && <Uninstaller onRecovered={addRecovered} />}
        {view === "health" && <DriveHealth />}
        {view === "about" && <About />}
      </main>
    </div>
  );
}

export default App;
