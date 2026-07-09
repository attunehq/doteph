// The single source of truth for the deploy base path. GitHub Pages serves
// this project site under the repo name rather than at a domain root, so
// every internal link needs this prefix. astro.config.mjs feeds it straight
// to Astro's `base` option; rehype-doc-links.mjs (which runs as a plain
// remark/rehype plugin outside Vite, so it cannot read `import.meta.env`)
// imports it directly instead of hardcoding a second copy.
export const BASE_PATH = "/doteph";
