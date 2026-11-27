<!-- The workspace: sessions (serve only), the chat, and the details as a
     side panel on a wide screen or a sheet from the header menu. -->
<script>
  import Chat from "./components/Chat.svelte";
  import Details from "./components/Details.svelte";
  import Sheet from "./components/Sheet.svelte";
  import Sidebar from "./components/Sidebar.svelte";
  import { api, DETACHED, REMOTE, SHARE, source } from "./lib/source.js";
  import { message } from "./lib/format.js";
  import { blankView, openTranscript } from "./lib/transcript.js";

  function routeId() {
    if (SHARE) return SHARE.id;
    if (REMOTE) return REMOTE.id;
    const hash = location.hash.replace(/^#/, "");
    return hash.startsWith("/s/") ? decodeURIComponent(hash.slice(3)) : null;
  }

  let id = $state(routeId());
  let sessions = $state.raw([]);
  let error = $state("");
  let drawer = $state(false);
  let menu = $state(false);

  // One fetch, one stream, two readers (chat and details). The view is
  // mutated in place; `version` is the rAF-coalesced paint counter.
  let view = $state.raw(blankView(null));
  let version = $state(0);
  let scheduled = false;
  const paint = () => {
    if (scheduled) return;
    scheduled = true;
    requestAnimationFrame(() => {
      scheduled = false;
      version += 1;
    });
  };
  $effect(() => {
    const opened = openTranscript(id, source, paint);
    view = opened;
    return () => opened.close();
  });

  $effect(() => {
    const onhash = () => {
      id = routeId();
      drawer = false;
      menu = false;
    };
    addEventListener("hashchange", onhash);
    return () => removeEventListener("hashchange", onhash);
  });

  // The listing is a slow poll: it keeps the dots honest.
  $effect(() => {
    let alive = true;
    const refresh = () =>
      api("/api/sessions")
        .then((listing) => {
          if (!alive) return;
          sessions = listing.sessions || [];
          error = "";
        })
        .catch((failure) => alive && (error = message(failure)));
    refresh();
    const timer = SHARE ? null : setInterval(refresh, 3000);
    return () => {
      alive = false;
      clearInterval(timer);
    };
  });

  const session = $derived(sessions.find((row) => row.id === id) || null);

  // The details are a side panel on a wide screen and a sheet otherwise:
  // one of them at a time, so one poll of the subagents.
  const query = matchMedia("(min-width: 1200px)");
  let wide = $state(query.matches);
  $effect(() => {
    const onchange = () => (wide = query.matches);
    query.addEventListener("change", onchange);
    return () => query.removeEventListener("change", onchange);
  });
</script>

<div class="app" class:solo={DETACHED} class:drawer>
  {#if !DETACHED}
    <Sidebar
      {sessions}
      current={id}
      {error}
      onpick={() => (drawer = false)}
      oncreated={(created) => {
        location.hash = "#/s/" + encodeURIComponent(created);
        drawer = false;
      }}
    />
    {#if drawer}<button class="scrim" type="button" aria-label="close" onclick={() => (drawer = false)}></button>{/if}
  {/if}
  <Chat {id} {view} {version} {session} ondrawer={() => (drawer = !drawer)} onmenu={() => (menu = true)} />
  {#if wide}
    <aside class="side-panel">{#if id}<Details {id} {view} {version} />{/if}</aside>
  {/if}
</div>
{#if menu && id && !wide}
  <Sheet title="Session" onclose={() => (menu = false)}>
    <Details {id} {view} {version} />
  </Sheet>
{/if}
