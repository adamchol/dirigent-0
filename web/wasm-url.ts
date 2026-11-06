// A Bun bundler plugin: chlive's WebAssembly core (chlive-client/wasm, wasm-bindgen's `web`
// output) finds its .wasm by `new URL(…, import.meta.url)`, which Bun's bundler leaves as the
// source file's path. Here the .wasm is imported as a file instead, so it's bundled as an asset
// and found at its URL.

import type { BunPlugin } from "bun";

const plugin: BunPlugin = {
  name: "wasm-bindgen-url",
  setup(build) {
    build.onLoad({ filter: /chlive_wasm\.js$/ }, async (args) => {
      const src = await Bun.file(args.path).text();
      const found = "new URL('chlive_wasm_bg.wasm', import.meta.url)";
      if (!src.includes(found)) throw new Error(`${args.path}: no ${found}`);
      return {
        loader: "js",
        contents: `import wasmUrl from "./chlive_wasm_bg.wasm" with { type: "file" };\n${src.replace(found, "new URL(wasmUrl, location.href)")}`,
      };
    });
  },
};

export default plugin;
