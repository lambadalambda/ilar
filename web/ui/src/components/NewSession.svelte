<!-- Start a session: what to ask, where it runs, which model. The
     directory is free text with known ones suggested (the browser cannot
     see the filesystem). -->
<script>
  import { DETACHED, post } from "../lib/source.js";
  import { message } from "../lib/format.js";
  let { cwds, oncreated } = $props();
  let open = $state(false);
  let prompt = $state("");
  let cwd = $state("");
  let model = $state("");
  let busy = $state(false);
  let error = $state("");

  async function submit(event) {
    event.preventDefault();
    if (!prompt.trim() || busy) return;
    busy = true;
    error = "";
    try {
      const created = await post("/api/sessions", { prompt: prompt.trim(), cwd: cwd.trim() || null, model: model.trim() || null });
      prompt = "";
      open = false;
      oncreated(created.id);
    } catch (failure) {
      error = message(failure);
    } finally {
      busy = false;
    }
  }
</script>

{#if !DETACHED}
  {#if !open}
    <button class="new-toggle" type="button" onclick={() => (open = true)}>New session</button>
  {:else}
    <form class="new-session" onsubmit={submit}>
      <!-- svelte-ignore a11y_autofocus -->
      <textarea rows="3" required autofocus placeholder="What should it do?" bind:value={prompt}></textarea>
      <input list="known-cwds" placeholder="Directory (this server's, if blank)" bind:value={cwd} />
      <datalist id="known-cwds">{#each cwds as known}<option value={known}></option>{/each}</datalist>
      <input placeholder="Model (config default)" bind:value={model} />
      {#if error}<p class="note error">{error}</p>{/if}
      <div class="row-end">
        <button type="button" class="pill-button" onclick={() => (open = false)}>Cancel</button>
        <button type="submit" class="pill-button" disabled={busy || !prompt.trim()}>{busy ? "Starting…" : "Start"}</button>
      </div>
    </form>
  {/if}
{/if}
