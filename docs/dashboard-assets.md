# Dashboard browser assets

The Rust gateway embeds the browser assets from the 9Router 0.5.119 dashboard. This includes the stylesheet, browser modules, icons, language catalogs, and page payloads. Upstream source and its license are available in the repository history. [Upstream baseline](https://github.com/decolua/9router/tree/1855aa44473e88feac9d3f131b9c0c5fc9706b68)

The archive is generated from a local instance of that version with a temporary database and fixture machine ID. Its page entries contain both the HTML document and the React route response. The gateway inserts the current machine ID and route segment when serving a page, checks the dashboard session, and handles API requests with the Rust backend.

To refresh the bundle, collect the dashboard pages and their responses to requests with the `RSC: 1` header from the reference instance. Store them at `page/<route>.html` and `page/<route>.rsc`. Store the matching build's static assets, provider icons, and language dictionaries under `asset/` with their request paths. Generate the bundle with temporary settings and replace build-time machine identifiers with `__RUST_DASHBOARD_CLIENT_ID__` before embedding it.
