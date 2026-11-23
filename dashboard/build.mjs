import { build } from "esbuild";
import postcss from "postcss";
import tailwind from "@tailwindcss/postcss";
import { cp, mkdir, readFile, readdir, rm, writeFile } from "node:fs/promises";
import { dirname, resolve, basename } from "node:path";
import { fileURLToPath } from "node:url";

const root = dirname(fileURLToPath(import.meta.url));
const dist = resolve(root, "dist");
const source = resolve(root, "src");
const standalone = path => resolve(source, "standalone", path);
const version = (await readFile(resolve(root, "../Cargo.toml"), "utf8")).match(/^version = "([^"]+)"/m)?.[1];
if (!version) throw new Error("Cargo package version missing");

await rm(dist, { recursive: true, force: true });
await mkdir(dist, { recursive: true });
const result = await build({
  absWorkingDir: root,
  entryPoints: [standalone("main.jsx")],
  outdir: dist,
  entryNames: "assets/[name]-[hash]",
  chunkNames: "assets/[name]-[hash]",
  assetNames: "assets/[name]-[hash]",
  publicPath: "/__dashboard/",
  bundle: true,
  splitting: true,
  format: "esm",
  jsx: "automatic",
  minify: true,
  metafile: true,
  loader: { ".js": "jsx", ".woff2": "file", ".woff": "file", ".ttf": "file", ".svg": "file" },
  alias: {
    "@": source,
    "open-sse": resolve(root, "open-sse"),
    "next/dynamic": standalone("dynamic.jsx"),
    "next/image": standalone("image.jsx"),
    "next/link": standalone("navigation.jsx"),
    "next/navigation": standalone("navigation.jsx"),
  },
  define: { "process.env.NODE_ENV": '"production"', "process.env": "{}", __NINEROUTER_VERSION__: JSON.stringify(version) },
  plugins: [{
    name: "dashboard-css",
    setup(builder) {
      builder.onLoad({ filter: /globals\.css$/ }, async ({ path }) => {
        const css = await postcss([tailwind({ base: root })])
          .process(await readFile(path, "utf8"), { from: path });
        return { contents: css.css, loader: "css", resolveDir: dirname(path) };
      });
    }
  }],
});
const entry = Object.entries(result.metafile.outputs).find(([, asset]) => asset.entryPoint === "src/standalone/main.jsx");
if (!entry) throw new Error("Dashboard entry asset missing");
const [, output] = entry;
const asset = path => "/__dashboard/assets/" + basename(path);
const html = `<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <meta name="theme-color" content="#0a0a0a">
  <title>9Router - AI Infrastructure Management</title>
  <link rel="icon" href="/favicon.svg">
  <link rel="manifest" href="/manifest.webmanifest">
  <link rel="stylesheet" href="${asset(output.cssBundle)}">
  <script type="module" src="${asset(entry[0])}"></script>
</head>
<body class="font-sans antialiased"><div id="app"></div></body>
</html>
`;
await cp(resolve(root, "public"), resolve(dist, "public"), { recursive: true });
await cp(resolve(root, "node_modules/monaco-editor/min/vs"), resolve(dist, "monaco/vs"), { recursive: true });

const notices = resolve(dist, "licenses");
await cp(resolve(root, "licenses"), notices, { recursive: true });
const npmPackages = new Set(["monaco-editor"]);
for (const input of Object.keys(result.metafile.inputs)) {
  const match = input.match(/^node_modules\/((?:@[^/]+\/)?[^/]+)/);
  if (match) npmPackages.add(match[1]);
}
for (const pkg of npmPackages) {
  const source = resolve(root, "node_modules", pkg);
  const files = (await readdir(source, { withFileTypes: true }))
    .filter(entry => entry.isFile() && /^(license|licence|copying|notice)(\.|$)/i.test(entry.name));
  if (files.length === 0) continue;
  const target = resolve(notices, pkg);
  await mkdir(target, { recursive: true });
  await Promise.all(files.map(file => cp(resolve(source, file.name), resolve(target, file.name))));
}
await writeFile(resolve(dist, "index.html"), html);
