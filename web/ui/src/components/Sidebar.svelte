<!-- serve's session list, by directory, with "new session". -->
<script>
  import NewSession from "./NewSession.svelte";
  import Spans from "./Spans.svelte";
  import { age, basename } from "../lib/format.js";
  import { grouped } from "../lib/rows.js";
  import { inline, preview } from "../lib/richtext.js";
  let { sessions, current, error, onpick, oncreated } = $props();
  const groups = $derived(grouped(sessions));
  const working = $derived(sessions.filter((session) => session.state === "working").length);
  const cwds = $derived([...new Set(sessions.map((session) => session.cwd).filter(Boolean))].sort());
  let closed = $state({});
</script>

<aside class="sidebar">
  <header class="sidebar-head">
    <a class="brand" href="#/">ilar</a>
    <span class="sidebar-count">{sessions.length} session{sessions.length === 1 ? "" : "s"}{#if working}<span class="working"> · {working} working</span>{/if}</span>
  </header>
  <NewSession {cwds} {oncreated} />
  <div class="sidebar-body">
    {#if error}<p class="note">{error}</p>{/if}
    {#if !error && !sessions.length}<p class="note">No sessions in this store yet.</p>{/if}
    {#each groups as group (group.cwd)}
      <section class="group">
        <button class="group-head" type="button" title={group.cwd || "sessions with no directory"} onclick={() => (closed[group.cwd] = !closed[group.cwd])}>
          <span class="group-name">{group.cwd ? basename(group.cwd) : "elsewhere"}</span>
          <span>{group.rows.length}</span>
        </button>
        {#if !closed[group.cwd]}
          {#each group.rows as session (session.id)}
            <a class="session" class:current={session.id === current} href={"#/s/" + encodeURIComponent(session.id)} title={session.cwd || ""} onclick={onpick}>
              <span class="dot {session.state || 'idle'}" title={session.state === "stalled" ? "a turn is running but has not written for a minute" : session.state || "idle"}></span>
              <span class="session-title"><Spans spans={inline(preview(session.title || session.id, 70))} /></span>
              <span class="session-age">{age(session.modified)}</span>
            </a>
          {/each}
        {/if}
      </section>
    {/each}
  </div>
</aside>
