import { useCallback, useEffect, useRef, useState } from 'react'
import PlaceResults from './PlaceResults.jsx'
import ScopeNote from './ScopeNote.jsx'
import { useGeolocation } from './useGeolocation.js'

/**
 * One search box, two indexes.
 *
 * Queries carrying locality markers ("near me", "nearest") route to the
 * places index; everything else goes to the web index. That routing is the
 * whole point — a page has no coordinates and a restaurant has no inbound
 * links, so one ranking function cannot serve both well.
 *
 * Snippets arrive as HTML (matched terms wrapped in <b>) and go through
 * dangerouslySetInnerHTML. Safe only because tantivy HTML-escapes page text
 * before inserting its own tags — re-verify if snippet generation changes.
 */

/**
 * One-tap categories offered once a location is known.
 *
 * These exist because a location-aware search box with an empty input is a
 * dead end — the user has just granted location and has nothing to act on.
 * Showing what's actually searchable nearby turns permission-granting into
 * an immediate payoff rather than a prompt they regret.
 */
const NEARBY_CATEGORIES = [
  { icon: '☕', label: 'Cafes', q: 'cafes near me' },
  { icon: '🍽️', label: 'Restaurants', q: 'restaurants near me' },
  { icon: '💊', label: 'Pharmacy', q: 'pharmacy near me' },
  { icon: '🏥', label: 'Hospitals', q: 'hospitals near me' },
  { icon: '🏧', label: 'ATMs', q: 'atm near me' },
  { icon: '⛽', label: 'Petrol', q: 'petrol pump near me' },
  { icon: '🛒', label: 'Grocery', q: 'grocery near me' },
  { icon: '🏨', label: 'Hotels', q: 'hotels near me' },
  { icon: '🏋️', label: 'Gyms', q: 'gym near me' },
  { icon: '🅿️', label: 'Parking', q: 'parking near me' },
]

const LOCAL_MARKERS = [
  'near me', 'nearby', 'near by', 'around me', 'close to me',
  'closest', 'nearest', 'near here', 'in my area', 'around here',
]

/**
 * Local intent comes in two shapes:
 *   - proximity: "cafes near me"
 *   - place-scoped: "starbucks in Delhi"
 *
 * The first is decidable here from the marker words. The second is not, and
 * trying to decide it client-side was a bug: the original version gated on a
 * hardcoded list of subject words (cafe, hospital, atm...), so any brand name
 * fell through — "starbucks in delhi" went to the web index and returned
 * nothing useful, even though the places index answers it perfectly.
 *
 * A word list can never cover every business name. The server holds the
 * gazetteer, so it is the only thing that can actually answer "is this a
 * place?" — we ask it, and fall back to web search when it says no.
 */
function proximityIntent(q) {
  const lower = q.toLowerCase()
  return LOCAL_MARKERS.some((m) => lower.includes(m))
}

function hasPlaceClause(q) {
  return / in .+/i.test(q.trim())
}

