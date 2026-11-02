# beleaf engine experiment (Rust)

The current beleaf web app lives at the root of this repo and is untouched by this folder.
It is also tagged `web-v1`, so that version can always be restored.

- `brush-core/` — the brush engine: pen samples in, brush dabs out (spacing, pressure, stabiliser,
  direction-following tips, Clip Studio-style taper, sprayed dual tip). No dependencies.
- `poc/` — draws the dabs on the graphics card (wgpu). On Windows it reads the pen straight from
  Windows Ink; in the browser it uses WebGL2. The tips are the ones from "Textured Acrylic 01 2".
- `web/` — the page for the browser build.

Built by `.github/workflows/engine.yml`: the Windows .exe goes to the `engine-test` release and the
browser build to `/engine-test/` on the site.
