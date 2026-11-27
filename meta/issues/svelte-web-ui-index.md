# Svelte web UI — index

## Summary

One Svelte renderer for every web view of a session: the phone app
(`web/remote`), `ilar serve` and share files. It replaces the Preact
`app.js` and looks like a native chat app on a phone. The phone app
also gets model and thinking selection and images.

## Subissues

- [The Svelte renderer](the-svelte-renderer.md) — first
- [The phone picks model and thinking](the-phone-picks-model-and-thinking.md)
- [Images to and from the phone](images-to-and-from-the-phone.md)

## Acceptance Criteria

- The phone app, `ilar serve` and a share file all render from the
  Svelte build, with nothing lost against the Preact app.
- From the phone, change the model and thinking level of the paired
  session, and send a prompt with a photo; images in the transcript
  show on the phone.

## Notes

- Decided with the user 2026-11-27: Svelte for all three views, native
  chat look, model/thinking and images first; settings are menu items
  in the app, not slash commands.
- Cargo builds must not need Node: the Svelte build output is committed.