export default function App() {
  const [query, setQuery] = useState('')
  const [webData, setWebData] = useState(null)
  const [placeData, setPlaceData] = useState(null)
  const [mode, setMode] = useState('web')
  const [loading, setLoading] = useState(false)
  const [error, setError] = useState(null)
  const [explain, setExplain] = useState(false)
  const [radiusKm, setRadiusKm] = useState(2)
  const abortRef = useRef(null)

  const { coords, status, request, isFallback } = useGeolocation()

  const run = useCallback(
    async (q, wantExplain, lat, lon, radius) => {
      if (!q.trim()) {
        setWebData(null)
        setPlaceData(null)
        setError(null)
        setMode('web')
        return
      }

      abortRef.current?.abort()
      const controller = new AbortController()
      abortRef.current = controller
      setLoading(true)
      setError(null)

      const nearbyUrl = () =>
        `/nearby?q=${encodeURIComponent(q)}&lat=${lat}&lon=${lon}` +
        `&radius=${Math.round(radius * 1000)}&limit=20`
      const webUrl = () =>
        `/search?q=${encodeURIComponent(q)}&limit=20&explain=${wantExplain}`

      const get = async (url) => {
        const res = await fetch(url, { signal: controller.signal })
        if (!res.ok) throw new Error(`API returned ${res.status}`)
        return res.json()
      }

      try {
        if (proximityIntent(q) || hasPlaceClause(q)) {
          // Explicit local signal: places is the answer, not a sidebar.
          const data = await get(nearbyUrl())
          if (data.resolved_place || proximityIntent(q)) {
            setPlaceData(data)
            setWebData(null)
            setMode('local')
            return
          }
          // " in " that names no known place — fall through to web.
          setWebData(await get(webUrl()))
          setPlaceData(null)
          setMode('web')
          return
        }

        // No routing signal. "IIM Bangalore" and "monsoon agriculture" are
        // indistinguishable from the text alone, so ask both indexes and let
        // the results decide — a local pack above web results, which is how
        // a general search engine presents this. Guessing one or the other
        // sent "IIM Bangalore" to the web index and returned a JPEG.
        const [places, web] = await Promise.all([
          get(nearbyUrl()).catch(() => null),
          get(webUrl()).catch(() => null),
        ])
        setPlaceData(places?.results?.length ? places : null)
        setWebData(web)
        setMode(places?.results?.length ? 'both' : 'web')
      } catch (e) {
        if (e.name !== 'AbortError') {
          setError(e.message)
          setWebData(null)
          setPlaceData(null)
        }
      } finally {
        if (abortRef.current === controller) setLoading(false)
      }
    },
    [],
  )

  useEffect(() => {
    const t = setTimeout(
      () => run(query, explain, coords.lat, coords.lon, radiusKm),
      180,
    )
    return () => clearTimeout(t)
  }, [query, explain, radiusKm, coords.lat, coords.lon, run])

  const hasResults = webData !== null || placeData !== null
  const resolvedPlace = placeData?.resolved_place ?? null

  return (
    <div className="page">
      <header className={hasResults ? 'compact' : 'hero'}>
        <h1 className="logo">rsearch</h1>

        <div className="searchbar">
          <svg className="icon" viewBox="0 0 24 24" aria-hidden="true">
            <circle cx="11" cy="11" r="7" />
            <line x1="16.5" y1="16.5" x2="21" y2="21" />
          </svg>
          <input
            autoFocus
            type="search"
            value={query}
            placeholder="Try “cafe near me” or “hospitals in Jaipur”…"
            onChange={(e) => setQuery(e.target.value)}
            aria-label="Search query"
          />
          {mode === 'local' && <span className="mode-badge">local</span>}
        </div>

        {!hasResults && !error && (
          <p className="hint">
            Web:{' '}
            <button onClick={() => setQuery('indian railways')}>indian railways</button>,{' '}
            <button onClick={() => setQuery('monsoon agriculture')}>monsoon agriculture</button>
            <br />
            Local:{' '}
            <button onClick={() => setQuery('starbucks near me')}>starbucks near me</button>,{' '}
            <button onClick={() => setQuery('pharmacy near me')}>pharmacy near me</button>,{' '}
            <button onClick={() => setQuery('nearest atm')}>nearest atm</button>,{' '}
            <button onClick={() => setQuery('hospitals in jaipur')}>hospitals in jaipur</button>
          </p>
        )}

        {!hasResults && <ScopeNote mode={mode} />}

        {/* Once a real location is known, offer something to do with it. */}
        {status === 'granted' && !hasResults && (
          <div className="category-grid">
            {NEARBY_CATEGORIES.map((c) => (
              <button key={c.label} className="category-chip" onClick={() => setQuery(c.q)}>
                <span className="category-icon">{c.icon}</span>
                {c.label}
              </button>
            ))}
          </div>
        )}

        {mode === 'local' && (
          <div className="local-controls">
            {/* A resolved place clause overrides both the coordinates and
                the radius server-side. Showing the user's own location and
                a stale slider here would contradict the result header. */}
            {resolvedPlace ? (
              <span className="location-chip">
                🗺️ searching <strong>{resolvedPlace.name}</strong>{' '}
                <span className="muted">
                  ({resolvedPlace.kind}, {(resolvedPlace.radius_m / 1000).toFixed(0)} km)
                </span>
              </span>
            ) : (
              <>
                <span className="location-chip">
                  📍 {coords.label}
                  {isFallback && status !== 'locating' && (
                    <button className="link-btn" onClick={request}>
                      {status === 'denied' ? 'permission denied' : 'use my location'}
                    </button>
                  )}
                  {status === 'locating' && <span className="muted"> locating…</span>}
                </span>
                <label className="radius">
                  radius
                  <input
                    type="range"
                    min="0.5"
                    max="10"
                    step="0.5"
                    value={radiusKm}
                    onChange={(e) => setRadiusKm(Number(e.target.value))}
                  />
                  {radiusKm} km
                </label>
              </>
            )}
          </div>
        )}
      </header>

      <main>
        {error && <p className="error">Couldn’t reach the API: {error}</p>}
        {loading && !hasResults && <p className="meta">Searching…</p>}

        {placeData?.unresolved_place && (
          <p className="notice">
            Couldn’t find a place called{' '}
            <strong>“{placeData.unresolved_place}”</strong> — showing results
            near {coords.label} instead.
          </p>
        )}

        {mode === 'both' && placeData && (
          <section className="local-pack">
            <h2>Places</h2>
            <PlaceResults data={placeData} locationLabel={coords.label} />
          </section>
        )}

        {mode === 'local' && placeData && (
          <>
            <div className="category-grid inline">
              {NEARBY_CATEGORIES.map((c) => (
                <button
                  key={c.label}
                  className={`category-chip${query === c.q ? ' active' : ''}`}
                  onClick={() => setQuery(c.q)}
                >
                  <span className="category-icon">{c.icon}</span>
                  {c.label}
                </button>
              ))}
            </div>
            <PlaceResults data={placeData} locationLabel={coords.label} />
          </>
        )}

        {webData && (
          <>
            <div className="meta">
              <span>
                {webData.total_hits.toLocaleString()} matching{' '}
                {webData.total_hits === 1 ? 'page' : 'pages'} · showing{' '}
                {webData.results.length} · {webData.took_ms} ms
              </span>
              <label className="explain-toggle">
                <input
                  type="checkbox"
                  checked={explain}
                  onChange={(e) => setExplain(e.target.checked)}
                />
                explain ranking
              </label>
            </div>

            {webData.results.length === 0 && (
              <p className="empty">
                No match in the web demo corpus (5,512 pages). This index is a
                small sample, not the open web — the <strong>local</strong>{' '}
                side is the complete one. Try “hospitals in Chennai” or
                “cafes near me”.
              </p>
            )}

            <ol className="results">
              {webData.results.map((r) => (
                <li key={r.url}>
                  <a className="result-url" href={r.url} target="_blank" rel="noreferrer noopener">
                    {prettyUrl(r.url)}
                  </a>
                  <a className="result-title" href={r.url} target="_blank" rel="noreferrer noopener">
                    {r.title || r.url}
                  </a>
                  <p
                    className="result-snippet"
                    dangerouslySetInnerHTML={{ __html: r.snippet || '' }}
                  />
                  {r.explain ? (
                    <ScoreBreakdown score={r.score} explain={r.explain} />
                  ) : (
                    <span className="result-score">score {r.score.toFixed(2)}</span>
                  )}
                </li>
              ))}
            </ol>
          </>
        )}
      </main>
    </div>
  )
}

