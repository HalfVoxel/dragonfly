# Code comment style

1. **Explain why, never what.** Don't restate the code. A comment earns its place only when it carries a fact a reader can't recover from identifiers and types: a race, a contract, a trade-off, a regression.

2. **Lead with the rule, then justify.** Declarative, present tense. State the conclusion as if it were always true. No narration of how you arrived at it.

3. **Label load-bearing facts.** Use `Invariant:`, `Regression:`, `Guarantee:`, `Contract:` as inline headings guards that exist only to prevent a known failure. 'Regression' only allowed on tests.

4. **Tell the counterfactual.** For non-obvious guards, write what breaks if the guard is removed. A reader should be able to predict the breakage from the comment alone.

5. **Be specific.** Reference real event types, error sentinels, functions, and test names. Avoid "some edge case" or "in certain conditions".

6. **Use godoc-style links where the target is a symbol.** Write `[ErrHeadForceReleased]` instead of `ErrHeadForceReleased` so IDE and godoc resolve them. Cross-file pointers should be links, not strings.

7. **Doc comments describe behavior, not signatures.** Open with one line of purpose. Then cover, as needed: when to use, what it does not do, lifecycle and ownership, error semantics, caller obligations. Never paraphrase the signature.

8. **For big blocks, add a scan-helping heading.** Short headings like `Row key format:`, `Allowed transitions:`, `# HITL Example` followed by flat declarative content. Small ASCII diagrams welcome. No prose narrative.

9. **Inline comments stay to one line, two at most.** If a comment grows, prefer a renamed identifier or named constant. Keep length only when the fact lives outside the file and can't be discovered locally.

10. **Present tense only.** No "Now we also", "Updated to", "Previously". Prior state is fair game only when it justifies a current workaround, and then one terse line.

11. **Inside tests, use section markers** ("Phase 1: heavy plan-mode iteration", "Reference: same run, no partial CreditCost") so a reader can skim a long table-driven body.

12. **TODOs are signed and conditional.** `TODO(your-name): when X happens, do Y`. Never a bare TODO, never a date.

13. **What not to write.** Section dividers with no info. Signature-restating godocs. "We used to / considered / now also" narration. Commented-out code. Ticket numbers (those belong in the commit message). Chain of thought reasoning: compress to the one fact that matters.

14. **Go doc newlines.** If a doc comment is longer than 2 lines it should start with 1 sentence describing the item clearly, then a blank line, and then the rest of the docs. To make it easily scannable. The single sentenace should ideally be 1 line, but can extend to 2 lines if necessary.

15. **Accessible summaries.** Function docs should in general be written for a reader who is not familiar with the implementation details. This is particularly important for the first sentance of a godoc, which must be a high level summary, and should not refer to implementation details.

16. **Avoid em-dashes.** Avoid em-dashes in new code and docs. But existing ones can be left to avoid churn.

The test for every comment: could a reader recover this from the code, the identifiers, or a linked doc? If yes, delete it. If no, write the fact flat.

This comment style guide takes precedence over project-specific guides (e.g. Lovable's AGENTS.md), but not user-specific style guides.
