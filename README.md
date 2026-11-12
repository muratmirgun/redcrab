# redcrab.muratmirgun.me

The website: plain static pages, no build. Lives on this branch so that changes to it
never run main's CI or release workflows.

- `index.html`: the home page
- `privacy/`: the privacy policy (App Store Connect links to it)
- `support/`: support (App Store Connect links to it)
- `images/`: copies of `docs/images` from main; copy them again after new screenshots

Preview: `python3 -m http.server` here, then http://localhost:8000.
