// The production build: the server (src/server.ts) with the app it serves bundled in, Tailwind's
// CSS made, into dist/; run with `bun dist/server.js`.

import tailwind from "bun-plugin-tailwind";
import { rm } from "node:fs/promises";

import wasmUrl from "./wasm-url.ts";

await rm("dist", { recursive: true, force: true });
const result = await Bun.build({
  entrypoints: ["src/server.ts"],
  outdir: "dist",
  target: "bun",
  minify: true,
  // shiki's languages and themes, loaded when first used
  splitting: true,
  plugins: [tailwind, wasmUrl],
  define: { "process.env.NODE_ENV": JSON.stringify("production") },
});
if (!result.success) {
  for (const log of result.logs) console.error(log);
  process.exit(1);
}
for (const out of result.outputs) console.log(`${out.path}  ${(out.size / 1024).toFixed(1)} KB`);
