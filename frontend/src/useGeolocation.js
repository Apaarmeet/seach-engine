import { useCallback, useEffect, useState } from 'react'

/**
 * Browser geolocation with a graceful fallback.
 *
 * Three states matter and are handled separately, because collapsing them
 * makes the UI lie:
 *   - `prompt`  — we haven't asked yet; show results from the fallback and
 *                 offer to get precise ones.
 *   - `granted` — real coordinates.
 *   - `denied`  — the user said no. Keep working from the fallback and stop
 *                 nagging; re-prompting after a denial does nothing in most
 *                 browsers anyway.
 *
 * The fallback exists so the page is useful before any permission decision.
 * A local-search UI that shows nothing until you grant location feels broken,
 * and users decline permissions they don't yet see a reason for.
 *
 * Note geolocation only works on HTTPS or localhost.
 */
const FALLBACK = { lat: 12.9716, lon: 77.5946, label: 'Bengaluru (default)' }

export function useGeolocation() {
  const [coords, setCoords] = useState(FALLBACK)
  const [status, setStatus] = useState('prompt')
  const [error, setError] = useState(null)

  const request = useCallback(() => {
    if (!('geolocation' in navigator)) {
      setStatus('unsupported')
      setError('This browser has no geolocation API.')
      return
    }
    setStatus('locating')
    navigator.geolocation.getCurrentPosition(
      (pos) => {
        setCoords({
          lat: pos.coords.latitude,
          lon: pos.coords.longitude,
          label: `your location (±${Math.round(pos.coords.accuracy)} m)`,
        })
        setStatus('granted')
        setError(null)
      },
      (err) => {
        setStatus(err.code === err.PERMISSION_DENIED ? 'denied' : 'error')
        setError(err.message)
        // Deliberately keep the fallback coords so search still works.
      },
      { enableHighAccuracy: true, timeout: 10000, maximumAge: 300000 },
    )
  }, [])

  // If permission was granted in a previous visit, use it without prompting.
  useEffect(() => {
    if (!navigator.permissions?.query) return
    navigator.permissions
      .query({ name: 'geolocation' })
      .then((p) => {
        if (p.state === 'granted') request()
        else if (p.state === 'denied') setStatus('denied')
      })
      .catch(() => {})
  }, [request])

  return { coords, status, error, request, isFallback: status !== 'granted' }
}
