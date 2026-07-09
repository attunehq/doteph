// Rewrite the relative links inside the user-guide markdown so the same files
// render correctly on two surfaces at once:
//
//   - on GitHub, the source links (`./concepts.md`, `../developer-guide/...`)
//     resolve as-authored, so the docs stay readable in the repo.
//   - on the site, those `.md` links would 404, so we rewrite them at build
//     time: links that stay inside the published user-guide become
//     `/doteph/guide/*` routes, and links that point anywhere else (the
//     developer guide, the root README) become GitHub blob URLs.
//
// The docs are a single source of truth; this plugin is what lets one set of
// files serve both readers without hand-maintained, surface-specific links.

import { BASE_PATH } from "./base-path.mjs";

const REPO = "https://github.com/attunehq/doteph";
const BLOB = `${REPO}/blob/main`;

// All user-guide chapters live in this one flat directory, so a relative link's
// base is constant. If the guide ever gains subdirectories this needs the
// per-file directory instead.
const BASE_DIR = "docs/user-guide";

// Collapse `.`/`..` segments against a notional repo-root path (no filesystem).
function normalize(path) {
  const parts = [];
  for (const seg of path.split("/")) {
    if (seg === "" || seg === ".") continue;
    if (seg === "..") parts.pop();
    else parts.push(seg);
  }
  return parts.join("/");
}

function rewrite(href) {
  if (typeof href !== "string" || href === "") return href;
  // Leave absolute URLs and in-page anchors untouched. A leading "/" in the
  // source markdown would be a site-absolute path in Bastion, but this repo's
  // docs never author one, and if they did it would need the base-path
  // treatment below rather than passing through unprefixed.
  if (/^[a-z][a-z0-9+.-]*:/i.test(href)) return href;
  if (href.startsWith("#")) return href;

  const hashIdx = href.indexOf("#");
  const pathPart = hashIdx === -1 ? href : href.slice(0, hashIdx);
  const hash = hashIdx === -1 ? "" : href.slice(hashIdx);
  if (pathPart === "") return href;

  const resolved = normalize(`${BASE_DIR}/${pathPart}`);

  // A markdown file still inside the published guide -> a /guide route,
  // prefixed with the GitHub Pages base path.
  if (resolved.startsWith(`${BASE_DIR}/`) && resolved.endsWith(".md")) {
    const name = resolved.slice(BASE_DIR.length + 1, -3);
    const route = name === "README" ? "/guide" : `/guide/${name}`;
    return `${BASE_PATH}${route}${hash}`;
  }

  // Anything else (developer guide, root files) -> GitHub.
  return `${BLOB}/${resolved}${hash}`;
}

export default function rehypeDocLinks() {
  return (tree) => {
    const walk = (node) => {
      if (
        node.type === "element" &&
        node.tagName === "a" &&
        node.properties &&
        typeof node.properties.href === "string"
      ) {
        node.properties.href = rewrite(node.properties.href);
      }
      if (Array.isArray(node.children)) node.children.forEach(walk);
    };
    walk(tree);
  };
}
