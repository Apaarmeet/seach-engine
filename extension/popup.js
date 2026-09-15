/** Kicks off a guide in the active tab. */
const out = document.getElementById('out')

document.getElementById('go').addEventListener('click', run)
document.getElementById('q').addEventListener('keydown', (e) => {
  if (e.key === 'Enter') run()
})

async function run() {
  const query = document.getElementById('q').value.trim()
  if (!query) return
  out.textContent = 'Working out the steps…'

  const [tab] = await chrome.tabs.query({ active: true, currentWindow: true })
  const res = await chrome.runtime.sendMessage({ type: 'start', query, tabId: tab.id })

  if (res?.error) out.textContent = `Couldn’t start: ${res.error}`
  else if (!res?.steps) out.textContent = res?.reason || 'No walkthrough found.'
  else out.textContent = `${res.steps} steps — follow the highlights.`
}
