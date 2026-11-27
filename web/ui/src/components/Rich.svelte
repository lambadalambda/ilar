<!-- A model's or a person's text: the few markdown shapes replies are
     made of, parsed once per distinct string. -->
<script module>
  import { memo } from "../lib/memo.js";
  import { blocks } from "../lib/richtext.js";
  const parse = memo(blocks);
</script>

<script>
  import List from "./List.svelte";
  import Spans from "./Spans.svelte";
  // `streaming`: a new prefix every frame, which a cache would only fill.
  let { text, streaming = false } = $props();
  const parsed = $derived(streaming ? blocks(text) : parse(text));
  const align = (aligns, index) => (aligns[index] ? "al-" + aligns[index] : "");
</script>

<div class="rich">
  {#each parsed as block}
    {#if block.kind === "text"}
      <p class="md-p"><Spans spans={block.spans} /></p>
    {:else if block.kind === "code"}
      <pre class="code"><code>{block.text}</code></pre>
    {:else if block.kind === "heading"}
      <div class="md-h md-h{block.level}"><Spans spans={block.spans} /></div>
    {:else if block.kind === "list"}
      <List list={block} />
    {:else if block.kind === "table"}
      <div class="table-wrap">
        <table class="md-table">
          <thead>
            <tr>
              {#each block.header as cell, index}<th class={align(block.aligns, index)}><Spans spans={cell} /></th>{/each}
            </tr>
          </thead>
          <tbody>
            {#each block.rows as row}
              <tr>
                {#each row as cell, index}<td class={align(block.aligns, index)}><Spans spans={cell} /></td>{/each}
              </tr>
            {/each}
          </tbody>
        </table>
      </div>
    {/if}
  {/each}
</div>
