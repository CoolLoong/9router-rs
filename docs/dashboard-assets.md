# Dashboard source and build

The React pages, components, styles, and translations are in `dashboard/`. They are based on the original 9Router dashboard. [Upstream baseline](https://github.com/decolua/9router/tree/1855aa44473e88feac9d3f131b9c0c5fc9706b68)

```sh
npm --prefix dashboard ci
npm --prefix dashboard run build
cargo check --locked --workspace
```

The dashboard build uses Node.js 22 and writes browser assets to `dashboard/dist/`. Cargo embeds those assets in the gateway executable. The deployed service serves them directly.

The original page components use a browser router in `dashboard/src/standalone/`. The Rust service checks the dashboard session and supplies the machine ID at `GET /api/dashboard/context`. Forms and dashboard views call the existing same-origin Rust API. Provider data in the browser describes display options and models. Provider credentials and request transport are handled by the gateway.
