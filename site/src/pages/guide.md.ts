import type { APIRoute } from "astro";
import { loadGuide, isIndex } from "../lib/guide";
import { markdownResponse } from "../lib/markdown-response";

// The guide index's canonical Markdown URL: its page lives at /guide, so the
// "append .md to any guide URL" rule llms.txt states lands here. The same body
// is also served at /guide/index.md (by guide/[slug].md.ts) for older links.
export const GET: APIRoute = async () => {
  const index = (await loadGuide()).find(isIndex);
  if (!index) throw new Error("guide index (order: 0) is missing");
  return markdownResponse(index.body ?? "");
};
