/**
 * Local ("near me") results.
 *
 * Rendered differently from web results on purpose. For a place, the things
 * that decide whether you go are distance, category, and whether it's open —
 * not a text snippet. Showing places in a web-result layout buries exactly
 * the fields the user is choosing on.
 */
export default function PlaceResults({ data, locationLabel }) {
  const results = data?.results ?? []
  const place = data?.resolved_place
  // When a place clause resolved, say where we searched. Otherwise the user
  // cannot tell a correct answer from a silently-relocated one.
  const where = place ? `${place.name} (${place.kind})` : locationLabel

  // Country/state-scale requests: say why the answer would be meaningless
  // rather than presenting arbitrary results from a geographic centroid.
  if (place?.scope_too_broad) {
    return (
      <p className="notice">
        <strong>“{place.name}” is too large to search by proximity.</strong>{' '}
        Local results are ranked by distance from a point, and the centre of{' '}
        {place.name} is unlikely to be near anything you want. Name a city or
        neighbourhood instead — try “{data.query.split(/ in /i)[0] || 'cafes'} in
        Bengaluru”.
      </p>
    )
  }

  if (!results.length) {
    return (
      <p className="empty">
        Nothing found nearby. Try a wider radius, or a broader term —
        “food” rather than a specific dish.
      </p>
    )
  }

  return (
    <>
      <p className="meta">
        <span>
          {results.length} place{results.length === 1 ? '' : 's'}{' '}
          {place ? 'in' : 'near'} <strong>{where}</strong> · within{' '}
          {(data.radius_m / 1000).toFixed(1)} km
          {data.requested_radius_m && (
            <span className="widened">
              {' '}(widened from {(data.requested_radius_m / 1000).toFixed(1)} km)
            </span>
          )}
          {' '}· {data.took_ms} ms
          {data.tiles_fetched > 0 && ` · fetched ${data.tiles_fetched} new tile(s)`}
        </span>
      </p>

      <ol className="places">
        {results.map((p) => (
          <li key={p.id}>
            <div className="place-head">
              <span className="place-name">{p.name}</span>
              <span className="place-distance">{formatDistance(p.distance_m)}</span>
            </div>
            <div className="place-meta">
              <span className="place-category">{p.category.replace(/_/g, ' ')}</span>
              {p.brand && p.brand.toLowerCase() !== p.name.toLowerCase() && (
                <span className="place-brand">{p.brand}</span>
              )}
              {p.opening_hours && <span className="place-hours">{p.opening_hours}</span>}
            </div>
            {p.address && <div className="place-address">{p.address}</div>}
            <div className="place-links">
              {p.phone && <a href={`tel:${p.phone}`}>{p.phone}</a>}
              {p.website && (
                <a href={p.website} target="_blank" rel="noreferrer noopener">
                  website
                </a>
              )}
              <a
                href={`https://www.openstreetmap.org/?mlat=${p.lat}&mlon=${p.lon}#map=18/${p.lat}/${p.lon}`}
                target="_blank"
                rel="noreferrer noopener"
              >
                map
              </a>
            </div>
          </li>
        ))}
      </ol>
    </>
  )
}

function formatDistance(m) {
  // Metres up close, kilometres further out — nobody thinks in "1430 m".
  if (m < 1000) return `${Math.round(m)} m`
  return `${(m / 1000).toFixed(1)} km`
}
