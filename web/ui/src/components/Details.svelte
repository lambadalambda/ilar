<!-- What the session is: its context window, model, agent, directory, and
     the subagents it started. The panel on a wide screen, a sheet's body
     otherwise. -->
<script>
  import { api, apiPath, DETACHED, SHARE } from "../lib/source.js";
  import { basename, compact, contextTokens, tokens } from "../lib/format.js";
  import { currentModel, digest } from "../lib/rows.js";
  import { preview } from "../lib/richtext.js";
  let { id, view, version } = $props();

  let children = $state.raw([]);
  $effect(() => {
    children = [];
    if (!id) return;
    let alive = true;
    // One dropped poll is not "no subagents": keep the last list.
    const refresh = () =>
      api(apiPath(id, "/children"))
        .then((page) => alive && (children = page.children || []))
        .catch(() => {});
    refresh();
    const timer = SHARE ? null : setInterval(refresh, 5000);
    return () => {
      alive = false;
      clearInterval(timer);
    };
  });

  const facts = $derived.by(() => {
    version;
    const facts = digest(view.events);
    return { ...facts, model: currentModel(view.events, (view.session && view.session.model) || facts.meta.model) };
  });
  const session = $derived.by(() => {
    version;
    return view.session || {};
  });
  // The newest turn's usage is what the model was holding.
  const used = $derived(contextTokens(facts.latest && facts.latest.usage));
  const limit = $derived(session.context_limit);
  const share = $derived(limit ? Math.min(1, used / limit) : 0);
  const level = $derived(share > 0.9 ? "hot" : share > 0.7 ? "warm" : "cool");
  const running = $derived(children.filter((child) => child.state === "working" || child.state === "stalled").length);
  const childState = (state) => (state === "working" ? "working" : state === "stalled" ? "stalled" : "done");
</script>

<div class="list-title">Context</div>
<div class="list">
  <div class="list-row">
    <span class="label">{compact(used)} tokens</span>
    <span class="value" title={limit ? tokens(used) + " of " + tokens(limit) : ""}>{limit ? (share * 100).toFixed(1) + "% of " + compact(limit) : "no known limit"}</span>
  </div>
  {#if limit}<div class="bar"><div class="bar-fill {level}" style:width={(share * 100).toFixed(2) + "%"}></div></div>{/if}
</div>

<div class="list-title">Session</div>
<div class="list">
  <div class="list-row"><span class="label">Model</span><span class="value mono">{facts.model || "—"}</span></div>
  <div class="list-row"><span class="label">Agent</span><span class="value mono">{session.agent || facts.meta.agent || "—"}</span></div>
  <div class="list-row"><span class="label">Directory</span><span class="value mono" title={session.cwd || facts.meta.cwd || ""}>{basename(session.cwd || facts.meta.cwd) || "—"}</span></div>
  {#if facts.compactions}<div class="list-row"><span class="label">Compactions</span><span class="value">{facts.compactions}</span></div>{/if}
</div>

<div class="list-title">Subagents {children.length ? running + "/" + children.length : ""}</div>
<div class="list">
  {#if !children.length}<div class="list-row"><span class="label note">none</span></div>{/if}
  {#each children as child (child.id)}
    {#if DETACHED}
      <div class="list-row"><span class="label">{preview(child.title || child.agent || child.id, 44)}</span><span class="value state-{childState(child.state)}">{childState(child.state)}</span></div>
    {:else}
      <a class="list-row" href={"#/s/" + encodeURIComponent(child.id)}><span class="label">{preview(child.title || child.agent || child.id, 44)}</span><span class="value state-{childState(child.state)}">{childState(child.state)}</span></a>
    {/if}
  {/each}
</div>
