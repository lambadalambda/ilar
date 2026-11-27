<!-- A delegation: its child transcript, fetched only when opened. The
     link to the child is written when the task finishes, so a running
     one asks the server to look it up, and an open row follows the child
     while it works. -->
<script>
  import Pre from "./Pre.svelte";
  import Rows from "./Rows.svelte";
  import ToolLine from "./ToolLine.svelte";
  import { api, apiPath, DETACHED, REMOTE, SHARE } from "../lib/source.js";
  import { message } from "../lib/format.js";
  import { mergeSlice, pageRows, sessionLive, toolState, unanswerable, UNANSWERED } from "../lib/rows.js";
  let { call, result, sessionId, cwd, live } = $props();

  // How often a running task asks again, and how many times.
  const CHILD_POLL_MS = 3000;
  const CHILD_LOOKUP_TRIES = 20;

  let open = $state(false);
  let found = $state(null);
  let page = $state.raw(null);
  let pending = $state(false);
  let error = $state("");
  const recorded = $derived((result && result.child_session_id) || null);
  const child = $derived(recorded || found);
  const dead = $derived(unanswerable(call, live));
  const state = $derived(toolState(call, result, live));
  const answered = $derived(Boolean(result));

  const slice = (childId, from) =>
    api(apiPath(childId, "?invocation=" + encodeURIComponent(call.id) + (from === undefined ? "" : "&from=" + from)));

  // Look the child up once (a share file carries the answer); poll only
  // while the call could still be running and a server can answer.
  $effect(() => {
    if (!open || recorded || found) return;
    let alive = true;
    let tries = 0;
    const look = () => {
      tries += 1;
      if (tries > CHILD_LOOKUP_TRIES) clearInterval(timer);
      api(apiPath(sessionId, "/invocations/" + encodeURIComponent(call.id)))
        .then((answer) => alive && answer.child_session_id && (found = answer.child_session_id))
        .catch(() => {});
    };
    const timer = DETACHED || answered || dead ? null : setInterval(look, CHILD_POLL_MS);
    look();
    return () => {
      alive = false;
      clearInterval(timer);
    };
  });

  $effect(() => {
    if (!open || !child || page) return;
    let alive = true;
    slice(child)
      .then((loaded) => {
        if (!alive) return;
        page = loaded;
        error = "";
      })
      .catch((failure) => {
        if (!alive) return;
        error = SHARE
          ? "this delegation's own transcript is not in this file"
          : REMOTE
            ? "a delegation's own transcript is not sent to the phone"
            : message(failure);
      });
    return () => (alive = false);
  });

  // Follow a child that is still working.
  $effect(() => {
    if (DETACHED || !open || !child || answered || dead) return;
    let alive = true;
    const timer = setInterval(() => {
      slice(child)
        .then((fresh) => alive && (page = mergeSlice(page, fresh)))
        .catch(() => {});
    }, CHILD_POLL_MS);
    return () => {
      alive = false;
      clearInterval(timer);
    };
  });

  async function earlier() {
    if (pending || !page || !page.has_more) return;
    pending = true;
    const at = page.cursor;
    try {
      const head = await slice(child, at);
      if (page && page.cursor === at) {
        page = { ...page, events: head.events.concat(page.events), cursor: head.cursor, has_more: head.has_more };
      }
      error = "";
    } catch (failure) {
      error = message(failure);
    } finally {
      pending = false;
    }
  }

  const rows = $derived(page ? pageRows(page.events, page.cursor) : []);
</script>

<div class="tool task {state}" class:open>
  <ToolLine name="task" summary={call.summary} agent={call.agent && call.agent.name} {state} {cwd} onclick={() => (open = !open)} />
  {#if open}
    <div class="tool-body">
      {#if call.detail}<Pre text={call.detail} />{/if}
      {#if !child}
        <p class="note" class:error={dead}>{result ? "no child session recorded" : dead ? UNANSWERED : "starting the subagent…"}</p>
      {/if}
      {#if error}<p class="note">{error}</p>{/if}
      {#if child && !page && !error}<p class="note">loading…</p>{/if}
      {#if page}
        <div class="child">
          {#if page.has_more}
            <div class="earlier"><button type="button" disabled={pending} onclick={earlier}>{pending ? "loading…" : "load earlier"}</button></div>
          {/if}
          <Rows {rows} sessionId={child} {cwd} live={sessionLive(page.session)} />
          {#if !DETACHED}<a class="link" href={"#/s/" + encodeURIComponent(child)}>open the child session</a>{/if}
        </div>
      {/if}
    </div>
  {/if}
</div>
