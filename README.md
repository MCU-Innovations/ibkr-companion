# IBKR Companion

A native, read-only re-entry watchlist built with Rust and Slint. All broker data comes directly from the **local IBKR Client Portal Gateway**. Quotes, positions, trades and alerts use IBKR. Company fundamentals (market cap, sector and industry) load asynchronously from Yahoo Finance by default, with optional FMP support. The app does not call the dashboard or Flex Web Service.

## Run

1. Start Client Portal Gateway and complete its browser login.
2. Install Rust 1.95.0 or newer and run `./scripts/bootstrap.ps1` once to fetch the pinned Slint source.
3. From this directory, run `cargo run --release --locked`.

## Automatic Windows builds

GitHub Actions runs tests and builds the release executable on every push and pull request, and can also be started from the Actions tab using **Run workflow**. Successful runs provide an `ibkr-companion-windows-x64` artifact containing the executable and documentation. No IBKR login or API credentials are needed for CI; the authenticated Gateway integration test stays ignored.

Slint is pinned in `slint-revision.txt` and checked out into the ignored `third_party/slint` directory. Both Rust dependencies and Material widgets come directly from this upstream checkout. No MCUi checkout or custom Slint changes are needed. To update Slint, change the revision and rerun the tests against that checkout.

The release executable attaches to the launching terminal on Windows when one is present, using the same early console attachment pattern as `ecuwb-rs`. Launching it from Explorer does not open a console. Console diagnostics show startup, snapshot results, WebSocket connections, subscriptions, and errors. Set `IBKR_COMPANION_LOG=debug` before launching to include individual Client Portal HTTP requests; use `off`, `error`, `warn`, or `info` to adjust verbosity (default: `info`).

The Options page includes a **Premiums Collected** list of recorded option sell executions, newest first, with contract, account, sale time, quantity, premium per share, and IBKR-reported proceeds. Sales are deduplicated by execution ID and retained in `option-premium-sales.json` alongside settings. Initial history is limited to the recent executions returned by Client Portal (up to seven days); the archive accumulates sales observed afterward. Both opening and closing sales are included; buybacks are not deducted, so this is not net strategy profit. Totals remain separate by currency, and rows with unavailable proceeds or unconfirmed currency do not contribute to totals.

The window can be resized or maximized. Recent exits appear in a compact table with **Best Buy** and share-weighted **VWAP Buy** immediately before the exit price, followed by the market price, move, and target. Either buy value turns yellow when the displayed market price is below it. Wider windows also show the exit time. The table starts sorted by **VS EXIT** ascending. Click a column header to sort and click it again to reverse the order. Rows without a value for that column stay at the end. The detail pane starts hidden; selecting a row opens it. Use **Hide details** in the pane or list header to close it, and **Show details** to reopen it.

Current holdings are always excluded. Select one date-range pill: **1 Week**, **1 Month**, **1 Year**, **Year to Date**, or **Inception**. The header reports the active period and visible count. Periods can contain the same rows when older history has not yet loaded.

The default Gateway URL is `https://localhost:5000/v1/api`. Optional environment variables: `IBKR_GATEWAY_URL`, `IBKR_VERIFY_SSL=true`, and `IBKR_ACCOUNT_IDS=U123,U456`. Only loopback Gateway URLs are accepted. The Gateway's default self-signed certificate is accepted for loopback when `IBKR_VERIFY_SSL` is unset or false.

If an HTTP request returns 401 or 403, the app checks the Gateway's brokerage authentication status and reports when browser login is needed. A WebSocket transport connection alone does not confirm brokerage authentication.

Client Portal HTTP requests include a `User-Agent` and `Accept: */*` as required by IBKR's request guidance. The authentication-status POST includes `Content-Length: 0`.

The app reads accounts, positions and the last seven days of executions through Client Portal HTTP, subscribes to `str` trade events and `smd` market-data events over its WebSocket, and requests Client Portal historical bars when a stock is selected. The selected stock shows 15-minute OHLC candlesticks from the latest available regular trading session, with green rising candles and red falling candles; the chart refreshes asynchronously. The details show the exit and current held quantity without account or contract identifiers. It reconnects and resubscribes automatically. Position snapshots are refreshed after trade events and periodically because Client Portal has no equivalent position change stream for this watchlist. All network and file I/O runs on Tokio workers; Slint only receives view updates.

