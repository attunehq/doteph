import type { APIRoute } from "astro";
import { loadGuide, isIndex, markdownPathFor } from "../lib/guide";

// llms.txt (https://llmstxt.org): a curated, machine-readable index of the
// docs so an agent can discover the guide and fetch any page as Markdown in
// one hop. It states the URL rule (page URL, minus the trailing slash GitHub
// Pages redirects to, plus `.md`) so an agent that lands on any HTML page can
// derive the Markdown URL, and each link points at that
// raw `.md` representation, rooted at the GitHub Pages base path.
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
  out.push(
    `Every page of the guide is available as raw Markdown: drop any trailing slash from the page URL and add \`.md\`. ${SITE}/guide/ becomes ${SITE}/guide.md, and ${SITE}/guide/concepts/ becomes ${SITE}/guide/concepts.md. Fetch the Markdown rather than the HTML; it is the same text without the site chrome. The links below already point at it, and ${SITE}/llms-full.txt has every page in one file.`
  );
  out.push("");
  out.push("## User guide");
  out.push("");
  if (index) {
    out.push(`- [${index.data.title}](${SITE}${markdownPathFor(index)}): ${index.data.summary}`);
  }
  for (const e of chapters) {
    out.push(`- [${e.data.title}](${SITE}${markdownPathFor(e)}): ${e.data.summary}`);
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
