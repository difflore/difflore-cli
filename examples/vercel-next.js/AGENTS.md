<!-- difflore:start -->
## Review rules

Recurring expectations from this repository's code review history, found by [difflore](https://github.com/difflore/difflore-cli) in 196 maintainer comments across 58 pull requests. Each rule links to the reviews it came from.

- **Use test-harness helpers like `next.fetch`/`next.distDir`, not raw fetch, spawns, or paths** E2E harness already provides server fetch, dist dir, next-bin utilities Applies to `test/**`. ([#99191](https://github.com/vercel/next.js/pull/99191#discussion_r4132903933), [#99191](https://github.com/vercel/next.js/pull/99191#discussion_r4132897487), [#99100](https://github.com/vercel/next.js/pull/99100#discussion_r4095061770))
- **Exercise features in both `next dev` and `next build`/`next start` e2e tests** E2E cases must cover dev and production modes, not one Applies to `test/**`. ([#97956](https://github.com/vercel/next.js/pull/97956#discussion_r4102960987), [#97956](https://github.com/vercel/next.js/pull/97956#discussion_r4102942569), [#98734](https://github.com/vercel/next.js/pull/98734#discussion_r4071066685))
- **Prefer keyed lookups over `#[turbo_tasks::function]` for per-chunk-group accessors** turbo_tasks functions create a task per chunk group Applies to `turbopack/crates/turbopack-core/src/module_graph/**`. ([#98733](https://github.com/vercel/next.js/pull/98733#discussion_r4147017809), [#99050](https://github.com/vercel/next.js/pull/99050#discussion_r4079325574))
- **Gate prerender render-stage access via `finalStage`, not stored stage fields** Past bugs let stages resolve in shell prerenders lacking stages Applies to `packages/next/src/server/**`. ([#98034](https://github.com/vercel/next.js/pull/98034#discussion_r4147087738), [#96076](https://github.com/vercel/next.js/pull/96076#discussion_r4083788260))
<!-- difflore:end -->
