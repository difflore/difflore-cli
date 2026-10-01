use std::collections::BTreeSet;
use std::fmt::Write as _;

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

use crate::agent::Agent;
use crate::github::{Comment, Repo};

const BATCH_CHARS: usize = 60_000;

const SYSTEM: &str = "You read code review comments and extract the recurring, \
project-specific rules that reviewers enforce. You answer with a JSON array only, \
no prose and no code fences.";

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Draft {
    rule: String,
    #[serde(default)]
    why: String,
    #[serde(default)]
    paths: String,
    #[serde(default)]
    evidence: Vec<usize>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Rule {
    pub rule: String,
    pub why: String,
    pub paths: String,
    pub prs: usize,
    pub evidence: Vec<Evidence>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Evidence {
    pub pr: u64,
    pub author: String,
    pub url: String,
    pub quote: String,
}

pub struct Options {
    pub max_rules: usize,
    pub min_prs: usize,
    pub jobs: usize,
}

pub fn distill(
    repo: &Repo,
    comments: &[Comment],
    agent: &Agent,
    opts: &Options,
    progress: &(dyn Fn(&str) + Sync),
) -> Result<Vec<Rule>> {
    let batches = batch(comments);
    let total = batches.len();
    let mut drafts = Vec::new();
    let mut failures = 0usize;
    let mut done = 0usize;

    for wave in batches.chunks(opts.jobs.max(1)) {
        let results: Vec<Result<Vec<Draft>>> = std::thread::scope(|s| {
            let handles: Vec<_> = wave
                .iter()
                .map(|b| s.spawn(|| ask(agent, &map_prompt(repo, comments, b))))
                .collect();
            handles
                .into_iter()
                .map(|h| h.join().unwrap_or_else(|_| bail!("worker panicked")))
                .collect()
        });
        for r in results {
            match r {
                Ok(d) => drafts.extend(d),
                Err(e) => {
                    failures += 1;
                    progress(&format!("A batch failed and was skipped: {e:#}"));
                }
            }
        }
        done += wave.len();
        progress(&format!(
            "Read {done} of {total} batches, {} candidate rules so far",
            drafts.len()
        ));
    }
    if failures == total {
        bail!(
            "every batch failed; check that `{}` works on its own",
            agent.label()
        );
    }

    let merged = if drafts.is_empty() {
        drafts
    } else {
        progress(&format!("Merging {} candidates", drafts.len()));
        ask(agent, &reduce_prompt(repo, &drafts, opts.max_rules))?
    };

    Ok(finalize(merged, comments, opts))
}

fn ask(agent: &Agent, prompt: &str) -> Result<Vec<Draft>> {
    let mut last_err = None;
    for _ in 0..2 {
        match agent
            .complete(SYSTEM, prompt)
            .and_then(|out| parse_drafts(&out))
        {
            Ok(d) => return Ok(d),
            Err(e) => last_err = Some(e),
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow::anyhow!("no answer")))
}

/// Splits comment indices into batches that fit a single prompt.
fn batch(comments: &[Comment]) -> Vec<Vec<usize>> {
    let mut out = vec![];
    let mut cur = vec![];
    let mut size = 0;
    for (i, c) in comments.iter().enumerate() {
        let len = c.body.len() + c.path.len() + c.line.len() + 24;
        if size + len > BATCH_CHARS && !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
            size = 0;
        }
        cur.push(i);
        size += len;
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

fn map_prompt(repo: &Repo, comments: &[Comment], ids: &[usize]) -> String {
    let mut p = format!(
        "Repository: {repo}\n\n\
Below are {n} code review comments that maintainers wrote on pull requests. Each starts \
with its id in brackets and the file path, followed by the line of code it was attached to \
when available.\n\n\
Find the review expectations that come up repeatedly: conventions, constraints or \
preferences that maintainers ask for in two or more separate comments. Keep only rules that are \
specific to this project: each must name something of this repository (a module, API, file, \
tool, command, test helper, doc page) or a constraint most repositories do not have. If the \
rule would make sense pasted into any repository's AGENTS.md (\"avoid dead code\", \
\"reuse helpers\", \"add tests\", \"keep PRs small\"), leave it out.\n\n\
Skip one-off bug reports, questions, praise, thanks, typo fixes and anything that only makes \
sense for a single line. Each rule states exactly one expectation; never join two with \"and\" or a semicolon.\n\n\
Return a JSON array, nothing else:\n\
[{{\"rule\": \"<one imperative instruction a coding agent can follow, max 16 words>\", \
\"why\": \"<the reason maintainers give, max 14 words>\", \
\"paths\": \"<glob the rule applies to, or *>\", \
\"evidence\": [<ids of the comments that ask for it>]}}]\n\n\
Return [] if nothing recurs.\n\nComments:\n",
        n = ids.len()
    );
    for &i in ids {
        let c = &comments[i];
        let _ = write!(p, "\n[{i}] {}\n{}\n", c.path, c.body);
        if !c.line.trim().is_empty() {
            let _ = writeln!(p, "code: {}", c.line.trim());
        }
    }
    p
}

fn reduce_prompt(repo: &Repo, drafts: &[Draft], max: usize) -> String {
    let json = serde_json::to_string(drafts).unwrap_or_default();
    format!(
        "Repository: {repo}\n\n\
Below are candidate rules extracted from separate batches of this repository's review \
comments, as JSON. Different batches may describe the same expectation in different words.\n\n\
Merge candidates that describe the same expectation and union their evidence ids. Drop \
every rule that would make sense pasted into any repository's AGENTS.md (for example \"avoid \
dead code\", \"reuse existing helpers\", \"add tests\", \"keep PRs focused\"); keep a rule only \
if it names something of this repository or a constraint most repositories do not have. \
Returning fewer rules is better than returning generic ones. Do not fold distinct \
expectations into one rule: each rule states exactly one thing in at most 16 words, and its \
why in at most 14 words. Return at most {max} rules \
that would most help a coding agent working in this repository, the best-evidenced first.\n\n\
Return a JSON array only, same shape: \
[{{\"rule\": \"...\", \"why\": \"...\", \"paths\": \"...\", \"evidence\": [ids]}}]\n\n\
Candidates:\n{json}\n"
    )
}

fn parse_drafts(out: &str) -> Result<Vec<Draft>> {
    let (Some(start), Some(end)) = (out.find('['), out.rfind(']')) else {
        bail!("agent answer had no JSON array");
    };
    if end < start {
        bail!("agent answer had no JSON array");
    }
    Ok(serde_json::from_str(&out[start..=end])?)
}

fn finalize(drafts: Vec<Draft>, comments: &[Comment], opts: &Options) -> Vec<Rule> {
    let mut rules: Vec<Rule> = drafts
        .into_iter()
        .filter_map(|d| {
            let ids: BTreeSet<usize> = d
                .evidence
                .into_iter()
                .filter(|&i| i < comments.len())
                .collect();
            let prs: BTreeSet<u64> = ids.iter().map(|&i| comments[i].pr).collect();
            if prs.len() < opts.min_prs || d.rule.trim().is_empty() {
                return None;
            }
            let evidence = ids
                .iter()
                .map(|&i| {
                    let c = &comments[i];
                    Evidence {
                        pr: c.pr,
                        author: c.author.clone(),
                        url: c.url.clone(),
                        quote: c.body.chars().take(200).collect(),
                    }
                })
                .collect();
            Some(Rule {
                rule: d.rule.trim().to_owned(),
                why: d.why.trim().to_owned(),
                paths: match d.paths.trim() {
                    "" | "*" | "**" | "**/*" => String::new(),
                    p => p.to_owned(),
                },
                prs: prs.len(),
                evidence,
            })
        })
        .collect();
    rules.sort_by(|a, b| {
        b.prs
            .cmp(&a.prs)
            .then(b.evidence.len().cmp(&a.evidence.len()))
    });
    rules.dedup_by(|a, b| a.rule.eq_ignore_ascii_case(&b.rule));
    rules.truncate(opts.max_rules);
    rules
}

#[cfg(test)]
mod tests {
    use super::*;

    fn comment(pr: u64) -> Comment {
        Comment {
            pr,
            author: "alice".into(),
            path: "src/lib.rs".into(),
            line: String::new(),
            body: "Use the shared error type here.".into(),
            url: format!("https://github.com/o/r/pull/{pr}#discussion_r1"),
            created_at: String::new(),
        }
    }

    #[test]
    fn parses_json_wrapped_in_prose_or_fences() {
        let out = "Sure:\n```json\n[{\"rule\":\"Do X\",\"evidence\":[1,2]}]\n```";
        let d = parse_drafts(out).expect("parsed");
        assert_eq!(d[0].rule, "Do X");
        assert_eq!(d[0].evidence, vec![1, 2]);
        assert!(parse_drafts("no rules here").is_err());
    }

    #[test]
    fn keeps_rules_seen_in_enough_prs_and_drops_invented_ids() {
        let comments = vec![comment(1), comment(1), comment(2)];
        let drafts = vec![
            Draft {
                rule: "Same PR twice".into(),
                why: String::new(),
                paths: "*".into(),
                evidence: vec![0, 1],
            },
            Draft {
                rule: "Two PRs".into(),
                why: String::new(),
                paths: "src/**".into(),
                evidence: vec![0, 2, 99],
            },
        ];
        let opts = Options {
            max_rules: 10,
            min_prs: 2,
            jobs: 1,
        };
        let rules = finalize(drafts, &comments, &opts);
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].rule, "Two PRs");
        assert_eq!(rules[0].prs, 2);
        assert_eq!(rules[0].evidence.len(), 2);
        assert_eq!(rules[0].paths, "src/**");
    }
}
