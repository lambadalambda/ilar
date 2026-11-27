<script>
  import List from "./List.svelte";
  import Spans from "./Spans.svelte";
  let { list } = $props();
</script>

{#if list.ordered}
  <ol class="md-list" start={list.start}>
    {#each list.items as item}
      <li><Spans spans={item.spans} />{#if item.children}<List list={item.children} />{/if}</li>
    {/each}
  </ol>
{:else}
  <ul class="md-list">
    {#each list.items as item}
      <li><Spans spans={item.spans} />{#if item.children}<List list={item.children} />{/if}</li>
    {/each}
  </ul>
{/if}
