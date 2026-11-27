<!-- A window of a log as rows (lib/rows.js descriptors). -->
<script>
  import Fold from "./Fold.svelte";
  import Images from "./Images.svelte";
  import Pre from "./Pre.svelte";
  import Rich from "./Rich.svelte";
  import TaskRow from "./TaskRow.svelte";
  import ToolRow from "./ToolRow.svelte";
  let { rows, sessionId, cwd = "", live } = $props();
</script>

{#each rows as row (row.key)}
  {#if row.kind === "user"}
    <div class="msg user">
      {#if row.event.text && row.event.text.trim()}<div class="bubble"><Rich text={row.event.text} /></div>{/if}
      <Images {sessionId} eventId={row.event.id} descriptors={row.event.images} />
    </div>
  {:else if row.kind === "assistant"}
    <div class="msg assistant"><Rich text={row.text} /></div>
  {:else if row.kind === "thinking"}
    <Fold label={row.label || "Thought"} headline={row.text} body={row.text} />
  {:else if row.kind === "note"}
    <Fold label={row.note.kind} headline={row.note.headline} body={row.note.body || ""} />
  {:else if row.kind === "failed"}
    <div class="msg failed">{row.text}</div>
  {:else if row.kind === "tool"}
    <ToolRow call={row.call} result={row.result} {sessionId} {cwd} {live} />
  {:else if row.kind === "task"}
    <TaskRow call={row.call} result={row.result} {sessionId} {cwd} {live} />
  {:else if row.kind === "result"}
    <div class="result-card" class:error={row.event.is_error}>
      <div class="who">result</div>
      {#if row.event.text}<Pre text={row.event.text} />{/if}
      <Images {sessionId} eventId={row.event.id} descriptors={row.event.images} />
    </div>
  {:else}
    <div class="divider">{row.text}</div>
  {/if}
{/each}
