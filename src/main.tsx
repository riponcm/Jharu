import React from "react";
import ReactDOM from "react-dom/client";
import App from "./App";
import Gadget from "./gadget/Gadget";
import { isMac } from "./format";

// The floating gadget is a second window on the same page, opened with
// ?view=gadget (see gadget.rs).
const isGadget = new URLSearchParams(window.location.search).get("view") === "gadget";
// Set before the first paint, so the transparent window never flashes the
// app's page background.
if (isGadget) document.documentElement.classList.add("gadget-root");
// macOS keeps its traffic lights and cannot make the gadget transparent, so a
// few rules differ there (see titlebar.css and gadget.css).
if (isMac) document.documentElement.classList.add("platform-mac");

ReactDOM.createRoot(document.getElementById("root") as HTMLElement).render(
  <React.StrictMode>{isGadget ? <Gadget /> : <App />}</React.StrictMode>,
);
