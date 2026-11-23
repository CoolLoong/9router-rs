import { extraSearchFixture, handleExtraSearchMock, isExtraSearchMock } from "./mock-web-search-extra.mjs";

const routes = new Map([
  ["https://google.serper.dev/search", "/serper/search"],
  ["https://google.serper.dev/news", "/serper/news"],
  ["https://api.search.brave.com/res/v1/web/search", "/brave/res/v1/web/search"],
  ["https://api.search.brave.com/res/v1/news/search", "/brave/res/v1/news/search"],
  ["https://api.exa.ai/search", "/exa/search"],
  ["https://api.exa.ai/contents", "/exa/contents"],
  ["https://api.tavily.com/search", "/tavily/search"],
  ["https://api.tavily.com/extract", "/tavily/extract"],
  ["https://api.firecrawl.dev/v1/scrape", "/firecrawl/v1/scrape"],
  ["https://r.jina.ai/", "/jina/"],
  ["https://ollama.com/api/web_fetch", "/ollama/api/web_fetch"],
  ["https://api.fetch.tinyfish.ai/", "/tinyfish/"],
]);
const paths = new Set(routes.values());
const target = "https://192.0.2.141/fixture?lang=zh";
const denied = (res, msg = "fixture request did not match") => json(res, 422, { error: msg });

export function webGatewayFixture(url) {
  const extra = extraSearchFixture(url);
  if (extra) return extra;
  const path = url && routes.get(url.origin + url.pathname);
  return path ? "http://127.0.0.1:52989" + path + url.search : null;
}

export function isWebGatewayMock(request) {
  return isExtraSearchMock(request) || paths.has(new URL(request.url, "http://127.0.0.1").pathname);
}

function json(response, status, body) {
  return response.writeHead(status, { "content-type": "application/json" }).end(JSON.stringify(body));
}

function matches(actual, expected) {
  return Object.keys(actual).length === Object.keys(expected).length && Object.entries(expected)
    .every(([key, value]) => JSON.stringify(actual[key]) === JSON.stringify(value));
}

