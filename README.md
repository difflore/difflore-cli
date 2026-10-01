# difflore

Every repo has rules nobody wrote down. They live in code review: the same
comment, left by the same maintainers, on pull request after pull request.

difflore reads a GitHub repo's review history and gives those rules back to
you, each one linked to the reviews it came from. Save them as `AGENTS.md`,
`CLAUDE.md` or a Cursor rule, and your coding agent follows them before the
next review has to ask.

```console
$ difflore rust-lang/cargo

difflore  rust-lang/cargo
300 maintainer review comments across 142 pull requests

12 unwritten rules

  1  Commit the failing test with its buggy snapshot first, then the fix.
     The fix commit's snapshot diff then demonstrates the behaviour change.
     seen in 19 PRs
     ↳ https://github.com/rust-lang/cargo/pull/17545#discussion_r4154892995

  2  Split refactorings and unrelated changes into separate atomic commits.
     Cargo merges preserve individual commits; reviewers request C-SPLIT extractions.
     seen in 6 PRs
     ↳ https://github.com/rust-lang/cargo/pull/17296#discussion_r3695853827
  …
```

More results: [cargo](examples/rust-lang-cargo/AGENTS.md),
[React](examples/facebook-react/AGENTS.md),
[Next.js](examples/vercel-next.js/AGENTS.md).

## Install

```sh
npx difflore owner/repo
```

or install the binary:

```sh
curl -LsSf https://github.com/difflore/difflore-cli/releases/latest/download/difflore-cli-installer.sh | sh
cargo install difflore-cli
```

difflore needs two things you probably have already:

- **GitHub access.** It uses `GITHUB_TOKEN`, `GH_TOKEN` or your `gh auth login`
  session. Private repos work with a token that can read them. Public repos
  work without one, within GitHub's 60 requests per hour.
- **An agent CLI to think with.** It runs your own
  [Claude Code](https://docs.anthropic.com/en/docs/claude-code),
  [Codex](https://github.com/openai/codex) or
  [Pi](https://github.com/earendil-works/pi) in non-interactive mode, on your subscription
  or key. Nothing is sent anywhere else.

## Use

```sh
difflore                      # the repo in this directory
difflore vercel/next.js       # any GitHub repo
difflore --write              # add the rules to ./AGENTS.md
difflore --write claude       # or CLAUDE.md
difflore --write cursor       # or .cursor/rules/difflore.mdc
difflore --json               # machine-readable, with all evidence
```

`--write` keeps the rest of your file: the rules live between
`<!-- difflore:start -->` and `<!-- difflore:end -->`, and running it again
replaces only that block.

| Option | Default | |
|---|---|---|
| `--limit` | 300 | most recent maintainer review comments to read |
| `--rules` | 15 | maximum rules to keep |
| `--min-prs` | 2 | a rule must come from at least this many pull requests |
| `--agent` | first of `claude`, `codex`, `pi` | also `DIFFLORE_AGENT` |
| `--model` | the agent's default | also `DIFFLORE_MODEL` |
| `--jobs` | 4 | agent calls at once |
| `--all-reviewers` | off | include reviewers who are not owners, members or collaborators |
| `--refresh` | off | refetch instead of using the 24-hour cache |

## How it works

1. Fetches the newest pull request review comments through the GitHub API.
   It keeps top-level comments from owners, members and collaborators, and
   drops replies, bots and one-word comments.
2. Sends them in batches to your agent CLI, which names the expectations that
   come up in more than one comment and cites the comments by id.
3. Merges the candidates, drops generic advice and anything that appears in fewer than two pull
   requests or cites a comment that does not exist, and ranks the rest by how
   many pull requests they appear in.

Comments are cached in `~/.cache/difflore` for a day.

## License

Apache-2.0
