<!-- The paired session's model and thinking level, as the TUI's picker
     sets them. A row opens its list in place; a choice goes to the
     session, and the menu changes when the session says it switched (a
     refusal comes back as the composer's notice). -->
<script>
  import { message } from "../lib/format.js";
  import { REMOTE } from "../lib/source.js";

  let menu = $state.raw(REMOTE.state().models);
  $effect(() => REMOTE.subscribe(() => (menu = REMOTE.state().models)));

  let open = $state(""); // "", "model" or "thinking"
  let failure = $state("");
  const current = $derived(menu ? menu.models.find((model) => model.id === menu.current) : null);
  const levelName = (model, id) => (id ? (model && model.variants.find((level) => level.id === id)?.name) || id : "Default");

  function choose(model, variant) {
    failure = "";
    open = "";
    REMOTE.send({ type: "model", model, variant }).catch((error) => (failure = message(error)));
  }
  const toggle = (name) => (open = open === name ? "" : name);
</script>

{#if menu}
  <div class="list-title">Model</div>
  <div class="list">
    <button class="list-row" type="button" onclick={() => toggle("model")}>
      <span class="label">Model</span>
      <span class="value">{current ? current.name : menu.current}</span>
      <span class="chev">›</span>
    </button>
    {#if open === "model"}
      {#each menu.models as model (model.id)}
        <button class="list-row" type="button" onclick={() => choose(model.id, null)}>
          <span class="label">{model.name}<small class="mono">{model.id}</small></span>
          {#if model.id === menu.current}<span class="check">✓</span>{/if}
        </button>
      {/each}
    {/if}
    {#if current && current.variants.length}
      <button class="list-row" type="button" onclick={() => toggle("thinking")}>
        <span class="label">Thinking</span>
        <span class="value">{levelName(current, menu.variant)}</span>
        <span class="chev">›</span>
      </button>
      {#if open === "thinking"}
        {#each [{ id: null, name: "Default" }, ...current.variants] as level (level.id)}
          <button class="list-row" type="button" onclick={() => choose(current.id, level.id)}>
            <span class="label">{level.name}</span>
            {#if level.id === (menu.variant ?? null)}<span class="check">✓</span>{/if}
          </button>
        {/each}
      {/if}
    {/if}
  </div>
  {#if failure}<p class="note error">{failure}</p>{/if}
{/if}
