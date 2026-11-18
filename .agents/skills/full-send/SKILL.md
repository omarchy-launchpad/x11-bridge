---
name: full-send
description: Review all pending repository changes, organize them into sensible commits, check for sensitive material, and push to main when explicitly requested.
---

# Full send

Use this workflow when the user invokes `/full-send` or asks to commit and push all pending work to main. Invocation authorizes committing the pending work and pushing it to `origin/main`.

1. Inspect repository instructions, working tree, staged and unstaged changes, untracked files, current history, and remote configuration. In a colocated Jujutsu repository, use `jj` to inspect and commit the working-copy change; Git's detached HEAD and unstaged view are normal there. Account for every pending change, including new files and changes already staged.
2. Inspect the actual content destined for every commit for credentials, private keys, tokens, personal data, and other material that appears sensitive. Check file names and contents, including untracked files and staged versions. Treat examples and placeholders in context. Never print secret values. If sensitive material is found or cannot be safely assessed, stop before committing or pushing, clearly warn the user with file paths and types of concern, and ask how to proceed. Do not silently omit it.
3. Group related changes into coherent commits in dependency order. Keep inseparable changes together; use one commit when splitting would be artificial. Preserve all user changes. Run appropriate checks and report failures; stop if a relevant check fails or cannot establish safe push readiness.
4. Fetch `origin` and verify the intended history extends current `origin/main`. Commit the reviewed changes with descriptive messages, move the `main` bookmark when using `jj`, and push to `origin/main`. Do not force push, overwrite remote work, or resolve unexpected divergence by rewriting shared history. Stop and explain any conflict, rejection, or ambiguity that prevents a safe fast-forward.
5. Verify remote `main` points to the intended final commit. Report commits, checks, skipped checks, and push result. If no changes exist, report whether local and remote main match.