The quote source follows US Eastern stock-market hours, including daylight saving time. During regular and extended hours it uses SMART quotes from the current session. During IBKR's overnight session it uses `@OVERNIGHT` quotes. A current-session last trade stays visible between trades; delayed data is labeled in the details. If no eligible session quote is available, the app shows the latest completed regular-session close from Client Portal daily bars. Until that lookup finishes, Client Portal's close fields provide a labeled fallback when available. Alerts still require a fresh real-time quote. Exchange holidays and early closes are reflected by the available Client Portal bars after the usual 16:00 Eastern cutoff; the app does not currently use a separate exchange calendar.

The WebSocket authenticates with the Gateway session cookie and waits for the authentication event before subscribing. Both text and binary JSON frames are supported. Trade history comes from HTTP; the trade stream subscribes to new executions. Position contract IDs can be strings or numbers, and invalid position records fail the refresh instead of being treated as zero holdings. If the uncached positions endpoint is unavailable, the app tries Client Portal's paginated positions endpoint.

Older purchase-history requests are queued at Client Portal's limit of one request per 15 minutes, with request timing and recovered history saved across restarts. Existing executions and saved histories are recalculated immediately when holdings refresh.

Only stock sales returned by Client Portal appear. The [trades endpoint](https://ibkrcampus.com/docs/web-api/v1/endpoints/order-monitoring/trades) covers the currently selected brokerage account for the current day and six previous days. The app archives executions it observes from Client Portal in the user's configuration directory, so re-entry targets can remain visible beyond that window. It asynchronously requests older buy and sell transactions from Client Portal's [PortfolioAnalyst transaction history](https://ibkrcampus.com/docs/web-api/v1/endpoints/portfolio-analyst/transaction-history) and saves those histories locally. An initial install cannot recover purchases outside the history Client Portal makes available; those costs remain unknown. The default view is **Inception**, with week, month, year, and year-to-date filters.

Exit price is share-weighted across the most recent consecutive sale fills for an account and contract. Purchase cost is shown only when a complete buy sample can be established from the available Client Portal executions and transaction history; otherwise it is left unknown. Quotes marked delayed, frozen, previous-close, or stale never trigger target alerts. Targets, pins, and alert settings are stored locally. This app never submits orders.

The streaming subscriptions use IBKR's [`str` trades](https://ibkrcampus.com/docs/web-api/v1/ws/order-position-operations/request-trades-data) and [`smd` market data](https://ibkrcampus.com/docs/web-api/v1/ws/market-data/market-data-request) topics. The selected-stock bars come from the [Client Portal historical market-data endpoint](https://ibkrcampus.com/docs/web-api/v1/endpoints/market-data/historical-market-data).

### History periods and lifetime purchase prices

The exit-date filter offers 1 Week, 1 Month, 1 Year, Year to Date, and Inception (default).
Best Buy is the lowest recorded purchase price; VWAP Buy is total purchase value divided
by total purchased shares across all available cycles. Neither is restricted by the exit filter.
Older Portfolio Analyst sales now participate in the exit list; one latest exit per stock is retained.
History requests cover all dates back to 1970, rather than stopping at one year, and run asynchronously
for contracts known from executions, cached history, and portfolio positions. Requests remain paced
at one per 15 minutes, in batches of up to 19 contracts per account. Missing
history for unheld stocks is prioritized. Unsupported lookbacks/errors are logged without
silently substituting a shorter window. Historical dates with no execution time display a date only.

Inception means all available cached/fetched history, not a guarantee of complete account history.
Client Portal may return incomplete history; previously sold contracts absent from both the current
portfolio and the saved execution archive cannot be discovered by this contract-based backfill.
The header reports loaded exits without an unconditional warning. Buy values should be considered provisional
until their underlying purchase history is complete.

History discovery limitation confirmed against the local Gateway: account-wide
`POST /pa/transactions` without conids returns HTTP 400 with
`acctIds, currency and conids are required`. Increasing days or switching date
filters cannot discover missing contract IDs. The UI reports loaded exits; Inception is not a guarantee of a complete account statement.

### Retaining historical stock identifiers

The app preserves Client Portal stock identifiers in `client-portal-contracts.json`
in its configuration directory. It can also load a migration file with that name
beside the executable. This installation includes 59 identifiers recovered from
saved Client Portal portfolio snapshots. These are lookup identifiers, not imported
trades or inferred sales. Historical transactions still come from Client Portal.
Missing histories for stocks no longer held are prioritized before routine refreshes.
The migration does not establish complete coverage back to August 17.

### Batched history recovery

The local Gateway was verified to return 63 transactions across 19 contracts in
one `/pa/transactions` request, including an August 17 purchase. Older documentation
saying only one contract per request does not match this Gateway. The app now batches
up to 19 stock IDs in a single request, while retaining the endpoint's pacing limit.
Returned purchases/sales are validated and grouped by account and contract before
merging into the local archive. Unknown symbols still require their contract IDs;
contract lookup for HIMS, LULU, NFLX and WDAY was performed through Client Portal.

A captured Client Portal batch can be recovered with
`ibkr-companion.exe --recover-client-portal-history <captured-json>` while the UI is
closed. This validates current Gateway holdings and verifies the requested stocks
appear in rendered exit rows before saving, and backs up the existing history.
This recovery accepts the app's captured Client Portal JSON, not CSV or another API.

### Portfolio sector heatmap

The portfolio heatmap groups held stocks by sector. The **Cell size** selector
switches between portfolio value in USD (default) and company market capitalization.
Multiple accounts holding the same stock share a single cell. Hover a cell to read
its symbol, classification, daily change, and position value. The layout adapts to
window size. Unknown values appear separately as equal-sized cells, labeled as
unavailable, rather than being assigned invented weights.

Yahoo Finance supplies market capitalization, sector, and industry without an API
key. Opening the portfolio queues the held stocks; a separate Tokio worker fetches
up to four profiles in parallel, deduplicates requests, and caches profiles for 24 hours in
`fundamentals-cache.json` beside the user's saved settings. Cached values remain
available while refreshes run. Reopening the portfolio, restarting the app, or
checking the daily cache does not refetch profiles less than 24 hours old.
All HTTP requests share a rolling rate limiter (at most four requests per second;
Yahoo: 120 per minute; FMP: 60 per minute and 250 per rolling 24 hours). Yahoo
has no published stable API quota, so these are conservative client limits,
with server throttling taking precedence. FMP usage and provider cooldowns are
persisted in `fundamentals-rate-limits.json`. Failed symbols retry after a delay,
with retry times persisted in the daily cache; HTTP 429 pauses
all profile requests, respecting Retry-After. Native Yahoo sessions renew their
cookie and crumb once when a session expires. Yahoo's endpoints are unofficial
and availability can change; failures are shown in the portfolio and settings.
Non-USD market caps use IBKR's exchange rates before participating in the treemap.

Open **Settings** from the navigation drawer to choose Yahoo Finance or Financial
Modeling Prep, enable/disable fetching, refresh cached data, and add, replace, or
remove an optional FMP API key. **Save settings** applies changes. Settings uses
the ECU Workbench full-page card layout above the current workspace; the title-bar
burger becomes Back, and Esc dismisses the same page even with a focused input.
Closing settings restores the original workspace with the navigation drawer open.
FMP credentials are sent in a sensitive request header and encrypted with Windows
DPAPI in `backends.dat`, scoped to the current Windows user. They are never logged
or returned in view snapshots. Unix builds store settings in a user-only file.
FMP endpoint access depends on the key's plan; Yahoo remains the free default.

The previous optional `IBKR_MARKET_CAP_FILE` JSON mapping is still supported as a
fallback for symbols without a provider market cap, using values in USD.
To check Yahoo independently of IBKR, run:
`ibkr-companion.exe --check-fundamentals AAPL`.

Data references: https://finance.yahoo.com/quote/AAPL/ and
https://site.financialmodelingprep.com/developer/docs/quickstart

The **Options** entry in the navigation drawer lists open IBKR option positions separately from stocks. Each contract shows its account, currency, multiplier, long/short quantity, expiry and days remaining, market value, average cost and P/L, bid/ask/last/mark, quote time and availability, Greeks, implied volatility, volume, and open interest when supplied by IBKR. **Share Cost** is the underlying shares' average purchase price per share from the latest IBKR position refresh, matched by account, underlying contract ID, and currency. It uses `avgPrice`, falling back to `avgCost` divided by the reported multiplier (one for stocks with no positive multiplier). It shows an em dash when no long underlying position or cost is available; it does not allocate particular share lots to options. Expirations within seven calendar days are highlighted. Missing fields display an em dash; option contract multipliers are never assumed. Options use regular market-data subscriptions, without overnight equity subscriptions.

The Options overview uses sortable compact rows for symbol, strike/type, expiry, days to expiry, signed position, mark and quote availability, unrealized P/L, return on absolute cost basis, and delta. Select a row to open the right-hand details panel; Close or Escape dismisses it. Selection follows the account/contract ID through sorting and quote updates. Totals stay separated by currency. Embedded OCC codes in IBKR descriptions supply missing expiry dates.

Options also show the underlying market price next to strike/type and an ITM/ATM/OTM column. Underlying contracts are resolved from IBKR contract metadata or streaming field 6457 and subscribed even when no stock position is held. Quote availability is displayed beneath the price. ATM means price and strike match at two decimal places; missing quotes produce no classification. Moneyness follows call/put direction regardless of whether the position is long or short.
