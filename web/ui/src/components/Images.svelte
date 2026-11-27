<!-- A message's images. serve gives a URL, the phone a promise of one
     (fetched over the wire), a share file none: then the marker says
     what was there, as the terminal does. Tap opens the full image. -->
<script>
  import { imageUrl } from "../lib/source.js";
  import { tokens } from "../lib/format.js";
  let { sessionId, eventId, descriptors = [] } = $props();
  let viewing = $state(null);
  const label = (image) => image.media_type + " · " + tokens(image.bytes) + " bytes";
</script>

{#if descriptors && descriptors.length}
  <div class="images">
    {#each descriptors as image (image.n)}
      {@const url = imageUrl(sessionId, eventId, image.n)}
      {#if url === null}
        <span class="thumb-absent">[{label(image)}]</span>
      {:else}
        {#await url}
          <span class="thumb-absent">loading image…</span>
        {:then src}
          <button class="thumb" type="button" onclick={() => (viewing = src)} aria-label="open image">
            <img class="thumb" loading="lazy" alt={label(image)} {src} />
          </button>
        {:catch}
          <span class="thumb-absent">[{label(image)}]</span>
        {/await}
      {/if}
    {/each}
  </div>
{/if}
{#if viewing}
  <button class="viewer" type="button" onclick={() => (viewing = null)} aria-label="close image">
    <img src={viewing} alt="" />
  </button>
{/if}
