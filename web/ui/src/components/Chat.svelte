<!-- The open session: header, transcript, live step, composer. The tail
     stays pinned unless someone scrolled up. -->
<script>
  import Composer from "./Composer.svelte";
  import LiveStep from "./LiveStep.svelte";
  import Rows from "./Rows.svelte";
  import Spans from "./Spans.svelte";
  import { inline, preview } from "../lib/richtext.js";
  import { currentModel, pageRows, sessionLive, stabilize } from "../lib/rows.js";
  import { DETACHED } from "../lib/source.js";
  import { tick } from "svelte";
  let { id, view, version, session, ondrawer, onmenu } = $props();

  let stream = $state(null);
  let follow = true;
  let tailing = $state(true);

  let cache = new Map();
  const rows = $derived.by(() => {
    version;
    if (!id) return [];
    const next = stabilize(cache, pageRows(view.events, view.base));
    cache = next.cache;
    return next.rows;
  });
  const head = $derived.by(() => {
    version;
    return view.session || session || {};
  });
  // The view changes in place; these are its flags as of this paint.
  const flags = $derived.by(() => {
    version;
    const { loading, error, retryable, hasMore, pending, rewound } = view;
    return { loading, error, retryable, hasMore, pending, rewound };
  });
  // The listing's word first: it is newer than the page load.
  const live = $derived.by(() => {
    version;
    return sessionLive(session || view.session);
  });
  const state = $derived((session && session.state) || (head && head.state) || "idle");
  const model = $derived.by(() => {
    version;
    return currentModel(view.events, head.model);
  });

  $effect(() => {
    version;
    id;
    tick().then(() => {
      if (follow && stream) stream.scrollTop = stream.scrollHeight;
    });
  });
  $effect(() => {
    id;
    follow = true;
    tailing = true;
  });

  function onscroll() {
    const bottom = stream.scrollHeight - stream.scrollTop - stream.clientHeight < 80;
    follow = bottom;
    tailing = bottom;
  }
  function toBottom() {
    stream.scrollTop = stream.scrollHeight;
    follow = true;
    tailing = true;
  }
</script>

<main class="chat">
  <header class="chat-head">
    <button class="icon-button drawer-toggle" class:hidden={DETACHED} type="button" onclick={ondrawer} aria-label="sessions">☰</button>
    <div class="chat-title">
      <h1>{#if id}<Spans spans={inline(preview(head.title || "New session", 120))} />{:else}ilar{/if}</h1>
      {#if id}
        <div class="chat-sub"><span class="dot {state}"></span>{model}{state === "working" ? " · working" : ""}</div>
      {/if}
    </div>
    <button class="icon-button menu-toggle" class:hidden={!id} type="button" onclick={onmenu} aria-label="session menu">⋯</button>
  </header>
  {#if flags.error}
    <p class="banner">
      <span>{flags.error}</span>
      {#if flags.retryable}<button class="pill-button" type="button" onclick={() => view.retry()}>Retry</button>{/if}
    </p>
  {/if}
  <div class="stream" bind:this={stream} {onscroll}>
    <div class="stream-inner">
      {#if !id}<p class="empty">Pick a session.</p>{/if}
      {#if id && flags.loading}<p class="empty">Loading…</p>{/if}
      {#if flags.hasMore}
        <div class="earlier"><button type="button" disabled={flags.pending} onclick={() => view.earlier()}>{flags.pending ? "Loading…" : "Load earlier"}</button></div>
      {/if}
      <Rows {rows} sessionId={id} cwd={head.cwd} {live} />
      <LiveStep live={view.live} {version} />
      {#if flags.rewound !== null}<div class="divider">rewound {flags.rewound} events</div>{/if}
    </div>
  </div>
  {#if id}
    <footer class="composer">
      {#if !tailing}<button class="jump" type="button" onclick={toBottom}>Jump to latest ↓</button>{/if}
      <div class="composer-inner">
        {#key id}<Composer {id} {session} {view} {version} />{/key}
      </div>
    </footer>
  {/if}
</main>
