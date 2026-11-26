// Opens the spike page with a ticket in headless chromium (CDP) and
// prints the page log once it says DONE or ERROR (60 s limit).
import { spawn } from "node:child_process";

const url = process.argv[2];
const bin = process.env.CHROME;
const chrome = spawn(bin, ["--headless=new", "--remote-debugging-port=9333", "--no-sandbox", "--user-data-dir=/tmp/spike-chrome", "about:blank"], { stdio: "ignore" });
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

try {
  let targets;
  for (let i = 0; i < 50 && !targets; i++) {
    await sleep(200);
    targets = await fetch("http://127.0.0.1:9333/json").then((r) => r.json()).catch(() => null);
  }
  const page = targets.find((t) => t.type === "page");
  const ws = new WebSocket(page.webSocketDebuggerUrl);
  await new Promise((r) => (ws.onopen = r));
  let id = 0;
  const pending = new Map();
  ws.onmessage = (m) => {
    const msg = JSON.parse(m.data);
    if (pending.has(msg.id)) pending.get(msg.id)(msg.result);
  };
  const call = (method, params = {}) =>
    new Promise((r) => { pending.set(++id, r); ws.send(JSON.stringify({ id, method, params })); });
  await call("Page.navigate", { url });
  let text = "";
  for (let i = 0; i < 120 && !/DONE|ERROR/.test(text); i++) {
    await sleep(500);
    const res = await call("Runtime.evaluate", { expression: "document.getElementById('log')?.textContent ?? ''" });
    text = res?.result?.value ?? "";
  }
  console.log(text || "(no log)");
  ws.close();
} finally {
  chrome.kill();
}
