// One self-contained ES module and one stylesheet, written where the
// binary embeds them (include_str! in crates/ilar-tui/src/web/mod.rs):
// no chunks and no imports, so serve, a share file and the phone app
// load the same two files with no import map.
import { defineConfig } from "vite";
import { svelte } from "@sveltejs/vite-plugin-svelte";

// A share file inlines app.js in a <script> block, which `</script` or
// `<!--` would end or derail (web/share.rs guards this). Svelte's runtime
// has `<!---->` in a template literal; `\x3C` is `<` in a string, a
// template literal and a regex alike, so the escape changes no meaning.
const scriptSafe = {
  name: "script-safe",
  // After minification, which would undo it.
  generateBundle(options, bundle) {
    for (const file of Object.values(bundle)) {
      if (file.type === "chunk") file.code = file.code.replace(/<(!--|\/script)/gi, "\\x3C$1");
    }
  },
};

export default defineConfig({
  plugins: [svelte(), scriptSafe],
  build: {
    outDir: "../../crates/ilar-tui/src/web/assets",
    emptyOutDir: false,
    cssCodeSplit: false,
    modulePreload: false,
    target: "es2022",
    rollupOptions: {
      input: "src/main.js",
      preserveEntrySignatures: false,
      output: {
        format: "es",
        entryFileNames: "app.js",
        assetFileNames: "app[extname]",
        codeSplitting: false,
      },
    },
  },
});
