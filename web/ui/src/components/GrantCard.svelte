<!-- A tool wants a secret or a command approved. The detail is what it is
     about to run, verbatim and whole: it is the whole of the safety. -->
<script>
  import Pre from "./Pre.svelte";
  let { prompt, onanswer } = $props();
  const ANSWERS = [
    ["once", "Allow once"],
    ["session", "This session"],
    ["always", "Always"],
  ];
</script>

<section class="ask">
  <h2>{prompt.tool} wants {prompt.secret}{prompt.agent ? " (" + prompt.agent + ")" : ""}</h2>
  {#if prompt.description}<p class="ask-note">{prompt.description}</p>{/if}
  {#if prompt.detail}<Pre text={prompt.detail} />{/if}
  <div class="ask-actions">
    {#each ANSWERS as [answer, label]}
      <button type="button" onclick={() => onanswer({ type: "grant", secret: prompt.secret, answer })}>{label}</button>
    {/each}
    <button type="button" class="ask-no" onclick={() => onanswer({ type: "grant", secret: prompt.secret, answer: null })}>Deny</button>
  </div>
</section>
