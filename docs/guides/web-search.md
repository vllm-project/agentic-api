# Web search providers

The gateway-owned `web_search` tool runs against one configured backend; the model-facing tool schema is the same for
every provider.

| Setting | Environment variable | `config.toml` key | Default |
| :--- | :--- | :--- | :--- |
| Provider | `AGENTIC_WEB_SEARCH_PROVIDER` | `[web_search] provider` | `you` |
| API key | variable named by `api_key_env` | `[web_search] api_key_env` | `YOU_API_KEY` / `BRAVE_API_KEY` / `TAVILY_API_KEY` / `SERPLY_API_KEY` / `KEENABLE_API_KEY` (optional) / `SEARXNG_API_KEY` (optional) |
| Endpoint | `AGENTIC_WEB_SEARCH_BASE_URL` (or `YOU_API_BASE_URL` for You.com) | `[web_search] base_url` | none for You.com; `https://api.search.brave.com` for Brave; `https://api.tavily.com` for Tavily; `https://api.serply.io` for Serply; `https://api.keenable.ai` for Keenable; **required** for SearXNG |
| Concurrent queries | `AGENTIC_WEB_SEARCH_MAX_CONCURRENT_QUERIES` | `[web_search] max_concurrent_queries` | You.com, Tavily, Serply, Keenable and SearXNG inherit `max_concurrent_gateway_calls`; Brave `1` |

**You.com** (`provider = "you"`) is the default and behaves exactly as before: domain filters are applied by the
provider, `count` accepts 1–100, and the You.com-specific `livecrawl`, `livecrawl_formats`, `crawl_timeout`, and
`boost_domains` arguments are forwarded.

