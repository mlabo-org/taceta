# Taceta Link

Taceta Link is the private, local browser extension boundary for Taceta. It
uses Manifest V3 and Native Messaging (`org.mlabo.taceta.link`) to operate only
on a window and tab created by the extension for the current session.

The typed workflows are `google_search`, `default_search` (the browser's
configured default provider through the official `chrome.search` API), and
`chatgpt_web`. The latter passes the prompt exactly as entered; it does not
read cookies, tokens, profiles, local storage, or CAPTCHA pages.

Load this directory as an unpacked extension after installing the native host
manifest from `native-host-manifest.template.json`. `VERSION` must match the
Taceta Cargo package version. Validate with:

```sh
node browser-extension/validate.mjs
node --test browser-extension/extension.test.mjs
```

MIT License.
The generated fixed Chromium extension ID is
`hefhkgbiiajifedgjlbiklclooifkidg`; the Native Messaging allowed origin must
match this value.
