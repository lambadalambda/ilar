<!-- A long thing folded to one line: reasoning, a compaction summary, a
     recalled memory, a background task reporting back. -->
<script>
  import Rich from "./Rich.svelte";
  import Spans from "./Spans.svelte";
  import { inline, preview } from "../lib/richtext.js";
  let { label, headline, body = "" } = $props();
  let open = $state(false);
</script>

<div class="fold" class:open>
  <button class="fold-line" type="button" onclick={() => (open = !open)} disabled={!body}>
    <span class="fold-label">{label}</span>
    <span class="fold-preview"><Spans spans={inline(preview(headline, 120))} /></span>
    {#if body}<span class="chev">›</span>{/if}
  </button>
  {#if open && body}<div class="fold-body"><Rich text={body} /></div>{/if}
</div>
