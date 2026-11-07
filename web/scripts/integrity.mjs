// A port squatter can pre-fill the browser cache before our process starts.
// SRI in the freshly served index authenticates every executable asset.
// Keep the graph closed: an unreferenced JS chunk would import without SRI.
import { createHash } from "node:crypto";
import { readFileSync, readdirSync, writeFileSync } from "node:fs";
import { join } from "node:path";

const root = new URL("../dist/", import.meta.url);
const index = new URL("index.html", root);
const referenced = new Set();
const html = readFileSync(index, "utf8").replace(
  /<(script|link)\b[^>]*>/g,
  (tag) => {
    if (!tag.startsWith("<script") && !/rel="stylesheet"/.test(tag)) {
      return tag;
    }
    const path = /(?:src|href)="(\/assets\/[^"?#]+)"/.exec(tag)?.[1];
    if (path === undefined) {
      throw new Error("Executable HTML must reference a bundled asset");
    }
    referenced.add(path.slice("/assets/".length));
    const digest = createHash("sha384")
      .update(readFileSync(new URL(path.slice(1), root)))
      .digest("base64");
    return tag.replace(/>$/, ` integrity="sha384-${digest}">`);
  },
);
for (const file of readdirSync(new URL("assets/", root))) {
  if (/\.(?:m?js|css)$/.test(file) && !referenced.has(file)) {
    throw new Error(`Asset ${join("assets", file)} is outside the SRI graph`);
  }
}
writeFileSync(index, html);
