<!-- One tool call: its head line, and on a tap its input and result. A
     truncated result is fetched whole only when someone opens it. -->
<script>
  import Images from "./Images.svelte";
  import Pre from "./Pre.svelte";
  import ToolLine from "./ToolLine.svelte";
  import { apiPath, apiText, REMOTE } from "../lib/source.js";
  import { message } from "../lib/format.js";
  import { toolState, unanswerable, UNANSWERED } from "../lib/rows.js";
  let { call, result, sessionId, cwd, live } = $props();
  let open = $state(false);
  let full = $state(null);
  const truncated = $derived(Boolean(result && result.truncated));
  const state = $derived(toolState(call, result, live));
  const dead = $derived(unanswerable(call, live));

  $effect(() => {
    if (!open || !truncated || full !== null) return;
    let alive = true;
    apiText(apiPath(sessionId, "/results/" + encodeURIComponent(result.tool_use_id)))
      .then((text) => alive && (full = text))
      .catch((error) =>
        alive && (full = REMOTE ? result.text + "\n\n[the full result is not sent to the phone]" : "could not load the full result: " + message(error)),
      );
    return () => (alive = false);
  });
</script>

<div class="tool {state}" class:open>
  <ToolLine name={call.name} summary={call.summary} agent={call.agent && call.agent.name} {state} {cwd} onclick={() => (open = !open)} />
  {#if open}
    <div class="tool-body">
      {#if call.diff && call.diff.length}<Pre lines={call.diff} />{:else if call.detail}<Pre text={call.detail} />{/if}
      {#if !result}<p class="note" class:error={dead}>{dead ? UNANSWERED : "running…"}</p>{/if}
      {#if result && (result.text || full)}<Pre text={full === null ? result.text : full} tone={result.is_error ? "error" : ""} />{/if}
      {#if result && truncated && full === null}<p class="note">loading the full result…</p>{/if}
      {#if result}<Images {sessionId} eventId={result.id} descriptors={result.images} />{/if}
    </div>
  {/if}
</div>
