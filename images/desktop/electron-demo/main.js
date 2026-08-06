// SPDX-License-Identifier: AGPL-3.0-or-later

const { app, BrowserWindow } = require("electron");
const path = require("node:path");

app.commandLine.appendSwitch("force-renderer-accessibility");

app.whenReady().then(() => {
  const window = new BrowserWindow({
    width: 760,
    height: 520,
    title: "Wild Buzzard Electron",
    webPreferences: {
      contextIsolation: true,
      sandbox: true,
    },
  });
  window.loadFile(path.join(__dirname, "index.html"));
});

app.on("window-all-closed", () => app.quit());
