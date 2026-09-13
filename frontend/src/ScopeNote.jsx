import { useEffect, useState } from 'react'

/**
 * States plainly what this index does and does not contain.
 *
 * This is here on purpose, not as an apology. The local index is genuinely
 * complete for India; the web index is a small demo corpus. A visitor who
 * types "python pandas" and gets nothing will conclude the whole thing is
 * broken — unless the scope was stated up front, in which case they judge it
 * on what it claims. Stating limits costs one line and buys the benefit of
 * the doubt on everything else.
 */
export default function ScopeNote({ mode }) {
  const [stats, setStats] = useState(null)

  useEffect(() => {
    Promise.all([
      fetch('/stats').then((r) => r.json()).catch(() => ({})),
      fetch('/places-stats').then((r) => r.json()).catch(() => ({})),
    ])
      .then(([web, places]) => setStats({ ...web, ...places }))
      .catch(() => {})
  }, [])

  if (!stats) return null

  const fmt = (n) => (n ?? 0).toLocaleString()

  return (
    <div className="scope-note">
      {mode === 'local' ? (
        <p>
          <strong>Local index — all of India.</strong>{' '}
          {fmt(stats.places)} places and {fmt(stats.place_names)} place names
          from OpenStreetMap, covering every city, town and village. Coverage
          reflects what OSM contributors have mapped, so it is denser in
          metros than in smaller towns.
        </p>
      ) : (
        <p>
          <strong>Web index — {fmt(stats.web_documents)} pages.</strong> A demo
          corpus (Wikipedia-India plus a Common Crawl <code>.in</code> slice),
          not the open web. General queries like “python pandas” will miss.
          The <strong>local</strong> side is the complete one — try{' '}
          “hospitals in Chennai”.
        </p>
      )}
    </div>
  )
}
