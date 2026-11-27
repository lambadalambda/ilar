<!-- The one-line head of a tool or task row. -->
<script>
  import Spans from "./Spans.svelte";
  import { glyph, shorten, toolTitle } from "../lib/format.js";
  import { inline, preview } from "../lib/richtext.js";
  let { name, summary, agent = "", state, cwd = "", onclick = null } = $props();
</script>

<button class="tool-line" type="button" {onclick} disabled={!onclick}>
  <span class="glyph {state}">{#if state === "run"}<span class="spinner"></span>{:else if state === "err"}✕{:else}{glyph(name)}{/if}</span>
  <span class="tool-name">{toolTitle(name)}</span>
  {#if agent}<span class="chip">{agent}</span>{/if}
  <span class="tool-args" title={summary || ""}><Spans spans={inline(preview(shorten(summary, cwd), 140))} /></span>
  {#if onclick}<span class="chev">›</span>{/if}
</button>
