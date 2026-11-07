// The operator's font rule for every site of ours, checked over the web
// UI's sources and the placeholder page vessel-web embeds.
//
// [Decision Log]
// - Purpose: Windows Chrome in Korean draws the generic `monospace` and
//   `serif` families with bitmap Gulim and Batang, which look like pixel
//   fonts at small sizes. Every font stack must therefore name Pretendard
//   before any generic family; code and terminal stacks name a real
//   monospace face first, then Pretendard, then `monospace`. Gulim, Dotum,
//   Batang and the IBM Plex family are never named.
// - Alternatives: a Stylelint plugin (a dependency and a config for one
//   rule, and it would not see the stacks in TypeScript, such as
//   xterm.js's `fontFamily`).
// - Choice: this script, reading every font stack it can find: CSS
//   `font-family` and `--font-*` declarations, and `fontFamily` / `font`
//   string literals in TypeScript. Stacks built from `var()` are checked
//   where the variable is defined.
// - Cost: a stack assembled at run time from pieces would escape it; none
//   is, and review keeps it so.
import { readdirSync, readFileSync, statSync } from "node:fs";
import { join, relative } from "node:path";
import { fileURLToPath } from "node:url";

const web = fileURLToPath(new URL("..", import.meta.url));
// Sources, the placeholder page, and the built CSS when there is a build
// (third-party stylesheets end up there; their JavaScript only holds
// defaults the app overrides, so it is not read).
const roots = [
  [join(web, "src"), [".css", ".html", ".ts", ".tsx"]],
  [join(web, "index.html"), [".html"]],
  [join(web, "..", "crates", "vessel-web", "assets"), [".css", ".html"]],
  [join(web, "dist", "assets"), [".css"]],
];

const GENERIC = new Set([
  "serif",
  "sans-serif",
  "monospace",
  "cursive",
  "fantasy",
  "system-ui",
  "ui-serif",
  "ui-sans-serif",
  "ui-monospace",
  "ui-rounded",
  "math",
  "emoji",
  "fangsong",
]);
const KEYWORDS = new Set([
  "inherit",
  "initial",
  "unset",
  "revert",
  "revert-layer",
]);
const FORBIDDEN = /gulim|dotum|batang|굴림|돋움|바탕|ibm\s*plex/i;

/** Every file under `path` with one of `extensions`. */
function files(path, extensions) {
  const stat = statSync(path, { throwIfNoEntry: false });
  if (stat === undefined) {
    return [];
  }
  if (stat.isFile()) {
    return extensions.some((extension) => path.endsWith(extension))
      ? [path]
      : [];
  }
  return readdirSync(path).flatMap((name) =>
    files(join(path, name), extensions),
  );
}

/** The font stacks in `text`, with their line numbers. */
function stacks(text, path) {
  const found = [];
  const patterns =
    path.endsWith(".ts") || path.endsWith(".tsx")
      ? [
          /fontFamily\s*[:=]\s*(["'`])([^"'`]*)\1/g,
          /\bfont\s*[:=]\s*(["'`])([^"'`]*)\1/g,
        ]
      : [/(?:^|[\s;{])(?:font-family|--font-[\w-]+)\s*:\s*()([^;}]+)/g];
  for (const pattern of patterns) {
    for (const match of text.matchAll(pattern)) {
      const value = match[2].trim();
      const line = text.slice(0, match.index).split("\n").length;
      found.push({ value, line });
    }
  }
  return found;
}

/** The families of a stack (a `font` shorthand keeps only its list). */
function families(value) {
  return value
    .split(",")
    .map((family) => family.trim().replace(/^["']|["']$/g, ""))
    .map((family, index) =>
      index === 0
        ? family.replace(/^.*\d(?:px|rem|em|%)(?:\s*\/\s*[\d.]+\w*)?\s+/, "")
        : family,
    )
    .filter((family) => family !== "");
}

/** What is wrong with `value`, if anything. */
function problem(value) {
  if (value.includes("var(")) {
    return null;
  }
  const list = families(value);
  if (list.length === 1 && KEYWORDS.has(list[0].toLowerCase())) {
    return null;
  }
  const forbidden = list.find((family) => FORBIDDEN.test(family));
  if (forbidden !== undefined) {
    return `names ${forbidden}, which is never used on our sites`;
  }
  const lower = list.map((family) => family.toLowerCase());
  const generic = lower.findIndex((family) => GENERIC.has(family));
  const pretendard = lower.findIndex((family) =>
    family.startsWith("pretendard"),
  );
  if (generic === -1) {
    return pretendard === -1 ? "has no Pretendard and no generic family" : null;
  }
  if (pretendard === -1 || pretendard > generic) {
    return `puts ${list[generic]} before Pretendard`;
  }
  const mono = lower.includes("monospace") || lower.includes("ui-monospace");
  if (mono && pretendard === 0) {
    return "is a code stack without a real monospace face before Pretendard";
  }
  return null;
}

const problems = [];
let checked = 0;
for (const [root, extensions] of roots) {
  for (const path of files(root, extensions)) {
    if (path.includes(`${join("src", "wire")}`)) {
      continue;
    }
    const text = readFileSync(path, "utf8");
    for (const { value, line } of stacks(text, path)) {
      checked += 1;
      const wrong = problem(value);
      if (wrong !== null) {
        problems.push(
          `${relative(join(web, ".."), path)}:${line}: \`${value}\` ${wrong}`,
        );
      }
    }
  }
}

if (problems.length > 0) {
  process.stderr.write(
    `Font rule broken (Pretendard before any generic family):\n${problems.join("\n")}\n`,
  );
  process.exitCode = 1;
} else {
  process.stdout.write(`fonts: ${checked} stacks follow the rule\n`);
}
