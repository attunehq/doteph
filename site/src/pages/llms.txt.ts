import type { APIRoute } from "astro";
import { loadGuide, isIndex, rawSlugFor } from "../lib/guide";

// llms.txt (https://llmstxt.org): a curated, machine-readable index of the
// docs so an agent can discover the guide and fetch any page as Markdown in
// one hop. Each link points at the raw `.md` representation served from
// /guide, rooted at the GitHub Pages base path.
const SITE = "https://attunehq.github.io/doteph";

export const GET: APIRoute = async () => {
  const entries = await loadGuide();
  const index = entries.find(isIndex);
  const chapters = entries.filter((e) => !isIndex(e));

  const out: string[] = [];
  out.push("# eph");
  out.push("");
  out.push(
    "> Ephemeral services per workspace: like .env files, but for services. A small Rust CLI that starts your dev services (Postgres, Redis, MinIO, your own app) isolated per workspace, with host ports assigned automatically."
  );
  out.push("");
  out.push(
    "You describe services in a `.eph` file. `eph up` starts them, namespaced by a hash of the workspace path so two checkouts never collide. `eval \"$(eph env)\"` loads the resolved connection strings into your shell. `eph down` stops them."
  );
  out.push("");
  out.push("## User guide");
  out.push("");
  if (index) {
    out.push(`- [${index.data.title}](${SITE}/guide/index.md): ${index.data.summary}`);
  }
  for (const e of chapters) {
    out.push(`- [${e.data.title}](${SITE}/guide/${rawSlugFor(e)}.md): ${e.data.summary}`);
  }
  out.push("");
  out.push("## Optional");
  out.push("");
  out.push(`- [Full guide as one file](${SITE}/llms-full.txt): every chapter concatenated for a single fetch.`);
  out.push(`- [Source repository](https://github.com/attunehq/doteph): the CLI source, install scripts, and the developer guide.`);
  out.push("");

  return new Response(out.join("\n"), {
    headers: { "Content-Type": "text/plain; charset=utf-8" },
  });
};
