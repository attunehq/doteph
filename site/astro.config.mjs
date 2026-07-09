// @ts-check
import { defineConfig } from "astro/config";
import { unified } from "@astrojs/markdown-remark";
import rehypeDocLinks from "./src/lib/rehype-doc-links.mjs";
import { BASE_PATH } from "./src/lib/base-path.mjs";

// GitHub Pages serves a project site (as opposed to a user/org site) under
// the repo name, so the site lives at attunehq.github.io/doteph rather than
// a domain root. `base` makes Astro aware of that prefix for routing and
// asset URLs; anything we hand-write (hrefs, the rehype link rewriter, the
// llms.txt generators) has to apply it too, via src/lib/base.ts.
export default defineConfig({
  site: "https://attunehq.github.io",
  base: BASE_PATH,
  trailingSlash: "never",
  build: {
    inlineStylesheets: "auto",
  },
  markdown: {
    // Rewrite the guide's relative `.md` links to site routes / GitHub URLs.
    // Astro 6 takes remark/rehype plugins through a `unified()` processor.
    processor: unified({ rehypePlugins: [rehypeDocLinks] }),
    // A light syntax theme that sits in the paper surface rather than
    // dropping a dark slab into the page. Code-block chrome is styled in the
    // docs prose CSS.
    shikiConfig: {
      theme: "github-light",
      wrap: false,
    },
  },
});
