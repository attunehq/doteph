import { defineCollection, z } from "astro:content";
import { glob } from "astro/loaders";

// The user guide is single-sourced from the repo-root `docs/user-guide`
// markdown (the same files people read on GitHub). The site renders those
// files in place rather than copying them, so the docs never drift between
// surfaces. Frontmatter (title, summary, order) drives the sidebar, page
// metadata, and the llms.txt index; it is the entire contract this site
// depends on, so chapter prose can be rewritten freely without touching here.
const guide = defineCollection({
  loader: glob({ pattern: "**/*.md", base: "../docs/user-guide" }),
  schema: z.object({
    title: z.string(),
    summary: z.string(),
    order: z.number(),
  }),
});

export const collections = { guide };
