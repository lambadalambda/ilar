<!-- What the running step has produced so far. One thing says "this is
     happening now": the spinner while a tool runs, the caret otherwise. -->
<script>
  import Rich from "./Rich.svelte";
  import Spans from "./Spans.svelte";
  import ToolLine from "./ToolLine.svelte";
  import { inline, preview } from "../lib/richtext.js";
  // `live` changes in place (lib/rows.js liveApply); `version` is the
  // paint counter that says it did.
  let { live, version } = $props();
  const step = $derived.by(() => {
    version;
    if (!live) return null;
    return { thoughts: live.thoughts.filter((one) => one.trim()), text: live.text, tools: live.tools.map((tool) => ({ ...tool })) };
  });
  const thoughts = $derived(step ? step.thoughts : []);
  const writing = $derived(step ? step.text.trim() : "");
  const busy = $derived(step ? step.tools.some((tool) => tool.ok === undefined) : false);
</script>

{#if step && (thoughts.length || writing || step.tools.length)}
  <div class="live">
    {#each thoughts as thought, index}
      <div class="live-thought">
        <span class="fold-label">Thinking</span>
        <span class="fold-preview" class:active={!busy && !writing && index === thoughts.length - 1}><Spans spans={inline(preview(thought, 160))} /></span>
      </div>
    {/each}
    {#if writing}<div class="msg assistant live-text" class:active={!busy}><Rich text={step.text} streaming /></div>{/if}
    {#each step.tools as tool}
      <div class="tool"><ToolLine name={tool.name} summary={tool.summary} state={tool.ok === undefined ? "run" : tool.ok ? "ok" : "err"} /></div>
    {/each}
  </div>
{/if}
