import { type ReactNode, useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import Overview from "./views/Overview";
import DevClean from "./views/DevClean";
import DeepClean from "./views/DeepClean";
import Browsers from "./views/Browsers";
import Uninstaller from "./views/Uninstaller";
import DriveHealth from "./views/DriveHealth";
import Junk from "./views/Junk";
import Models from "./views/Models";
import Python from "./views/Python";
import Desktop from "./views/Desktop";
import Worktrees from "./views/Worktrees";
import RecycleBin from "./views/RecycleBin";
import Projects from "./views/Projects";
import SystemSpace from "./views/SystemSpace";
import Duplicates from "./views/Duplicates";
import History from "./views/History";
import Sandboxes from "./views/Sandboxes";
import AiTools from "./views/AiTools";
import Speed from "./views/Speed";
import Startup from "./views/Startup";
import About from "./views/About";
import TitleBar from "./components/TitleBar";
import { formatBytes, isMac, isWindows } from "./format";
import type { AccessStatus } from "./types";
import logo from "./assets/jharu-mark.svg";
import "./App.css";

type View =
  | "overview"
  | "dev"
  | "models"
  | "python"
  | "projects"
  | "worktrees"
  | "ai"
  | "deep"
  | "dupes"
  | "sandboxes"
  | "system"
  | "junk"
  | "desktop"
  | "browsers"
  | "bin"
  | "uninstall"
  | "speed"
  | "startup"
  | "health"
  | "history"
  | "about";

type NavItem = { id: View; label: string; icon: string; sub: string };

// Grouped so nineteen tabs stay findable: what to clean, where the space
// went, what to manage, and how the drive is doing.
const NAV: { group: string; items: NavItem[] }[] = [
  {
    group: "",
    items: [{ id: "overview", label: "Overview", icon: "◎", sub: "Drives, savings, forecast" }],
  },
  {
    group: "Developer",
    items: [
      { id: "dev", label: "Dev clean", icon: "⌘", sub: "ML and dev caches" },
      { id: "models", label: "ML models", icon: "◈", sub: "Per-model, used or not" },
      { id: "python", label: "Python", icon: "◐", sub: "Envs and duplicates" },
      { id: "projects", label: "Build folders", icon: "▥", sub: "node_modules, target, venvs" },
      { id: "worktrees", label: "Git worktrees", icon: "⑂", sub: "Merged spare checkouts" },
      { id: "ai", label: "AI tools", icon: "✦", sub: "Agent caches and history" },
    ],
  },
  {
    group: "Disk",
    items: [
      { id: "deep", label: "Deep clean", icon: "◉", sub: "Full-disk analysis" },
      { id: "dupes", label: "Duplicates", icon: "⧉", sub: "Same file or model twice" },
      { id: "sandboxes", label: "App sandboxes", icon: "▣", sub: isWindows ? "Hidden Store-app data" : "Containers of removed apps" },
      { id: "system", label: "System space", icon: "▦", sub: isWindows ? "Hibernation, restore points, Docker" : "Snapshots, sleep image, Docker" },
      { id: "junk", label: "Junk files", icon: "▤", sub: "Installers, backups, logs" },
      { id: "desktop", label: "Desktop", icon: "▢", sub: "Screenshots and clutter" },
      { id: "browsers", label: "Browsers", icon: "◍", sub: "Web caches" },
    ],
  },
  {
    group: "Manage",
    items: [
      { id: "bin", label: isWindows ? "Recycle Bin" : "Trash", icon: "♻", sub: "Restore or empty" },
      { id: "uninstall", label: "Uninstaller", icon: "✕", sub: "Apps and leftovers" },
      { id: "speed", label: "Speed", icon: "↯", sub: "Live load, task killer" },
      { id: "startup", label: "Startup apps", icon: "⏻", sub: "What starts at sign-in" },
    ],
  },
  {
    group: "Insight",
    items: [
      { id: "health", label: "Drive health", icon: "♥", sub: "Capacity and SMART" },
      { id: "history", label: "Space history", icon: "◔", sub: "What grew, forecast, alerts" },
      { id: "about", label: "About", icon: "◈", sub: "Settings and updates" },
    ],
  },
];

// Views whose scans or removals can run for minutes. They stay mounted once
// opened, so their progress survives switching tabs.
const KEPT: View[] = ["worktrees", "projects", "system", "dupes", "sandboxes", "ai", "speed"];

function App() {
  const [view, setView] = useState<View>("overview");
  const [recovered, setRecovered] = useState(0);
  const [access, setAccess] = useState<AccessStatus | null>(null);
  const [accessDismissed, setAccessDismissed] = useState(false);
  const [opened, setOpened] = useState<Set<View>>(() => new Set());

  useEffect(() => {
    if (KEPT.includes(view)) setOpened((o) => (o.has(view) ? o : new Set(o).add(view)));
  }, [view]);

  // Renders a long-running view once it has been opened, hidden while
  // another tab is showing.
  const kept = (id: View, el: ReactNode) =>
    (opened.has(id) || view === id) && (
      <div style={{ display: view === id ? undefined : "none" }}>{el}</div>
    );

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
        {/* macOS draws its traffic lights over this band (tauri.macos.conf.json),
            level with the title bar beside it; it also drags the window. */}
        {isMac && <div className="mac-lights" data-tauri-drag-region />}
        <div className="brand" data-tauri-drag-region>
          <img src={logo} alt="" className="brand-logo" />
          <div>
            <h1>Jharu</h1>
            <p className="subtitle">Storage insight</p>
          </div>
        </div>
        <nav className="nav">
          {NAV.map((g) => (
            <div key={g.group} className="nav-group">
              {g.group && <span className="nav-group-title">{g.group}</span>}
              {g.items.map((n) => (
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
            </div>
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

      <div className="main-col">
        <TitleBar
          title={NAV.flatMap((g) => g.items).find((n) => n.id === view)?.label ?? ""}
          onSettings={() => setView("about")}
        />
        <div className="content-scroll">
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

        {view === "overview" && <Overview onNavigate={(v) => setView(v as View)} />}
        {view === "dev" && <DevClean onRecovered={addRecovered} />}
        {view === "models" && <Models onRecovered={addRecovered} />}
        {view === "python" && <Python onRecovered={addRecovered} />}
        {kept("projects", <Projects onRecovered={addRecovered} />)}
        {kept("worktrees", <Worktrees onRecovered={addRecovered} />)}
        {kept("ai", <AiTools onRecovered={addRecovered} onOpen={(v) => setView(v as View)} />)}
        {view === "deep" && <DeepClean onRecovered={addRecovered} />}
        {kept("dupes", <Duplicates onRecovered={addRecovered} />)}
        {kept("sandboxes", <Sandboxes onRecovered={addRecovered} />)}
        {kept("system", <SystemSpace onRecovered={addRecovered} />)}
        {view === "junk" && <Junk onRecovered={addRecovered} />}
        {view === "desktop" && <Desktop onRecovered={addRecovered} />}
        {view === "browsers" && <Browsers onRecovered={addRecovered} />}
        {view === "bin" && <RecycleBin onRecovered={addRecovered} />}
        {view === "uninstall" && <Uninstaller onRecovered={addRecovered} />}
        {kept("speed", <Speed />)}
        {view === "startup" && <Startup />}
        {view === "health" && <DriveHealth />}
        {view === "history" && <History onRecovered={addRecovered} />}
        {view === "about" && <About />}
      </main>
        </div>
      </div>
    </div>
  );
}

export default App;
