<!-- Program output: a diff if it reads like one, plain otherwise. Never
     markdown. `lines` is the projected diff of an edit, when sent. -->
<script module>
  import { memo } from "../lib/memo.js";
  import { diffKind, looksLikeDiff } from "../lib/richtext.js";
  const asDiff = memo((text) => (looksLikeDiff(text) ? text.split("\n").map((line) => ({ kind: diffKind(line), text: line })) : null), 1000);
  const mark = (kind) => (kind === "add" ? "+" : kind === "del" ? "-" : " ");
</script>

<script>
  let { text = "", lines = null, tone = "" } = $props();
  const diff = $derived(lines ? lines.map((line) => ({ kind: line.kind, text: mark(line.kind) + line.text })) : asDiff(text));
</script>

{#if diff}<pre class="diff">{#each diff as line}<span class={line.kind}>{line.text + "\n"}</span>{/each}</pre>{:else}<pre class="detail {tone}">{text}</pre>{/if}
