# GitHub PR request coverage by the chd cache

Observed on `crcl-main/events-trigger#185` with authenticated read-only `gh api -i`.
A conditional re-request with the observed ETag returned `304` for every REST row below.
A `304` validates only that request's representation. Every mutable row is revalidated
during each open operation; tuicr never reuses rows based on cache age or an unchanged head.

| UI data | Source | Validation | Status |
| --- | --- | --- | --- |
| PR identity (head, base) | `GET pulls/N` | ETag | bundle |
| Changed files | `GET pulls/N/files` | per-page ETag | bundle |
| Reviews (viewer commit markers, summaries) | `GET pulls/N/reviews` | per-page ETag | bundle |
| Merge base, commits, diff, blobs | git / compare API | immutable OID | bundle |
| Check runs | `GET commits/H/check-runs?filter=latest` | per-page ETag, keyed by head | bundle |
| Legacy status contexts | `GET commits/H/status` | per-page ETag, keyed by head | bundle |
| Conversation comments | `GET issues/N/comments` | per-page ETag | bundle |
| Requested reviewers | `GET pulls/N/requested_reviewers` | ETag | bundle |
| Viewer login | `GET user` | ETag per credential | bundle |
| `reviewDecision`, `mergeable`, `mergeStateStatus` | GraphQL via `gh pr view` | none (no validator) | live residual |
| `latestReviews` | GraphQL via `gh pr view` | none | live residual |
| Review threads, reactions | GraphQL | none | live |

## Field equivalence

- **Check rollup.** On PR 185 the 23 REST check runs (`filter=latest`) equal the GraphQL
  `CheckRun` rows by name, status, conclusion and URL once REST enum values are upper-cased,
  and the 3 status contexts match. Both paths sort checks by name, status and conclusion, and
  requested reviewers by label, so cached and live opens render identically.
- **Comments.** GraphQL spells bot logins without the REST `[bot]` suffix, so tuicr strips `[bot]` from REST comment authors.
- **Requested reviewers.** Users match by login; teams match by name, then slug.
- **`latestReviews`.** REST cannot reproduce it: on PR 185 GraphQL omitted a `COMMENTED`-only
  reviewer that REST lists, and excludes pending reviews. It stays in the live residual query.
- **`reviewDecision` and merge state.** These depend on branch-protection rules that approvals
  alone do not reproduce, so they stay live.

## Fallback

If any part that the panel needs is missing, unvalidated, or bound to a different head or base,
one full live `gh pr view` query supplies the info, started as soon as the bundle returns. A
part is consumed once per open.
