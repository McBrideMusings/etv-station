# Vendored fonts

Bundled fallback fonts so `OverlayKind::Text` renders inside slim Linux deploy
containers that ship without a system font stack. Registered into the renderer's
`FontContext` at `VelloRenderer::new`.

## Inter-Regular.ttf

- Source: [fontsource/inter](https://fontsource.org/fonts/inter) Latin subset
  (`https://cdn.jsdelivr.net/fontsource/fonts/inter@latest/latin-400-normal.ttf`).
- Upstream: [github.com/rsms/inter](https://github.com/rsms/inter)
- License: SIL Open Font License, Version 1.1 — see `LICENSE.OFL.txt`.
- Family name as registered: `Inter`.

The Latin-only subset keeps the binary at ~68 KB. If non-Latin overlay text is
ever needed, swap to the full Inter (or another font that covers the scripts in
use) and update this README + `Cargo.toml`'s `include` list.
