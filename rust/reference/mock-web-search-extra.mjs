const routes = new Map([
  ["https://ydc-index.io/v1/search", "/youcom/v1/search"],
  ["https://www.googleapis.com/customsearch/v1", "/google-pse/customsearch/v1"],
  ["https://www.searchapi.io/api/v1/search", "/searchapi/api/v1/search"],
]);
const paths = new Set(routes.values());

export function extraSearchFixture(url) {
  const path = url && routes.get(url.origin + url.pathname);
  return path ? "http://127.0.0.1:52989" + path + url.search : null;
}

export function isExtraSearchMock(request) {
  return paths.has(new URL(request.url, "http://127.0.0.1").pathname);
}

function json(response, status, body) {
  return response.writeHead(status, { "content-type": "application/json" }).end(JSON.stringify(body));
}

function match(params, expected) {
  const actual = [...params.entries()];
  return JSON.stringify(actual) === JSON.stringify(Object.entries(expected));
}

export function handleExtraSearchMock(request, response) {
  const url = new URL(request.url, "http://127.0.0.1");
  const params = url.searchParams;
  const provider = url.pathname.split("/")[1];
  const expectedKey = `offline-web-${provider}`;
  const key = provider === "youcom" ? request.headers["x-api-key"]
    : params.get(provider === "google-pse" ? "key" : "api_key");
  if (key !== expectedKey || request.headers.authorization || request.headers.accept !== "application/json" || request.method !== "GET") {
    return json(response, 401, { error: "Unexpected search fixture credentials or request" });
  }
  if (params.get("query") === "redirect fixture") {
    return response.writeHead(302, { Location: "http://127.0.0.1:52982/capture" }).end();
  }
  if (params.get("query") === "same-origin fixture") {
    return response.writeHead(302, { Location: "/youcom/v1/search?query=fixture+you+empty&count=5" }).end();
  }
  if (params.get("query") === "credential fixture") {
    return json(response, 422, { error: `Fixture echoed ${expectedKey}` });
  }
  if (provider === "youcom") {
    if (match(params, {
      query: "fixture you news", count: "2", freshness: "week", offset: "9",
      country: "US", language: "en", include_domains: "news.example.invalid,example.invalid",
      exclude_domains: "blocked.example.invalid",
      livecrawl: "news", livecrawl_formats: "markdown",
    })) {
      return json(response, 200, { results: { news: [
        {
          title: "You.com fixture", url: "https://www.example.invalid/you?q=1", snippets: [false, "", "alternative"],
          description: "not selected", page_age: "2026-02-10",
          favicon_url: "https://example.invalid/favicon.svg", thumbnail_url: "https://example.invalid/t.png",
          markdown: "Fixture 🌿", html: "<p>not selected</p>",
        },
        { title: "Live HTML", url: "https://example.invalid/html", description: "HTML excerpt", html: "<p>Fixture</p>" },
        { title: "Outside request limit" },
      ], web: [{ title: "wrong branch" }] } });
    }
    if (match(params, { query: "fixture you web", count: "5", livecrawl: "web", livecrawl_formats: "html" })) {
      return json(response, 200, { results: { web: [{
        title: "No text for empty markdown", snippets: [null], description: "Web excerpt",
        markdown: "", html: "<p>ignored when markdown is a string</p>",
      }] } });
    }
    if (match(params, {query:"fixture you empty",count:"5"})) {
      return json(response, 200, {});
    }
  }
  if (provider === "google-pse") {
    if (match(params, {
      key: expectedKey, cx: "explicit-cx", q: "fixture google offset", num: "1",
      gl: "us", hl: "en", dateRestrict: "m1", start: "91",
    })) {
      return json(response, 200, { items: [
        { title: "Google fixture", link: "https://www.example.invalid/pse?result=1", snippet: "One",
          pagemap: { cse_thumbnail: [{ src: "https://example.invalid/thumbnail.png" }], metatags: [{ "og:image": "ignored" }] },
        },
        { title: "Outside request limit" },
      ], searchInformation: { totalResults: "0x10" } });
    }
    if (match(params, {
      key: expectedKey, cx: "stored-cx", q: "fixture google stored", num: "10", start: "4.5",
    })) {
      return json(response, 200, { items: [
        { title: "Fallback image", link: "https://example.invalid/google", pagemap: { metatags: [{ "og:image": "https://example.invalid/meta.png" }] } },
      ], queries: { request: [{ totalResults: "200" }] } });
    }
  }
  if (provider === "searchapi") {
    if (match(params, {
      engine: "google_news", q: "fixture SearchAPI", api_key: expectedKey, gl: "us", hl: "en", page: "3",
    })) {
      return json(response, 200, {
        top_stories: [
          { title: "SearchAPI story", link: "https://example.invalid/news", description: "Story excerpt",
            published_at: "2026-02-10", favicon: "https://example.invalid/favicon.svg",
            source: "Fixture desk", thumbnail: "https://example.invalid/news.png",
          },
          { title: "Second item" },
        ],
        search_information: { total_results: "1,234" },
      });
    }
    if (match(params, {engine:"google",q:"fixture SearchAPI empty",api_key:expectedKey})) {
      return json(response, 200, { organic_results: [], top_stories: [{ title: "unused" }] });
    }
  }
  return json(response, 422, { error: "Request did not match extra search fixture" });
}
