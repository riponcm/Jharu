import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { getVersion } from "@tauri-apps/api/app";
import { check, type Update } from "@tauri-apps/plugin-updater";
import { relaunch } from "@tauri-apps/plugin-process";
import logo from "../assets/jharu-mark.svg";
import GadgetSettings from "../components/GadgetSettings";

const REPO_URL = "https://github.com/riponcm/Jharu";
const MATILY_URL = "https://matily.org";

type UpdateState =
  | { phase: "idle" }
  | { phase: "checking" }
  | { phase: "uptodate" }
  | { phase: "available"; update: Update }
  | { phase: "downloading"; percent: number; update: Update }
  | { phase: "ready" }
  | { phase: "error"; message: string };

export default function About() {
  const [version, setVersion] = useState("");
  const [state, setState] = useState<UpdateState>({ phase: "idle" });

  useEffect(() => {
    getVersion().then(setVersion).catch(() => {});
  }, []);

  const open = (url: string) => invoke("open_url", { url }).catch(() => {});

  const checkForUpdates = async () => {
    setState({ phase: "checking" });
    try {
      const update = await check();
      setState(update ? { phase: "available", update } : { phase: "uptodate" });
    } catch (e) {
      setState({
        phase: "error",
        message: "Could not reach the update server. Check your connection and try again.",
      });
    }
  };

  const install = async (update: Update) => {
    setState({ phase: "downloading", percent: 0, update });
    try {
      let total = 0;
      let received = 0;
      await update.downloadAndInstall((event) => {
        if (event.event === "Started") {
          total = event.data.contentLength ?? 0;
        } else if (event.event === "Progress") {
          received += event.data.chunkLength;
          const percent = total ? Math.round((received / total) * 100) : 0;
          setState({ phase: "downloading", percent, update });
        }
      });
      setState({ phase: "ready" });
    } catch (e) {
      setState({ phase: "error", message: String(e) });
    }
  };

  return (
    <>
      <div className="view-head">
        <div>
          <h2 className="view-title">About</h2>
          <p className="view-sub">Settings, version, updates, and project links</p>
        </div>
      </div>

      <GadgetSettings />

      <div className="about-card">
        <img src={logo} alt="Jharu logo" className="about-logo" />
        <h2 className="about-name">Jharu</h2>
        <p className="about-version">Version {version || "0.1.0"}</p>
        <p className="about-tagline">
          Storage insight and cleanup for developers. Jharu is the Bengali word for broom —
          it finds the data your tools leave behind and sweeps it away safely.
        </p>

        <div className="about-update">
          {state.phase === "idle" && (
            <button className="btn" onClick={checkForUpdates}>Check for updates</button>
          )}
          {state.phase === "checking" && (
            <span className="about-status"><span className="spinner sm inline" /> Checking for updates…</span>
          )}
          {state.phase === "uptodate" && (
            <span className="about-status ok">You are on the latest version.</span>
          )}
          {state.phase === "available" && (
            <span className="confirm">
              <span className="about-status">Version {state.update.version} is available.</span>
              <button className="btn" onClick={() => install(state.update)}>
                Download and install
              </button>
            </span>
          )}
          {state.phase === "downloading" && (
            <div className="about-progress">
              <div className="child-bar wide">
                <div className="child-fill" style={{ width: `${state.percent}%` }} />
              </div>
              <span className="about-status">Downloading… {state.percent}%</span>
            </div>
          )}
          {state.phase === "ready" && (
            <span className="confirm">
              <span className="about-status ok">Update installed.</span>
              <button className="btn" onClick={() => relaunch()}>Restart Jharu</button>
            </span>
          )}
          {state.phase === "error" && (
            <span className="confirm">
              <span className="about-status err">{state.message}</span>
              <button className="btn-ghost" onClick={checkForUpdates}>Try again</button>
            </span>
          )}
        </div>

        <div className="about-links">
          <button className="btn" onClick={() => open(REPO_URL)}>
            Star Jharu on GitHub
          </button>
          <button className="btn-ghost" onClick={() => open(`${REPO_URL}/issues`)}>
            Report an issue
          </button>
        </div>

        <div className="about-foot">
          <p>
            A product of{" "}
            <button className="linklike" onClick={() => open(MATILY_URL)}>Matily.org</button>
          </p>
          <p className="about-fineprint">
            Free and open source under the Apache License 2.0. If Jharu saved you space,
            a star on GitHub helps other developers find it.
          </p>
        </div>
      </div>
    </>
  );
}
