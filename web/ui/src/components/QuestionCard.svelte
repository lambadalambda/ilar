<!-- The question tool's form, answered in the shape `QuestionResponse`
     reads; the session checks the answers against the request. -->
<script>
  let { request, onanswer } = $props();
  const questions = $derived(request.questions || []);
  let drafts = $state({});
  const set = (id, value) => (drafts[id] = { ...drafts[id], ...value });

  const answers = () =>
    questions.flatMap((question) => {
      const draft = drafts[question.id] || {};
      const other = draft.other && draft.other.trim() ? draft.other.trim() : undefined;
      if (question.type === "free_text") {
        const text = (draft.text || "").trim();
        return text ? [{ type: "free_text", question_id: question.id, text }] : [];
      }
      if (question.type === "single_choice") {
        return draft.option || other ? [{ type: "single_choice", question_id: question.id, option_id: draft.option, other }] : [];
      }
      const picked = Object.keys(draft.options || {}).filter((id) => draft.options[id]);
      return picked.length || other ? [{ type: "multiple_choice", question_id: question.id, option_ids: picked, other }] : [];
    });
</script>

<section class="ask">
  {#each questions as question (question.id)}
    <fieldset>
      <legend>{question.prompt}{question.required ? "" : " (optional)"}</legend>
      {#if question.description}<p class="ask-note">{question.description}</p>{/if}
      {#if question.type === "free_text"}
        <textarea class="field" rows="2" oninput={(event) => set(question.id, { text: event.target.value })}></textarea>
      {:else}
        {#each question.options || [] as option (option.id)}
          <label class="ask-option">
            <input
              type={question.type === "single_choice" ? "radio" : "checkbox"}
              name={question.id}
              onchange={(event) =>
                question.type === "single_choice"
                  ? set(question.id, { option: option.id })
                  : set(question.id, { options: { ...((drafts[question.id] || {}).options || {}), [option.id]: event.target.checked } })}
            />
            <span>{option.label}{#if option.description}<small>{option.description}</small>{/if}</span>
          </label>
        {/each}
      {/if}
      {#if question.allow_other}
        <input class="field" placeholder="Other" oninput={(event) => set(question.id, { other: event.target.value })} />
      {/if}
    </fieldset>
  {/each}
  <div class="ask-actions">
    <button type="button" onclick={() => onanswer({ type: "question", response: { status: "answered", answers: answers() } })}>Answer</button>
    <button type="button" class="ask-no" onclick={() => onanswer({ type: "question", response: { status: "cancelled" } })}>Dismiss</button>
  </div>
</section>
