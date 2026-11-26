// Drives the remote PWA in headless Chrome over CDP: open the link, wait
// for the transcript, type a message and send it, then report what the
// page shows. Usage: node pwa.mjs <url>   (CHROME = path to chrome)
import { spawn } from "node:child_process";

const url = process.argv[2];
const chrome = spawn(
  process.env.CHROME,
  ["--headless=new", "--remote-debugging-port=9334", "--no-sandbox", "--user-data-dir=/tmp/pwa-chrome", "--window-size=400,860", "about:blank"],
  { stdio: "ignore" },
);
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

try {
  let targets;
  for (let i = 0; i < 50 && !targets; i++) {
    await sleep(200);
    targets = await fetch("http://127.0.0.1:9334/json").then((r) => r.json()).catch(() => null);
  }
  const ws = new WebSocket(targets.find((t) => t.type === "page").webSocketDebuggerUrl);
  await new Promise((r) => (ws.onopen = r));
  let id = 0;
  const pending = new Map();
  const logs = [];
  ws.onmessage = (m) => {
    const msg = JSON.parse(m.data);
    if (pending.has(msg.id)) pending.get(msg.id)(msg.result);
    if (msg.method === "Runtime.consoleAPICalled") logs.push(msg.params.args.map((a) => a.value ?? a.description).join(" "));
    if (msg.method === "Runtime.exceptionThrown") logs.push("EXCEPTION " + JSON.stringify(msg.params.exceptionDetails).slice(0, 400));
  };
  const call = (method, params = {}) =>
    new Promise((r) => { pending.set(++id, r); ws.send(JSON.stringify({ id, method, params })); });
  const evaluate = async (expression) =>
    (await call("Runtime.evaluate", { expression, awaitPromise: true, returnByValue: true }))?.result?.value;
  const until = async (expression, ms = 30000) => {
    for (let t = 0; t < ms; t += 250) {
      if (await evaluate(expression)) return true;
      await sleep(250);
    }
    return false;
  };

  await call("Runtime.enable");
  await call("Emulation.setDeviceMetricsOverride", { width: 400, height: 860, deviceScaleFactor: 2, mobile: true });
  const t0 = Date.now();
  await call("Page.navigate", { url });
  const shown = await until("!!document.querySelector('.pill')");
  console.log(`transcript shown: ${shown} after ${Date.now() - t0} ms`);
  console.log("address bar:", await evaluate("location.href"));
  console.log("stored key bytes:", await evaluate("(JSON.parse(localStorage.getItem('ilar.remote'))||{key:[]}).key.length"));
  console.log("pill:", await evaluate("document.querySelector('.pill')?.innerText"));
  console.log("rows:", await evaluate("document.querySelectorAll('.stream > *').length"));

  await evaluate(`(() => {
    const box = document.querySelector('textarea.prompt');
    box.value = 'hello from the phone';
    box.dispatchEvent(new Event('input', { bubbles: true }));
  })()`);
  await sleep(300);
  await evaluate("document.querySelector('button.send').click()");
  await until("document.body.innerText.includes('hello from the phone')", 15000);
  await sleep(4000);
  console.log("fate:", await evaluate("document.querySelector('.fate')?.innerText"));
  console.log("page has the message:", await evaluate("document.querySelector('.stream').innerText.includes('hello from the phone')"));
  console.log("pill after:", await evaluate("document.querySelector('.pill')?.innerText"));
  const shot = await call("Page.captureScreenshot", { format: "png" });
  (await import("node:fs")).writeFileSync("/tmp/pwa-shot.png", Buffer.from(shot.data, "base64"));
  console.log("console:", logs.slice(0, 15).join("\n  "));
  ws.close();
} finally {
  chrome.kill();
}
