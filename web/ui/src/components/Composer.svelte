<!-- The input and the line above it: what the session is doing and has
     spent. Enter sends, Shift-Enter breaks the line; while a turn runs an
     empty box offers stop, and a message steers the turn. What a send did
     is the server's word, shown briefly. A 409 means another process
     holds the session: said, but the box stays usable. -->
<script>
  import GrantCard from "./GrantCard.svelte";
  import QuestionCard from "./QuestionCard.svelte";
  import { apiPath, post, REMOTE, SHARE } from "../lib/source.js";
  import { compact, cost, FATE_WORDS, message } from "../lib/format.js";
  let { id, session, view, version } = $props();

  let text = $state("");
  let busy = $state(false);
  let aborting = $state(false);
  let fate = $state("");
  let refused = $state(false);

  // The paired session's open prompt and last notice (web/remote).
  let remote = $state.raw(REMOTE ? REMOTE.state() : {});
  $effect(() => (REMOTE ? REMOTE.subscribe(() => (remote = REMOTE.state())) : undefined));
  let remoteFailure = $state("");
  const answer = (command) => {
    remoteFailure = "";
    REMOTE.send(command).catch((error) => (remoteFailure = message(error)));
  };

  // Any change in what the session does may mean the holder let go. The
  // listing row is a new object every poll, so compare the two fields.
  const lease = $derived(String(session && session.driven) + "|" + (session && session.state));
  $effect(() => {
    lease;
    refused = false;
  });
  $effect(() => {
    if (!fate) return;
    const timer = setTimeout(() => (fate = ""), 4000);
    return () => clearTimeout(timer);
  });

  const head = $derived.by(() => {
    version;
    return { live: Boolean(view.live), usage: view.usage || {}, count: view.count, status: view.status, model: view.session && view.session.model };
  });
  const state = $derived((session && session.state) || (head.live ? "working" : "idle"));
  const driven = $derived(Boolean(session && session.driven));
  const model = $derived((session && session.model) || head.model || "");
  const saying = $derived(
    state === "working"
      ? (session && session.activity) || (model ? model + " is working…" : "working…")
      : state === "stalled"
        ? "quiet since the last write"
        : head.status && head.status !== "live"
          ? head.status
          : "idle",
  );

  async function send() {
    const body = text.trim();
    if (!body || busy) return;
    busy = true;
    try {
      const result = await post(apiPath(id, "/message"), { text: body });
      // Only the words sent: what was typed on top is still a draft.
      if (text.trim() === body) text = "";
      refused = false;
      fate = FATE_WORDS[result.fate] || result.fate || "sent";
    } catch (error) {
      if (error.status === 409) refused = true;
      fate = message(error);
    } finally {
      busy = false;
    }
  }

  async function abort() {
    aborting = true;
    try {
      const result = await post(apiPath(id, "/abort"), {});
      fate = FATE_WORDS[result.fate] || result.fate || "aborted";
    } catch (error) {
      fate = message(error);
    } finally {
      aborting = false;
    }
  }

  const onkeydown = (event) => {
    // A phone's return key is a newline; the button sends.
    if (event.key !== "Enter" || event.shiftKey || event.isComposing || matchMedia("(pointer: coarse)").matches) return;
    event.preventDefault();
    send();
  };
  const spent = $derived(typeof head.usage.cost_dollars === "number" ? cost(head.usage.cost_dollars) : head.usage.plan ? "plan" : "");
  const stoppable =$derived(driven && state === "working" && !text.trim());
</script>

{#if refused}<p class="watching">This session is open in another process — watching only. Send again to try once it lets go.</p>{/if}
{#if REMOTE}
  {#if remote.prompt && remote.prompt.kind === "grant"}<GrantCard prompt={remote.prompt} onanswer={answer} />{/if}
  {#if remote.prompt && remote.prompt.kind === "question"}
    {#key JSON.stringify(remote.prompt.request)}<QuestionCard request={remote.prompt.request} onanswer={answer} />{/key}
  {/if}
  {#if remote.prompt && remote.prompt.kind === "terminal"}<section class="ask"><p class="ask-note">Waiting at the terminal: {remote.prompt.detail}</p></section>{/if}
{/if}
<div class="status">
  <span class="dot {state}"></span>
  <span class="status-say">{saying}</span>
  {#if driven && state === "working" && text.trim()}
    <button class="status-stop" type="button" disabled={aborting} onclick={abort}>Stop</button>
  {/if}
  <span class="status-facts">
    <span class="long">{head.count} events · in {compact(head.usage.input)} · out {compact(head.usage.output)}{spent ? " · " : ""}</span>{spent}
  </span>
</div>
{#if !SHARE}
  <div class="input">
    <div class="input-box">
      <textarea rows="1" placeholder="Message" bind:value={text} {onkeydown}></textarea>
    </div>
    {#if stoppable}
      <button class="round stop" type="button" disabled={aborting} onclick={abort} title="stop this turn" aria-label="stop this turn"></button>
    {:else}
      <button class="round send" type="button" disabled={busy || !text.trim()} onclick={send} title="send" aria-label="send">↑</button>
    {/if}
  </div>
{/if}
{#if remoteFailure || fate || (REMOTE && remote.notice)}<p class="fate">{remoteFailure || fate || remote.notice}</p>{/if}
