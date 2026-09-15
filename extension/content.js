/**
 * Spotlighting a control on someone else's page.
 *
 * Two rules shape everything here.
 *
 * **Find by text, never by CSS selector.** A selector like `.yt-btn-3f2a`
 * breaks the week the site ships a redesign; "Cancel membership" survives it.
 * Text is also the only thing that can be matched reliably from outside,
 * since the backend reads served markup while this runs against the *rendered*
 * DOM — the mismatch that made URL-based highlighting fail on JS-heavy sites
 * disappears here, because this code sees what the user sees.
 *
 * **Never touch the page's own DOM.** The overlay lives in a shadow root
 * attached to a single appended element. Injecting styles or wrapper nodes
 * into a stranger's markup breaks their layout, and a tool that visibly
 * damages the page it is helping you use has failed.
 *
 * It highlights. It does not click. The tasks people want help with are
 * cancelling subscriptions and editing DNS records, and an automation that
 * misfires there has cancelled someone's service.
 *
 * And it marks, rather than spotlights: no dimming, no callout box. The page
 * belongs to the person reading it.
 */

;(() => {
  const HOST_ID = '__rsearch_guide_host'
  if (document.getElementById(HOST_ID)) return

  let overlay = null

  /** Visible text of an element, whitespace-normalised. */
  const textOf = (el) => (el.innerText || el.textContent || '').replace(/\s+/g, ' ').trim()

  /**
   * The interactive control whose text matches — or nothing.
   *
   * Interactive elements only, with no fallback to paragraphs and list
   * items. The fallback seemed harmless and was not: asked to find "Add
   * record" on Cloudflare's docs, it highlighted the sentence *"2. Select
   * Add record."* — a numbered instruction in the prose, not a button. The
   * real control on that page is "Go to Records".
   *
   * Highlighting prose is worse than highlighting nothing. It tells someone
   * to click a line of text, they click it, nothing happens, and they
   * conclude the tool is broken — or worse, that they are.
   *
   * So: if there is no button, link or field with this text, say so and
   * highlight nothing.
   */
  const CLICKABLE = [
    'a[href]', 'button', '[role=button]', '[role=link]', '[role=menuitem]',
    '[role=tab]', 'input[type=submit]', 'input[type=button]', 'summary',
    'label', 'select', '[onclick]',
  ].join(',')

  function findTarget(needle) {
    const want = needle.replace(/\s+/g, ' ').trim().toLowerCase()
    if (!want) return null

    const controls = [...document.querySelectorAll(CLICKABLE)].filter(isVisible)

    // Exact first: "Save" should not match "Save and close" while a plain
    // "Save" button exists on the page.
    const exact = controls.find((el) => textOf(el).toLowerCase() === want)
    if (exact) return exact

    // Then the tightest containing match, so a nav wrapper never wins over
    // the link inside it.
    const partial = controls
      .filter((el) => textOf(el).toLowerCase().includes(want))
      .sort((a, b) => textOf(a).length - textOf(b).length)
    return partial[0] || null
  }

  function isVisible(el) {
    const r = el.getBoundingClientRect()
    if (r.width < 4 || r.height < 4) return false
    const s = getComputedStyle(el)
    return s.visibility !== 'hidden' && s.display !== 'none' && s.opacity !== '0'
  }

  function ensureOverlay() {
    if (overlay) return overlay
    const host = document.createElement('div')
    host.id = HOST_ID
    // Fixed and non-interactive so it never eats the click it is pointing at.
    host.style.cssText = 'position:fixed;inset:0;z-index:2147483647;pointer-events:none'
    document.documentElement.appendChild(host)

    const root = host.attachShadow({ mode: 'open' })
    root.innerHTML = `
      <style>
        /*
         * A highlighter pen, not a spotlight.
         *
         * The first version dimmed the whole page behind a 35% black scrim to
         * make the target pop. It did — and it also made the site look
         * broken, hid everything the person needed for context, and turned a
         * hint into an interruption. Someone following a walkthrough is still
         * reading the page around the button.
         *
         * mix-blend-mode multiply is what makes this read as ink on the
         * page rather than a box floating above it: dark text stays legible
         * straight through the yellow.
         */
        .mark {
          position: fixed;
          background: rgba(255, 214, 0, .45);
          border: 2px solid #ffc400;
          border-radius: 4px;
          mix-blend-mode: multiply;
          transition: top .15s ease, left .15s ease, width .15s ease, height .15s ease;
        }
      </style>
      <div class="mark" hidden></div>`

    overlay = { host, mark: root.querySelector('.mark') }
    return overlay
  }

  function spotlight(el) {
    const o = ensureOverlay()
    el.scrollIntoView({ block: 'center', behavior: 'smooth' })

    // Re-measured after the scroll settles, and again on scroll and resize,
    // or the mark sits where the element used to be.
    const place = () => {
      const r = el.getBoundingClientRect()
      o.mark.hidden = false
      o.mark.style.top = `${r.top - 2}px`
      o.mark.style.left = `${r.left - 2}px`
      o.mark.style.width = `${r.width + 4}px`
      o.mark.style.height = `${r.height + 4}px`
    }
    setTimeout(place, 350)
    addEventListener('scroll', place, { passive: true })
    addEventListener('resize', place, { passive: true })

    // Advancing on click, not on a timer: the user sets the pace, and a step
    // that was never clicked has not been completed.
    el.addEventListener('click', () => {
      chrome.runtime.sendMessage({ type: 'advance' })
      clear()
    }, { once: true })
  }

  function clear() {
    if (!overlay) return
    overlay.mark.hidden = true
  }

  /** Visible text of every control on the page, for the model to choose from. */
  function allControls() {
    return [...document.querySelectorAll(CLICKABLE)]
      .filter(isVisible)
      .map(textOf)
      .filter((t) => t && t.length <= 60)
  }

  async function tick() {
    const res = await chrome.runtime.sendMessage({ type: 'whatNow', url: location.href })
      .catch(() => null)
    if (!res?.step) return clear()

    // Literal match first — free, instant, and right most of the time.
    let el = findTarget(res.step.find)

    // Then ask the model, because help pages and products use different
    // words. Cloudflare's docs say "log in"; the button says "Sign in".
    // Without this the walkthrough goes quiet exactly when it is needed.
    if (!el) {
      const label = await chrome.runtime
        .sendMessage({ type: 'match', step: res.step, controls: allControls() })
        .catch(() => null)
      if (label?.match) el = findTarget(label.match)
    }

    if (el) spotlight(el)
    else clear() // Genuinely not on this page — stay silent rather than guess.
  }

  /**
   * The search engine asks for a walkthrough.
   *
   * A page cannot call `chrome.runtime` — content scripts run in an isolated
   * world precisely so that websites cannot drive extensions. So the search
   * frontend posts a message to its own window, this script (which *is*
   * running in that page) hears it, and forwards it to the worker. That is
   * the standard bridge, and it keeps the entry point where the user already
   * is: the search box, not a toolbar popup nobody thinks to open.
   *
   * Origin is checked, not assumed. `<all_urls>` means this listener is live
   * on every site on the web, and without the check any page could start a
   * guide — navigating the tab wherever it liked.
   */
  const GUIDE_ORIGIN = 'http://localhost:8091'

  addEventListener('message', (event) => {
    if (event.origin !== GUIDE_ORIGIN) return
    if (event.source !== window) return
    const msg = event.data
    if (msg?.type !== 'RSEARCH_START_GUIDE' || typeof msg.query !== 'string') return

    chrome.runtime.sendMessage({ type: 'start', query: msg.query })
      .then((res) => {
        // Reported back so the page can say "no walkthrough for that" rather
        // than appearing to do nothing.
        window.postMessage(
          { type: 'RSEARCH_GUIDE_STARTED', steps: res?.steps ?? 0, reason: res?.reason },
          GUIDE_ORIGIN,
        )
      })
      .catch(() => {
        window.postMessage({ type: 'RSEARCH_GUIDE_STARTED', steps: 0, reason: 'extension not reachable' }, GUIDE_ORIGIN)
      })
  })

  // Tell the page an extension is present, so it can offer the button at all.
  if (location.origin === GUIDE_ORIGIN) {
    document.documentElement.dataset.rsearchGuide = '1'
  }

  // Run on load, and again as the page settles — single-page apps rewrite
  // their content after the initial load event, so one attempt finds nothing.
  tick()
  setTimeout(tick, 1200)
  setTimeout(tick, 3000)

  // SPA route changes fire no navigation event the content script can see.
  let lastUrl = location.href
  new MutationObserver(() => {
    if (location.href !== lastUrl) {
      lastUrl = location.href
      setTimeout(tick, 800)
    }
  }).observe(document, { subtree: true, childList: true })
})()
