// Runs each inline script of the web UI Review page to the end of its top-level load,
// in one vm context whose DOM is a stub that accepts any access. Exits 1 with the thrown
// error when a load throws. `node --check` only parses, so it passes a script that throws
// on load; this runs it. The stub is a load check, not a sandbox: the page is repo content.
//
// Usage: node tools/review-page-check.js <path to review.html>

const fs = require('fs');
const vm = require('vm');

const file = process.argv[2];
if (!file) {
  console.log('usage: node tools/review-page-check.js <path to review.html>');
  process.exit(2);
}
const html = fs.readFileSync(file, 'utf8');
const blocks = [...html.matchAll(/<script(\s[^>]*)?>([\s\S]*?)<\/script>/g)]
  .filter((m) => !/\ssrc=/.test(m[1] || ''))
  .map((m) => m[2]);
if (blocks.length === 0) {
  console.log('no inline <script> block found');
  process.exit(1);
}

const stub = () => new Proxy(function () {}, {
  get: (t, k) => (k === Symbol.toPrimitive ? () => '' : k === 'then' ? undefined : stub()),
  apply: () => stub(),
  construct: () => stub(),
  set: () => true,
});
const ctx = {
  document: stub(), window: stub(), console,
  fetch: () => new Promise(() => {}),
  location: { search: '?channel=x' },
  URLSearchParams, setTimeout: () => 0, addEventListener: () => {},
};
ctx.globalThis = ctx;

const context = vm.createContext(ctx);
blocks.forEach((src, i) => {
  try {
    vm.runInContext(src, context, { filename: `review.html script ${i + 1}` });
  } catch (e) {
    console.log(`review page script ${i + 1} throws on load: ${e.name}: ${e.message}`);
    process.exit(1);
  }
});
console.log(`review page script loads without error (${blocks.length} inline block${blocks.length === 1 ? '' : 's'})`);
