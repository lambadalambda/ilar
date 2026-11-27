<!-- A bottom sheet on a phone, a dialog on a wide screen. -->
<script>
  let { title, onclose, children } = $props();
  const onkeydown = (event) => event.key === "Escape" && onclose();
</script>

<svelte:window {onkeydown} />
<!-- svelte-ignore a11y_click_events_have_key_events, a11y_no_static_element_interactions -->
<div class="sheet-scrim" onclick={(event) => event.target === event.currentTarget && onclose()}>
  <div class="sheet" role="dialog" aria-modal="true" aria-label={title}>
    <div class="sheet-grip"></div>
    <header class="sheet-head">
      <h2>{title}</h2>
      <button class="sheet-done" type="button" onclick={onclose}>Done</button>
    </header>
    {@render children()}
  </div>
</div>