export async function handleWebGatewayMock(request, response) {
  if (isExtraSearchMock(request)) return handleExtraSearchMock(request, response);
  const url = new URL(request.url, "http://127.0.0.1");
  const path = url.pathname;
  const provider = path.split("/")[1];
  const expectKey = `offline-web-${provider}`;
  const brave = provider === "brave";
  const apiKeyHeader = ["serper", "exa", "tinyfish"].includes(provider);
  const supplied = brave ? request.headers["x-subscription-token"] : apiKeyHeader
    ? request.headers["x-api-key"] : request.headers.authorization?.replace(/^Bearer /, "");
  const backup = provider === "exa" && supplied === "offline-web-exa-backup";
  if (supplied !== expectKey && !backup) return json(response, 401, { error: "Unexpected fixture credential" });
  if (request.method !== (brave ? "GET" : "POST")) return denied(response, "unexpected fixture method");

  if (brave) {
    if (request.headers.accept !== "application/json") return denied(response);
    const destination = path.includes("/news/") ? "news" : "web";
    if (url.searchParams.get("q") === "fixture query combo") {
      return json(response, 429, { error: "fixture quota reached" });
    }
    if (url.searchParams.get("q") !== "fixture query news" || url.searchParams.get("count") !== "20" ||
      url.searchParams.get("country") !== "US" || url.searchParams.get("search_lang") !== "en") return denied(response);
    return json(response, 200, { [destination]: { totalCount: 21, results: [{
      title: "Brave fixture", url: "https://www.example.invalid/brave?q=1", description: "News article",
      page_age: "2026-01-01", meta_url: { favicon: "https://example.invalid/brave.svg" },
    }] } });
  }
  if (request.headers["content-type"] !== "application/json") return denied(response);
  let raw = "";
  for await (const chunk of request) raw += chunk;
  let body;
  try { body = JSON.parse(raw); } catch { return denied(response); }
  if (path === "/serper/search") {
    if (body.q === "fixture query combo" && body.num === 5) {
      return json(response, 200, { organic: [{
        title: "Combo fallback fixture", link: "https://example.invalid/combo", snippet: "Fixture search",
      }] });
    }
    if (body.q === "redacted fixture") {
      return response.writeHead(422, { "content-type": "text/plain" }).end(`credential ${expectKey} and Bearer offline-web-synthetic`);
    }
    if (!matches(body, { q: "fixture query news", num: 1, gl: "us", hl: "en" })) return denied(response);
    return json(response, 200, { searchParameters: { totalResults: 17 }, organic: [
      { title: "Serper fixture", link: "https://www.example.invalid/page?foo=1", snippet: "Headline", date: "2026-01-02" },
      { title: "Serper secondary" },
    ] });
  }
  if (path === "/serper/news") {
    if (!matches(body, { q: "fixture news", num: 5 })) return denied(response);
    return json(response, 200, { news: [{ title: "No link", description: "Fallback excerpt" }] });
  }
  if (path === "/exa/search") {
    const filters = { includeDomains: ["example.invalid"], excludeDomains: ["excluded.invalid"] };
    if (body.query === "rotate fixture") {
      if (!matches(body, { query: "rotate fixture", numResults: 5, type: "auto", text: true, highlights: true })) return denied(response);
      if (!backup) return json(response, 429, { error: "fixture quota reached" });
      return json(response, 200, { results: [{ title: "Backup account" }] });
    }
    if (!matches(body, { query: "fixture exa", numResults: 1, type: "auto", text: true, highlights: true, ...filters, category: "news" })) return denied(response);
    return json(response, 200, { results: [
      { title: "Exa fixture", url: "http://example.invalid/exa", highlights: ["First highlight"], text: "Full 🙂 text",
        score: 1.5, publishedDate: "2026-01-03", favicon: "https://example.invalid/exa.svg", image: "https://example.invalid/exa.png", author: "Tester" },
      { title: "Capped result" },
    ] });
  }
  if (path === "/tavily/search") {
    if (!matches(body, { query: "fixture tavily", max_results: 5, topic: "general", country: "taiwan" })) return denied(response);
    return json(response, 200, { results: [{ title: "Tavily fixture", url: "http://example.invalid/tavily", content: "Excerpt", score: -1, published_date: "2026-01-04", raw_content: "Full Tavily text" }] });
  }
  if (path === "/exa/contents") {
    if (backup || !matches(body, { ids: [target], text: true })) return denied(response, "fetch used an unexpected account or body");
    return json(response, 200, { results: [{ title: "Exa page", text: "Document with Unicode 🙂" }] });
  }
  if (path === "/tavily/extract") {
    if (!matches(body, { urls: [target], extract_depth: "basic", format: "text" })) return denied(response);
    return json(response, 200, { results: [{ raw_content: "Tavily extracted text" }] });
  }
  if (path === "/firecrawl/v1/scrape") {
    if (!matches(body, { url: target, formats: ["html"] })) return denied(response);
    return json(response, 200, { data: { html: "<p>Fixture page extract</p>", metadata: { title: "Firecrawl page" } } });
  }
  if (path === "/jina/") {
    if (!matches(body, { url: target }) || request.headers["x-respond-with"] !== "markdown") return denied(response);
    return response.writeHead(200, { "content-type": "text/plain" }).end("# Heading fallback\nTitle: Fixture Reader\n\nReader Content");
  }
  if (path === "/ollama/api/web_fetch") {
    if (!matches(body, { url: target })) return denied(response);
    return json(response, 200, { title: "Ollama page", content: "Ollama fixture", links: ["https://example.invalid/link"] });
  }
  if (path === "/tinyfish/") {
    if (!matches(body, { urls: [target], format: "html" })) return denied(response);
    return json(response, 200, { results: [{ title: "TinyFish page", text: "<p>TinyFish text</p>", links: ["https://example.invalid/link"],
      author: "Fixture author", published_date: "2026-01-05", language: "en" }] });
  }
  return denied(response);
}
