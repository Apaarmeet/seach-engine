/**
 * Guide state, held outside any page.
 *
 * This is the part that makes multi-page walkthroughs possible at all. A
 * content script dies on every navigation, so it cannot remember that the
 * user is on step 2 of 5. The service worker outlives navigation, so it owns
 * the step cursor and the content script stays stateless — it asks "what
 * should I show on this page?" each time it loads.
 */

const API = 'http://localhost:8091'

/** One guide per tab: different tabs are different tasks. */
const guides = new Map()

chrome.runtime.onMessage.addListener((msg, sender, respond) => {
  // Async responses require returning true synchronously.
  handle(msg, sender).then(respond).catch((e) => respond({ error: String(e) }))
  return true
})

async function handle(msg, sender) {
  const tabId = msg.tabId ?? sender.tab?.id
  if (tabId == null) return { error: 'no tab' }

  switch (msg.type) {
    case 'start':
      return start(tabId, msg.query)

    case 'whatNow': {
      // Asked by the content script on every page load.
      const g = guides.get(tabId)
      if (!g || g.index >= g.steps.length) return { step: null }
      const step = g.steps[g.index]
      // A step pinned to a particular page stays quiet elsewhere, so the
      // user isn't shown step 3 while still on the page for step 2.
      if (step.url_hint && !msg.url.includes(step.url_hint)) {
        return { step: null, waitingFor: step.url_hint }
      }
      return { step, index: g.index, total: g.steps.length }
    }

    case 'advance': {
      const g = guides.get(tabId)
      if (!g) return { done: true }
      g.index += 1
      const done = g.index >= g.steps.length
      if (done) guides.delete(tabId)
      return { done, index: g.index, total: g.steps.length }
    }

    case 'match': {
      // Proxied through the worker because the content script runs in the
      // page's origin, where a cross-origin POST to localhost would need the
      // page's own CORS blessing. The worker has host permissions instead.
      const res = await fetch(`${API}/match`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({
          find: msg.step.find,
          instruction: msg.step.instruction || '',
          controls: msg.controls || [],
        }),
      })
      if (!res.ok) return { match: null }
      return res.json()
    }

    case 'stop':
      guides.delete(tabId)
      return { stopped: true }

    default:
      return { error: 'unknown message' }
  }
}

async function start(tabId, query) {
  const res = await fetch(`${API}/guide?q=${encodeURIComponent(query)}`)
  if (!res.ok) throw new Error(`API returned ${res.status}`)
  const data = await res.json()

  if (!data.steps?.length) {
    return { steps: 0, reason: 'no walkthrough found for that request' }
  }

  guides.set(tabId, { steps: data.steps, index: 0, query })
  if (data.start_url) {
    await chrome.tabs.update(tabId, { url: data.start_url })
  }
  return { steps: data.steps.length, startUrl: data.start_url }
}

/**
 * A new tab inherits its opener's guide.
 *
 * Without this the walkthrough silently ends the first time a step opens in a
 * new tab — and steps constantly do. Cloudflare's docs send you to the
 * dashboard with `target="_blank"`, help centres open the real settings page
 * in a new tab, and "Go to Records ↗" is exactly that shape. The guide lived
 * on the old tab, the user was looking at the new one, and nothing
 * highlighted.
 *
 * The *same object* is shared rather than copied, deliberately: advancing a
 * step in either tab advances it in both, so a flow that hops to a new tab
 * and back does not desynchronise into two half-finished walkthroughs.
 */
chrome.tabs.onCreated.addListener((tab) => {
  const opener = tab.openerTabId
  if (opener != null && guides.has(opener)) {
    guides.set(tab.id, guides.get(opener))
  }
})

/**
 * Same-tab navigation needs no special handling — the content script reloads
 * and asks `whatNow` again — but a *replaced* tab (prerender, some redirects)
 * gets a new id and would otherwise lose its guide.
 */
chrome.tabs.onReplaced?.addListener((addedTabId, removedTabId) => {
  if (guides.has(removedTabId)) {
    guides.set(addedTabId, guides.get(removedTabId))
    guides.delete(removedTabId)
  }
})

/**
 * Dropping a guide when its tab closes keeps the map from growing forever.
 *
 * Safe with inheritance: tabs sharing a guide share one object, and deleting
 * one key leaves the others pointing at it.
 */
chrome.tabs.onRemoved.addListener((tabId) => guides.delete(tabId))
