<!-- Inline spans (see lib/richtext.js). Every value is a text node; links
     are http(s) only. Kept on one line: whitespace here is on screen. -->
<script>
  import Spans from "./Spans.svelte";
  let { spans } = $props();
</script>

{#each spans as span}{#if span.t === "code"}<code class="inline">{span.v}</code>{:else if span.t === "bold"}<strong><Spans spans={span.spans} /></strong>{:else if span.t === "link"}<a class="link" href={span.v} target="_blank" rel="noreferrer noopener">{span.v}</a>{:else}{span.v}{/if}{/each}
