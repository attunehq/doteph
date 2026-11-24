import type { APIRoute, GetStaticPaths } from "astro";
import { loadGuide, rawSlugFor } from "../../lib/guide";
import { markdownResponse } from "../../lib/markdown-response";

// Serve every guide page's raw Markdown at a predictable `.md` URL (e.g.
// /doteph/guide/concepts.md). This is the representation agents and crawlers
// probe for, and the exact text the "Copy page" button copies. The index is
// emitted here as /guide/index.md, an alias kept for existing links; its
// canonical URL is /guide.md (see ../guide.md.ts).
export const getStaticPaths = (async () => {
  const entries = await loadGuide();
  return entries.map((entry) => ({
    params: { slug: rawSlugFor(entry) },
    props: { body: entry.body ?? "" },
  }));
}) satisfies GetStaticPaths;

export const GET: APIRoute = ({ props }) => markdownResponse(props.body as string);
