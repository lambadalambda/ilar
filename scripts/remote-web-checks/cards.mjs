// Checks the grant and question cards with a fake remote (cards.html).
import { spawn } from "node:child_process";

const chrome = spawn(
  process.env.CHROME,
  ["--headless=new", "--remote-debugging-port=9335", "--no-sandbox", "--user-data-dir=/tmp/cards-chrome", "about:blank"],
  { stdio: "ignore" },
);
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
let failed = false;
const check = (name, ok, detail) => {
  console.log(`${ok ? "ok  " : "FAIL"} ${name}${ok ? "" : ": " + JSON.stringify(detail)}`);
  if (!ok) failed = true;
};

try {
  let targets;
  for (let i = 0; i < 50 && !targets; i++) {
    await sleep(200);
    targets = await fetch("http://127.0.0.1:9335/json").then((r) => r.json()).catch(() => null);
  }
  const ws = new WebSocket(targets.find((t) => t.type === "page").webSocketDebuggerUrl);
  await new Promise((r) => (ws.onopen = r));
  let id = 0;
  const pending = new Map();
  ws.onmessage = (m) => {
    const msg = JSON.parse(m.data);
    if (pending.has(msg.id)) pending.get(msg.id)(msg.result);
    if (msg.method === "Runtime.exceptionThrown") console.log("EXCEPTION", JSON.stringify(msg.params.exceptionDetails).slice(0, 300));
  };
  const call = (method, params = {}) =>
    new Promise((r) => { pending.set(++id, r); ws.send(JSON.stringify({ id, method, params })); });
  const evaluate = async (expression) =>
    (await call("Runtime.evaluate", { expression, awaitPromise: true, returnByValue: true }))?.result?.value;
  await call("Runtime.enable");
  await call("Page.navigate", { url: process.argv[2] });
  for (let t = 0; t < 40 && !(await evaluate("!!document.querySelector('.pill')")); t++) await sleep(250);

  await evaluate(`setPrompt({ kind: "grant", tool: "bash", secret: "GH_TOKEN", description: "GitHub token", detail: "gh pr list", agent: null })`);
  await sleep(200);
  check("grant card shows the command", await evaluate("document.querySelector('.ask').innerText.includes('gh pr list')"));
  await evaluate("[...document.querySelectorAll('.ask button')].find((b) => b.innerText.includes('allow once')).click()");
  await sleep(100);
  const grant = await evaluate("sent.at(-1)");
  check("allow once sends the secret", JSON.stringify(grant) === JSON.stringify({ type: "grant", secret: "GH_TOKEN", answer: "once" }), grant);
  await evaluate("[...document.querySelectorAll('.ask button')].find((b) => b.innerText === 'deny').click()");
  await sleep(100);
  const deny = await evaluate("sent.at(-1)");
  check("deny sends null", deny && deny.answer === null, deny);

  await evaluate(`setPrompt({ kind: "question", request: { questions: [
    { id: "q1", prompt: "Which?", required: true, type: "single_choice", allow_other: false, options: [{ id: "a", label: "A" }, { id: "b", label: "B" }] },
    { id: "q2", prompt: "Note", required: false, type: "free_text" } ] } })`);
  await sleep(200);
  check("question card shows the options", await evaluate("document.querySelectorAll('.ask-option').length === 2"));
  await evaluate("document.querySelectorAll('.ask-option input')[1].click()");
  await evaluate(`(() => { const t = document.querySelector('.ask textarea'); t.value = 'fine'; t.dispatchEvent(new Event('input', { bubbles: true })); })()`);
  await sleep(100);
  await evaluate("[...document.querySelectorAll('.ask button')].find((b) => b.innerText === 'answer').click()");
  await sleep(100);
  const answered = await evaluate("sent.at(-1)");
  const want = { type: "question", response: { status: "answered", answers: [
    { type: "single_choice", question_id: "q1", option_id: "b" },
    { type: "free_text", question_id: "q2", text: "fine" } ] } };
  check("answer sends the response shape", JSON.stringify(answered) === JSON.stringify(want), answered);

  await evaluate("setPrompt(null)");
  await sleep(100);
  check("the card goes when the prompt does", await evaluate("!document.querySelector('.ask')"));
  ws.close();
} finally {
  chrome.kill();
  process.exitCode = failed ? 1 : 0;
}