function ScoreBreakdown({ score, explain }) {
  const parts = [
    { label: 'text (BM25F)', value: explain.text_bm25, cls: 'bm25' },
    { label: 'authority', value: explain.pagerank_boost, cls: 'pagerank' },
    { label: 'quality', value: explain.quality_boost, cls: 'quality' },
    { label: 'domain trust', value: explain.domain_trust_boost, cls: 'trust' },
  ].filter((p) => p.value > 0.001)
  const total = parts.reduce((s, p) => s + p.value, 0) || 1

  return (
    <div className="breakdown">
      <div className="bar" role="img" aria-label="score composition">
        {parts.map((p) => (
          <span
            key={p.label}
            className={`seg ${p.cls}`}
            style={{ width: `${(p.value / total) * 100}%` }}
            title={`${p.label}: ${p.value.toFixed(2)}`}
          />
        ))}
      </div>
      <div className="legend">
        <strong>{score.toFixed(2)}</strong>
        {parts.map((p) => (
          <span key={p.label} className="legend-item">
            <i className={`dot ${p.cls}`} />
            {p.label} {p.value.toFixed(2)}
          </span>
        ))}
      </div>
    </div>
  )
}

function prettyUrl(url) {
  try {
    const u = new URL(url)
    const path = u.pathname === '/' ? '' : u.pathname.replace(/\/$/, '')
    return u.hostname + decodeURIComponent(path).replaceAll('/', ' › ')
  } catch {
    return url
  }
}