**Brave Search** (`provider = "brave"`) needs only `BRAVE_API_KEY`; its [free plan](https://brave.com/search/api/)
covers local and evaluation deployments. Web and news results come from one request per query. The gateway adapts
the shared tool contract to Brave:

- `allowed_domains` / `blocked_domains` (and the model's `include_domains` / `exclude_domains`) are enforced by the
  gateway after the response arrives, since Brave has no server-side domain filter. An allowlist is a hard contract,
  so a filtered search can return fewer than `count` results.
- `count` is clamped to Brave's maximum of 20; `freshness` is translated to Brave's `pd`/`pw`/`pm`/`py` codes or
  passed through as a date range; `language` maps to `search_lang`.
- The You.com-specific arguments above are ignored (logged at debug level).
- Each per-query `metadata[]` entry carries `"provider": "brave"` so the model can see which backend answered.
- Brave's free plan allows roughly one request per second, so batched queries run one at a time by default. Raise
  `max_concurrent_queries` on a paid plan. A rate-limited request (HTTP 429) fails that `web_search_call` without an
  automatic retry and reports the upstream `Retry-After` value; a cap lowers, but cannot eliminate, 429s on a
  per-second quota.

**Tavily** (`provider = "tavily"`) needs only `TAVILY_API_KEY`; its [plans](https://tavily.com) are metered in
credits, and each query costs one credit. Requests are `POST` bodies against `https://api.tavily.com/search` with the
key sent as a bearer token. The gateway adapts the shared tool contract to Tavily:

- `allowed_domains` / `blocked_domains` (and the model's `include_domains` / `exclude_domains`) are forwarded to
  Tavily's native `include_domains` / `exclude_domains` and re-checked by the gateway on the response as defense in
  depth, so a filtered search can return fewer than `count` results.
- `count` is clamped to Tavily's maximum of 20 (`max_results`); `freshness` maps to `time_range` (`day`, `week`,
  `month`, `year`) or to a `start_date` / `end_date` pair widened by one day on each side, since Tavily's bounds are
  exclusive and the gateway's range is inclusive; `language` keeps Tavily's documented compound tags (`zh-CN` →
  `zh-cn`) and otherwise reduces to its primary subtag (`en-GB` → `en`); `safesearch` becomes Tavily's boolean
  `safe_search` (anything but `off` enables it).
- A `freshness` filter is a hard contract: the date window is sent with `filter_by_published_date`, so Tavily drops
  results published outside it *and* results with no detectable publication date rather than letting them through.
  Like an allowlist, this can return fewer than `count` results.
- Every query is one `topic: "general"` search, so all hits land in `results.web` and `results.news` is always empty;
  a second news search per query would double the credits spent. `page_age` carries Tavily's `published_date`.
- `country` is ignored (Tavily expects full country names rather than ISO codes), as are the You.com-specific
  arguments above (all logged at debug level).
- Each per-query `metadata[]` entry carries `"provider": "tavily"` plus Tavily's `request_id` as `search_uuid` and
  its `response_time` as `latency`.
- Batched queries inherit the gateway concurrency limit. A rate-limited request (HTTP 429) fails that
  `web_search_call` without an automatic retry and reports the upstream `Retry-After` value; Tavily's plan-limit
  statuses (432, 433) fail the same way without a retry.

**Serply** (`provider = "serply"`) needs only `SERPLY_API_KEY` and returns Google results; its
[plans](https://serply.io) are metered per request. Requests are `GET`s against `https://api.serply.io/v1/search` with
the key sent in the `X-Api-Key` header. The gateway adapts the shared tool contract to Serply:

- `allowed_domains` / `blocked_domains` (and the model's `include_domains` / `exclude_domains`) are sent as `site:` /
  `-site:` query operators and re-checked by the gateway on the response as defense in depth, so a filtered search can
  return fewer than `count` results.
- `count` is clamped to Serply's maximum of 10 (`num`), and extra results are trimmed; `freshness` maps to Google's
  `tbs=qdr:d`/`w`/`m`/`y`, while a date range becomes `after:` / `before:` operators with the end widened by one day so
  both boundary dates stay in scope; `country` maps to `gl`; `safesearch: "strict"` becomes `safe=active`.
- `language` sets Google's interface language `hl` and, for the languages Google can restrict results to, the
  result-language restrict `lr` (`ja` becomes `lr=lang_ja`, `zh-TW` becomes `lr=lang_zh-TW`); any other language is
  sent as `hl` only.
- All hits land in `results.web` and `results.news` is always empty. Google redirect links
  (`https://www.google.com/url?q=...`) are unwrapped to the target URL, and `page_age` carries Serply's
  `published_time` when present.
- The You.com-specific arguments above are ignored (logged at debug level).
- Each per-query `metadata[]` entry carries `"provider": "serply"` plus Serply's `ts` as `latency`.
- Batched queries inherit the gateway concurrency limit. A rate-limited request (HTTP 429) fails that
  `web_search_call` without an automatic retry and reports the upstream `Retry-After` value.

**Keenable** (`provider = "keenable"`) works without an API key: requests are `POST`s against
`https://api.keenable.ai/v1/search/public` carrying an `X-Keenable-Title: vLLM Agentic API` header. With
`KEENABLE_API_KEY` set they go to `https://api.keenable.ai/v1/search` with the key in the `X-API-Key` header instead.
The gateway adapts the shared tool contract to Keenable:

- The body carries only `query` and `max_results`; `count` is clamped to Keenable's maximum of 20.
- Keenable has no server-side domain filter, so `allowed_domains` / `blocked_domains` (and the model's
  `include_domains` / `exclude_domains`) are applied by the gateway on the response, and a filtered search can return
  fewer than `count` results.
- `freshness`, `country`, `language`, `safesearch`, and the You.com-specific arguments are ignored (logged at debug
  level).
- All hits land in `results.web` and `results.news` is always empty. A result's description is Keenable's `snippet`,
  falling back to `description`, and `page_age` carries `published_at`.
- Each per-query `metadata[]` entry carries `"provider": "keenable"`. A 401/403 names `KEENABLE_API_KEY` and a 429 is
  reported with its `Retry-After` value, without an automatic retry.

If you switch an existing deployment to Brave, Tavily, or Serply, update `api_key_env` if an older `config.toml` pins
it (or remove it) and drop a You.com `base_url`; a mismatched key variable is reported in the failed `web_search_call`
message. Example:

```toml
[web_search]
provider = "tavily"
api_key_env = "TAVILY_API_KEY"
```

**SearXNG** (`provider = "searxng"`) runs against a [self-hosted SearXNG](https://docs.searxng.org/admin/installation.html)
instance: the gateway only ever sends an HTTP request to that instance, no API key is needed, and no search vendor
sees your deployment. Note that SearXNG itself forwards each query to the engines enabled in its `settings.yml`, so
for a fully air-gapped setup restrict it to internal or offline engines. This does **not** cover SearXNG's "bang"
syntax (`!!g rust`, `!ddg rust`, and its thousands of other pinned bangs): a bang makes SearXNG redirect straight to
the named external engine without consulting the enabled-engines list at all, before it even looks at
`format=json`. The gateway's SearXNG client disables HTTP redirects specifically so a bang in the query, whether written
by the model, requested by the user, or picked up from a page the model read, fails that `web_search_call` instead
of silently sending it past the configured instance. The endpoint is mandatory: the server refuses to start when
`searxng` is selected without `AGENTIC_WEB_SEARCH_BASE_URL` or `[web_search] base_url` (an absolute `http(s)` URL
without a query or fragment; a sub-path such as `http://host/searxng` is fine). Two instance settings matter:

- The JSON output format must be enabled: add `json` to `search.formats` in SearXNG's `settings.yml`
  (`formats: [html, json]`). Without it SearXNG answers `403`, which the failed `web_search_call` explains.
- If the instance runs with `server.limiter: true` (the default in the official `searxng-docker` template), its bot
  detection rejects requests that lack `Accept-Encoding: gzip`, which the gateway deliberately never sends. Add the
  gateway's address to `botdetection.ip_lists.pass_ip` in `limiter.toml`, or disable the limiter for an internal
  instance; otherwise every search fails with `429`.

The gateway adapts the shared tool contract to SearXNG:

- Web and news results come from one `categories=general,news` request per query, split by each hit's category.
- `allowed_domains` / `blocked_domains` (and the model's `include_domains` / `exclude_domains`) are enforced by the
  gateway after the response arrives; `count` is applied by the gateway after filtering, since SearXNG has no
  result-count parameter. Without `count` or `search_context_size` every hit the instance returned is passed on.
- `freshness` maps to `time_range=day|week|month|year`; a `YYYY-MM-DDtoYYYY-MM-DD` range has no SearXNG equivalent
  and is ignored. `language` is normalized to SearXNG's `xx` / `xx-YY` form (`zh-Hans` becomes `zh`); `safesearch`
  maps to `0` / `1` / `2`.
- `country` and the You.com-specific arguments are ignored (logged at debug level).
- Each per-query `metadata[]` entry carries `"provider": "searxng"`.
- Concurrency inherits `max_concurrent_gateway_calls`; lower `max_concurrent_queries` for a small instance. Rate
  limits (`429`) fail that `web_search_call` without an automatic retry.

If the instance sits behind an authenticating reverse proxy, set `SEARXNG_API_KEY` (or the variable named by
`api_key_env`) and the gateway sends it as a `Bearer` token. Example:

```toml
[web_search]
provider = "searxng"
base_url = "http://searxng.internal:8080"
```

General configuration, including `config.toml` and environment precedence, is described in
[Configuration](configuration.md).
