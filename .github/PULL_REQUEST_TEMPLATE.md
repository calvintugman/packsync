## What and why

<!-- What does this change, and why? Link the issue if there is one. -->

## Could this lose or corrupt someone's data?

<!-- Think about: interruption (power cut, killed process), two computers, hostile or damaged cloud data,
     "unknown" being treated as "missing". If the honest answer is "no, because …", write the because. -->

## How was it tested?

- [ ] `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`
- [ ] UI: `npm run typecheck`, `npm run lint`, `npm test` (and `npm run e2e` for user-visible flows)
- [ ] New or changed behaviour has a test that fails without the change
- [ ] Touches restore/upload/snapshot/sessions: a failpoint/crash test covers the new boundary
- [ ] Reads cloud or disk data: a hostile-data test covers it

Platform(s) I ran this on: <!-- Linux / Windows -->

## Not verified

<!-- Be honest: a platform, a real Google account, a real Prism… -->

## Docs

- [ ] The README is still accurate (or is updated here)

## Checklist

- [ ] No credentials, tokens, `.env`, mod JARs, Minecraft files or real world data in the diff
- [ ] No new dependency — or it is explained above and passes `cargo deny` / `npm audit`
